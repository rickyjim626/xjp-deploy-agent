//! 数据库管理 API
//!
//! 包含 /db/* 端点

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tracing::error;

use crate::domain::database::{
    is_dangerous_sql, validate_sql, ColumnInfo, DatabaseType, DbConnectionConfig,
    DbExecuteRequest, DbExecuteResponse, DbExportRequest, DbExportResponse, DbQueryRequest,
    DbQueryResponse, ExportFormat, GetSchemaRequest, GetSchemaResponse,
    ListDatabasesRequest, ListDatabasesResponse, ListTablesRequest, ListTablesResponse, TableInfo,
};
use crate::error::{ApiError, ApiResult};
use crate::middleware::RequireApiKey;
use crate::state::AppState;

/// 创建数据库管理路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/db/list", post(db_list_databases))
        .route("/db/tables", post(db_list_tables))
        .route("/db/schema", post(db_get_schema))
        .route("/db/query", post(db_execute_query))
        .route("/db/execute", post(db_execute_command))
        .route("/db/export", post(db_export_data))
}

/// 列出数据库
///
/// POST /db/list
async fn db_list_databases(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(_request): Json<ListDatabasesRequest>,
) -> ApiResult<impl IntoResponse> {
    // TODO: 完整实现移至 services/database
    // 当前返回空列表作为占位
    Ok(Json(ListDatabasesResponse { databases: vec![] }))
}

/// 列出表
///
/// POST /db/tables
async fn db_list_tables(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ListTablesRequest>,
) -> ApiResult<impl IntoResponse> {
    let conn = &request.connection;
    let schema = request.schema.as_deref().unwrap_or("public");

    let (output, _) = execute_db_command(conn, &format!(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = '{}'",
        schema
    )).await?;

    let tables: Vec<TableInfo> = output
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('-') && !line.contains("table_name"))
        .map(|line| TableInfo {
            name: line.trim().to_string(),
            schema: Some(schema.to_string()),
            row_count: None,
            size_kb: None,
        })
        .collect();

    Ok(Json(ListTablesResponse {
        tables,
        database: conn.database.clone(),
    }))
}

/// 获取表结构
///
/// POST /db/schema
async fn db_get_schema(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<GetSchemaRequest>,
) -> ApiResult<impl IntoResponse> {
    let conn = &request.connection;
    let table = &request.table;
    let schema = request.schema.as_deref().unwrap_or("public");

    // 获取列信息
    let sql = match conn.db_type {
        DatabaseType::Postgres => format!(
            "SELECT column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_schema = '{}' AND table_name = '{}'",
            schema, table
        ),
        DatabaseType::Mysql => format!(
            "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT \
             FROM information_schema.columns \
             WHERE table_schema = '{}' AND table_name = '{}'",
            conn.database, table
        ),
    };

    let (output, _) = execute_db_command(conn, &sql).await?;

    let columns: Vec<ColumnInfo> = output
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('-') && !line.contains("column_name"))
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
            if parts.len() >= 4 {
                Some(ColumnInfo {
                    name: parts[0].to_string(),
                    data_type: parts[1].to_string(),
                    is_nullable: parts[2] == "YES",
                    default_value: if parts[3].is_empty() || parts[3] == "NULL" {
                        None
                    } else {
                        Some(parts[3].to_string())
                    },
                    is_primary_key: false, // TODO: 获取主键信息
                })
            } else {
                None
            }
        })
        .collect();

    Ok(Json(GetSchemaResponse {
        table: table.clone(),
        columns,
        indexes: vec![], // TODO: 获取索引信息
        row_count: None,
    }))
}

/// 执行查询 (SELECT)
///
/// POST /db/query
async fn db_execute_query(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<DbQueryRequest>,
) -> ApiResult<impl IntoResponse> {
    // 验证 SQL
    let validation = validate_sql(&request.sql, false);
    if !validation.valid {
        return Err(ApiError::bad_request(
            validation.error.unwrap_or_else(|| "Invalid SQL".to_string()),
        ));
    }

    // 确保是 SELECT 查询
    let sql_upper = request.sql.trim().to_uppercase();
    if !sql_upper.starts_with("SELECT") && !sql_upper.starts_with("WITH") {
        return Err(ApiError::bad_request(
            "Only SELECT queries are allowed. Use /db/execute for other operations.",
        ));
    }

    let start = Instant::now();
    let (output, _) = execute_db_command(&request.connection, &request.sql).await?;
    let execution_time_ms = start.elapsed().as_millis() as u64;

    // 解析输出
    let lines: Vec<&str> = output.lines().collect();
    let mut columns = Vec::new();
    let mut rows = Vec::new();

    if !lines.is_empty() {
        // 第一行是列名
        columns = lines[0]
            .split('|')
            .map(|s| s.trim().to_string())
            .collect();

        // 跳过分隔行
        for line in lines.iter().skip(2) {
            if line.is_empty() || line.starts_with('(') {
                continue;
            }
            let row: Vec<serde_json::Value> = line
                .split('|')
                .map(|s| {
                    let s = s.trim();
                    if s.is_empty() || s == "NULL" {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(s.to_string())
                    }
                })
                .collect();
            if row.len() == columns.len() {
                rows.push(row);
            }
        }
    }

    let row_count = rows.len();
    let warning = if row_count >= request.max_rows {
        Some(format!("Results limited to {} rows", request.max_rows))
    } else {
        None
    };

    Ok(Json(DbQueryResponse {
        success: true,
        columns,
        rows,
        row_count,
        execution_time_ms,
        warning,
    }))
}

