//! 代码浏览相关领域模型

use serde::{Deserialize, Serialize};

/// 代码仓库配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeRepo {
    /// 仓库标识符 (e.g., "backend", "frontend")
    pub name: String,
    /// 仓库根目录路径
    pub root_path: String,
    /// 显示名称
    pub display_name: String,
    /// 允许访问的路径前缀（空表示允许所有）
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// 阻止访问的文件模式（正则表达式）
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
}

/// 文件节点（用于文件树）
#[derive(Clone, Debug, Serialize)]
pub struct FileNode {
    /// 唯一标识符
    pub id: String,
    /// 文件/目录名
    pub name: String,
    /// 相对路径（相对于仓库根目录）
    pub path: String,
    /// 是否为目录
    pub is_directory: bool,
    /// 子节点（仅目录有）
    #[serde(default)]
    pub children: Vec<FileNode>,
    /// 文件大小（字节，仅文件有）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 修改时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// 文件内容响应
#[derive(Clone, Debug, Serialize)]
pub struct FileContent {
    /// 文件路径
    pub path: String,
    /// 文件语言（用于语法高亮）
    pub language: String,
    /// 文件内容
    pub content: String,
    /// 总行数
    pub total_lines: usize,
    /// 起始行（用于部分读取）
    pub start_line: usize,
    /// 结束行
    pub end_line: usize,
}

/// 搜索结果
#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    /// 文件路径
    pub file: String,
    /// 行号
    pub line: usize,
    /// 匹配的内容
    pub content: String,
    /// 匹配开始位置
    pub match_start: usize,
    /// 匹配结束位置
    pub match_end: usize,
}

/// Git 分支信息
#[derive(Clone, Debug, Serialize)]
pub struct GitBranch {
    /// 分支名
    pub name: String,
    /// 最新 commit SHA
    pub sha: String,
    /// 是否为当前分支
    pub current: bool,
}

/// Git 提交信息
#[derive(Clone, Debug, Serialize)]
pub struct GitCommit {
    /// Commit SHA
    pub sha: String,
    /// 提交信息
    pub message: String,
    /// 作者
    pub author: String,
    /// 作者邮箱
    pub email: String,
    /// 提交时间
    pub date: String,
}

/// 代码统计信息
#[derive(Clone, Debug, Serialize)]
pub struct CodeStats {
    /// 文件总数
    pub total_files: usize,
    /// 目录总数
    pub total_directories: usize,
    /// 按语言统计的文件数
    pub languages: std::collections::HashMap<String, usize>,
    /// 扫描时间
    pub scanned_at: String,
}
