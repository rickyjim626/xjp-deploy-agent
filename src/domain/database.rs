//! 数据库管理相关领域模型

use serde::{Deserialize, Serialize};

/// 数据库类型
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    Postgres,
    Mysql,
}

impl std::fmt::Display for DatabaseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DatabaseType::Postgres => write!(f, "postgres"),
            DatabaseType::Mysql => write!(f, "mysql"),
        }
    }
}

/// 数据库连接配置
#[derive(Debug, Clone, Deserialize)]
pub struct DbConnectionConfig {
    /// 数据库类型
    pub db_type: DatabaseType,
    /// Docker 容器名称
    pub container: String,
    /// 数据库名称
    pub database: String,
    /// 用户名
    #[serde(default)]
    pub username: Option<String>,
    /// 密码
    #[serde(default)]
    pub password: Option<String>,
}

/// 列出数据库请求
#[derive(Debug, Deserialize)]
pub struct ListDatabasesRequest {
    /// 数据库类型过滤
    pub db_type: Option<DatabaseType>,
}

/// 数据库信息
#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub name: String,
    pub db_type: DatabaseType,
    pub container: String,
    pub size_mb: Option<f64>,
}

/// 列出数据库响应
#[derive(Debug, Serialize)]
pub struct ListDatabasesResponse {
    pub databases: Vec<DatabaseInfo>,
}

/// 列出表请求
#[derive(Debug, Deserialize)]
pub struct ListTablesRequest {
    pub connection: DbConnectionConfig,
    /// Schema 名称 (PostgreSQL 用，默认 public)
    #[serde(default)]
    pub schema: Option<String>,
}

/// 表信息
#[derive(Debug, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub schema: Option<String>,
    pub row_count: Option<i64>,
    pub size_kb: Option<f64>,
}

/// 列出表响应
#[derive(Debug, Serialize)]
pub struct ListTablesResponse {
    pub tables: Vec<TableInfo>,
    pub database: String,
}

/// 获取表结构请求
#[derive(Debug, Deserialize)]
pub struct GetSchemaRequest {
    pub connection: DbConnectionConfig,
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
}

/// 列信息
#[derive(Debug, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub default_value: Option<String>,
    pub is_primary_key: bool,
}

/// 索引信息
#[derive(Debug, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
}

/// 获取表结构响应
#[derive(Debug, Serialize)]
pub struct GetSchemaResponse {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub row_count: Option<i64>,
}

/// 执行查询请求
#[derive(Debug, Deserialize)]
pub struct DbQueryRequest {
    pub connection: DbConnectionConfig,
    pub sql: String,
    /// 最大返回行数，默认 1000
    #[serde(default = "default_max_rows")]
    pub max_rows: usize,
    /// 查询超时秒数，默认 30
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

fn default_max_rows() -> usize {
    1000
}

fn default_timeout() -> u32 {
    30
}

/// 执行查询响应
#[derive(Debug, Serialize)]
pub struct DbQueryResponse {
    pub success: bool,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 执行命令请求 (INSERT/UPDATE/DELETE)
#[derive(Debug, Deserialize)]
pub struct DbExecuteRequest {
    pub connection: DbConnectionConfig,
    pub sql: String,
    /// 确认危险操作
    #[serde(default)]
    pub confirm_dangerous: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

/// 执行命令响应
#[derive(Debug, Serialize)]
pub struct DbExecuteResponse {
    pub success: bool,
    pub affected_rows: i64,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 导出格式
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Json,
    Sql,
}

impl Default for ExportFormat {
    fn default() -> Self {
        ExportFormat::Csv
    }
}

/// 导出数据请求
#[derive(Debug, Deserialize)]
pub struct DbExportRequest {
    pub connection: DbConnectionConfig,
    /// 表名或 SQL 查询
    pub source: String,
    /// source 是否为 SQL 查询
    #[serde(default)]
    pub is_query: bool,
    #[serde(default)]
    pub format: ExportFormat,
    /// 最大导出行数，默认 100000
    #[serde(default = "default_export_max_rows")]
    pub max_rows: usize,
}

fn default_export_max_rows() -> usize {
    100000
}

/// 导出数据响应
#[derive(Debug, Serialize)]
pub struct DbExportResponse {
    pub success: bool,
    pub format: ExportFormat,
    pub row_count: usize,
    pub data: String,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// SQL 验证结果
#[derive(Debug)]
pub struct SqlValidation {
    pub valid: bool,
    pub is_dangerous: bool,
    pub error: Option<String>,
}

impl SqlValidation {
    pub fn ok() -> Self {
        Self {
            valid: true,
            is_dangerous: false,
            error: None,
        }
    }

    pub fn dangerous() -> Self {
        Self {
            valid: true,
            is_dangerous: true,
            error: None,
        }
    }

    pub fn invalid(error: impl Into<String>) -> Self {
        Self {
            valid: false,
            is_dangerous: false,
            error: Some(error.into()),
        }
    }
}

/// 检查 SQL 是否为危险操作
pub fn is_dangerous_sql(sql: &str) -> bool {
    let sql_upper = sql.to_uppercase();
    sql_upper.contains("DELETE")
        || sql_upper.contains("TRUNCATE")
        || sql_upper.contains("DROP")
        || sql_upper.contains("ALTER")
}

/// 简单验证 SQL 语法
pub fn validate_sql(sql: &str, check_dangerous: bool) -> SqlValidation {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return SqlValidation::invalid("SQL cannot be empty");
    }

    // 简单检查是否有基本的 SQL 结构
    let sql_upper = trimmed.to_uppercase();
    if !sql_upper.starts_with("SELECT")
        && !sql_upper.starts_with("INSERT")
        && !sql_upper.starts_with("UPDATE")
        && !sql_upper.starts_with("DELETE")
        && !sql_upper.starts_with("WITH")
    {
        return SqlValidation::invalid("SQL must start with SELECT, INSERT, UPDATE, DELETE, or WITH");
    }

    if check_dangerous && is_dangerous_sql(sql) {
        return SqlValidation::dangerous();
    }

    SqlValidation::ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_dangerous_sql() {
        assert!(is_dangerous_sql("DELETE FROM users"));
        assert!(is_dangerous_sql("TRUNCATE TABLE users"));
        assert!(is_dangerous_sql("DROP TABLE users"));
        assert!(!is_dangerous_sql("SELECT * FROM users"));
        assert!(!is_dangerous_sql("INSERT INTO users VALUES (1)"));
    }

    #[test]
    fn test_validate_sql() {
        let result = validate_sql("SELECT * FROM users", true);
        assert!(result.valid);
        assert!(!result.is_dangerous);

        let result = validate_sql("DELETE FROM users", true);
        assert!(result.valid);
        assert!(result.is_dangerous);

        let result = validate_sql("", true);
        assert!(!result.valid);
    }
}