/// 执行命令 (INSERT/UPDATE/DELETE)
///
/// POST /db/execute
async fn db_execute_command(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<DbExecuteRequest>,
) -> ApiResult<impl IntoResponse> {
    // 验证 SQL
    let validation = validate_sql(&request.sql, true);
    if !validation.valid {
        return Err(ApiError::bad_request(
            validation.error.unwrap_or_else(|| "Invalid SQL".to_string()),
        ));
    }

    // 检查危险操作
    if is_dangerous_sql(&request.sql) && !request.confirm_dangerous {
        return Ok(Json(DbExecuteResponse {
            success: false,
            affected_rows: 0,
            execution_time_ms: 0,
            warning: Some("Dangerous operation detected".to_string()),
            error: Some("This operation (DELETE/TRUNCATE/DROP/ALTER) requires confirm_dangerous=true".to_string()),
        }));
    }

    let start = Instant::now();
    let result = execute_db_command(&request.connection, &request.sql).await;
    let execution_time_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok((output, _)) => {
            // 尝试解析受影响的行数
            let affected_rows = output
                .lines()
                .find(|line| line.contains("INSERT") || line.contains("UPDATE") || line.contains("DELETE"))
                .and_then(|line| {
                    line.split_whitespace()
                        .last()
                        .and_then(|s| s.parse::<i64>().ok())
                })
                .unwrap_or(0);

            Ok(Json(DbExecuteResponse {
                success: true,
                affected_rows,
                execution_time_ms,
                warning: None,
                error: None,
            }))
        }
        Err(e) => Ok(Json(DbExecuteResponse {
            success: false,
            affected_rows: 0,
            execution_time_ms,
            warning: None,
            error: Some(e.to_string()),
        })),
    }
}

/// 导出数据
///
/// POST /db/export
async fn db_export_data(
    _auth: RequireApiKey,
    State(_state): State<Arc<AppState>>,
    Json(request): Json<DbExportRequest>,
) -> ApiResult<impl IntoResponse> {
    let sql = if request.is_query {
        request.source.clone()
    } else {
        format!("SELECT * FROM {} LIMIT {}", request.source, request.max_rows)
    };

    let start = Instant::now();
    let (output, _) = execute_db_command(&request.connection, &sql).await?;
    let execution_time_ms = start.elapsed().as_millis() as u64;

    // 根据格式转换输出
    let (data, row_count) = match request.format {
        ExportFormat::Csv => {
            let lines: Vec<&str> = output.lines().collect();
            let csv_data = lines
                .iter()
                .filter(|line| !line.is_empty() && !line.starts_with('-'))
                .map(|line| {
                    line.split('|')
                        .map(|s| s.trim())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let count = csv_data.lines().count().saturating_sub(1);
            (csv_data, count)
        }
        ExportFormat::Json => {
            // 简化的 JSON 导出
            (output.clone(), output.lines().count())
        }
        ExportFormat::Sql => {
            // SQL 导出需要更复杂的处理
            (format!("-- SQL export for {}\n{}", request.source, output), output.lines().count())
        }
    };

    Ok(Json(DbExportResponse {
        success: true,
        format: request.format,
        row_count,
        data,
        execution_time_ms,
        warning: None,
    }))
}

/// 执行数据库命令
async fn execute_db_command(
    conn: &DbConnectionConfig,
    sql: &str,
) -> Result<(String, std::process::ExitStatus), ApiError> {
    let (username, password) = get_db_credentials(conn);

    let output = match conn.db_type {
        DatabaseType::Postgres => {
            let mut cmd = Command::new("docker");
            cmd.args(["exec", "-i", &conn.container, "psql"]);
            cmd.args(["-U", &username, "-d", &conn.database, "-c", sql]);

            if let Some(pwd) = password {
                cmd.env("PGPASSWORD", pwd);
            }

            cmd.output().await
        }
        DatabaseType::Mysql => {
            let mut args = vec![
                "exec".to_string(),
                "-i".to_string(),
                conn.container.clone(),
                "mysql".to_string(),
                "-u".to_string(),
                username,
                conn.database.clone(),
                "-e".to_string(),
                sql.to_string(),
            ];

            if let Some(pwd) = password {
                args.insert(6, format!("-p{}", pwd));
            }

            Command::new("docker").args(&args).output().await
        }
    };

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok((
                    String::from_utf8_lossy(&output.stdout).to_string(),
                    output.status,
                ))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                error!(error = %stderr, "Database command failed");
                Err(ApiError::internal(format!("Database error: {}", stderr)))
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to execute database command");
            Err(ApiError::internal(format!(
                "Failed to execute command: {}",
                e
            )))
        }
    }
}

/// 获取数据库凭据
fn get_db_credentials(conn: &DbConnectionConfig) -> (String, Option<String>) {
    let username = conn.username.clone().unwrap_or_else(|| {
        match conn.db_type {
            DatabaseType::Postgres => "postgres".to_string(),
            DatabaseType::Mysql => "root".to_string(),
        }
    });
    (username, conn.password.clone())
}
