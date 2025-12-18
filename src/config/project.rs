//! 项目配置

use serde::Deserialize;
use std::collections::HashMap;
use std::env;

use crate::domain::deploy::DeployType;

/// 项目配置
#[derive(Clone, Debug)]
pub struct ProjectConfig {
    pub work_dir: String,
    pub deploy_type: DeployType,
}

/// 远程 Agent 项目配置（来自部署中心 API）
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteAgentConfig {
    /// Deploy type: "execute_command", "docker_build", "docker_compose"
    #[serde(default = "default_deploy_type")]
    pub deploy_type: String,
    /// Script or command to execute (for execute_command type)
    pub script: Option<String>,
    /// Working directory
    pub work_dir: Option<String>,
    /// Dockerfile path (for docker_build type)
    pub dockerfile: Option<String>,
    /// Docker build context path
    pub build_context: Option<String>,
    /// Docker image name
    pub image: Option<String>,
    /// Docker image tag
    pub tag: Option<String>,
    /// Also push with :latest tag
    #[serde(default)]
    pub push_latest: bool,
    /// Git branch
    pub branch: Option<String>,
    /// Git repository URL
    pub repo_url: Option<String>,
    /// docker-compose.yml path (for docker_compose type)
    pub compose_file: Option<String>,
    /// Service name to restart (for docker_compose type)
    pub services: Option<Vec<String>>,
    /// Environment variables
    pub env: Option<HashMap<String, String>>,
    /// Timeout in seconds
    pub timeout_secs: Option<u32>,
    // Xiaojincut 相关字段
    /// Xiaojincut 任务类型: "import", "export", "process"
    pub xjc_task_type: Option<String>,
    /// Xiaojincut 输入文件路径
    pub xjc_input_path: Option<String>,
    /// Xiaojincut 输出目录
    pub xjc_output_dir: Option<String>,
    /// Xiaojincut AI 提示
    pub xjc_prompt: Option<String>,
    /// Xiaojincut 服务端口
    pub xjc_port: Option<u16>,
}

fn default_deploy_type() -> String {
    "execute_command".to_string()
}

impl RemoteAgentConfig {
    /// 转换为本地 ProjectConfig
    pub fn to_project_config(&self) -> Option<ProjectConfig> {
        let work_dir = self.work_dir.clone()?;

        let deploy_type = match self.deploy_type.as_str() {
            "docker_build" | "docker-build" => {
                let image = self.image.clone()?;
                DeployType::DockerBuild {
                    dockerfile: self.dockerfile.clone(),
                    build_context: self.build_context.clone(),
                    image,
                    tag: self.tag.clone(),
                    push_latest: self.push_latest,
                    branch: self.branch.clone(),
                }
            }
            "docker_compose" | "docker-compose" => {
                let image = self.image.clone()?;
                DeployType::DockerCompose {
                    compose_file: self
                        .compose_file
                        .clone()
                        .unwrap_or_else(|| "docker-compose.yml".to_string()),
                    image,
                    git_repo_dir: Some(work_dir.clone()),
                    service: self.services.as_ref().and_then(|s| s.first().cloned()),
                }
            }
            "xiaojincut" => DeployType::Xiaojincut {
                task_type: self
                    .xjc_task_type
                    .clone()
                    .unwrap_or_else(|| "import".to_string()),
                input_path: self.xjc_input_path.clone(),
                output_dir: self.xjc_output_dir.clone(),
                prompt: self.xjc_prompt.clone(),
                port: self.xjc_port,
            },
            _ => {
                // execute_command / script
                let script = self
                    .script
                    .clone()
                    .unwrap_or_else(|| "./deploy.sh".to_string());
                DeployType::Script { script }
            }
        };

        Some(ProjectConfig { work_dir, deploy_type })
    }
}

/// 从环境变量加载项目配置
pub fn load_projects_from_env() -> HashMap<String, ProjectConfig> {
    let mut projects = HashMap::new();

    // 遗留支持：xiaojinpro-backend
    if let Ok(work_dir) = env::var("BACKEND_WORK_DIR") {
        projects.insert(
            "xiaojinpro-backend".to_string(),
            ProjectConfig {
                work_dir,
                deploy_type: DeployType::Script {
                    script: "./build-and-push-backend.sh".to_string(),
                },
            },
        );
    }

    // 从 PROJECT_*_DIR 环境变量加载
    for (key, value) in env::vars() {
        if key.starts_with("PROJECT_") && key.ends_with("_DIR") {
            let name = key
                .strip_prefix("PROJECT_")
                .and_then(|s| s.strip_suffix("_DIR"))
                .map(|s| s.to_lowercase().replace('_', "-"));

            if let Some(name) = name {
                let project_type = env::var(format!("PROJECT_{}_TYPE", name.to_uppercase().replace('-', "_")))
                    .unwrap_or_else(|_| "script".to_string());

                let deploy_type = match project_type.as_str() {
                    "docker_compose" | "docker-compose" => {
                        let image = env::var(format!(
                            "PROJECT_{}_IMAGE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let compose_file = env::var(format!(
                            "PROJECT_{}_COMPOSE_FILE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .unwrap_or_else(|_| "docker-compose.yml".to_string());
                        let git_repo = env::var(format!(
                            "PROJECT_{}_GIT_REPO",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let service = env::var(format!(
                            "PROJECT_{}_SERVICE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();

                        if let Some(image) = image {
                            DeployType::DockerCompose {
                                compose_file,
                                image,
                                git_repo_dir: git_repo,
                                service,
                            }
                        } else {
                            continue; // 跳过没有 image 的 docker_compose 项目
                        }
                    }
                    "docker_build" | "docker-build" => {
                        let image = env::var(format!(
                            "PROJECT_{}_IMAGE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let dockerfile = env::var(format!(
                            "PROJECT_{}_DOCKERFILE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let build_context = env::var(format!(
                            "PROJECT_{}_BUILD_CONTEXT",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let tag = env::var(format!(
                            "PROJECT_{}_TAG",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let push_latest = env::var(format!(
                            "PROJECT_{}_PUSH_LATEST",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .map(|v| v == "true" || v == "1")
                        .unwrap_or(false);
                        let branch = env::var(format!(
                            "PROJECT_{}_BRANCH",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();

                        if let Some(image) = image {
                            DeployType::DockerBuild {
                                dockerfile,
                                build_context,
                                image,
                                tag,
                                push_latest,
                                branch,
                            }
                        } else {
                            continue;
                        }
                    }
                    "xiaojincut" => {
                        let task_type = env::var(format!(
                            "PROJECT_{}_TASK_TYPE",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .unwrap_or_else(|_| "import".to_string());
                        let input_path = env::var(format!(
                            "PROJECT_{}_INPUT_PATH",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let output_dir = env::var(format!(
                            "PROJECT_{}_OUTPUT_DIR",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let prompt = env::var(format!(
                            "PROJECT_{}_PROMPT",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok();
                        let port = env::var(format!(
                            "PROJECT_{}_PORT",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .ok()
                        .and_then(|v| v.parse().ok());

                        DeployType::Xiaojincut {
                            task_type,
                            input_path,
                            output_dir,
                            prompt,
                            port,
                        }
                    }
                    _ => {
                        // script type
                        let script = env::var(format!(
                            "PROJECT_{}_SCRIPT",
                            name.to_uppercase().replace('-', "_")
                        ))
                        .unwrap_or_else(|_| "./deploy.sh".to_string());
                        DeployType::Script { script }
                    }
                };

                projects.insert(
                    name,
                    ProjectConfig {
                        work_dir: value,
                        deploy_type,
                    },
                );
            }
        }
    }

    projects
}
