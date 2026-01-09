//! XJP Deploy Agent - 私有云部署代理
//!
//! Supports running as:
//! - Console application (default)
//! - Windows service (with `service` subcommand)
//!
//! Usage:
//! - Normal mode: `xjp-deploy-agent.exe`
//! - With custom port: `xjp-deploy-agent.exe --port 19999`
//! - Canary mode: `xjp-deploy-agent.exe --port 19999 --canary`
//! - Install service: `xjp-deploy-agent.exe service install`
//! - Uninstall service: `xjp-deploy-agent.exe service uninstall`
//! - Run as service: `xjp-deploy-agent.exe service run` (called by SCM)

use xjp_deploy_agent::RuntimeConfig;

/// 解析命令行参数
fn parse_args() -> RuntimeConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut config = RuntimeConfig::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" if i + 1 < args.len() => {
                config.port_override = args[i + 1].parse().ok();
                i += 2;
            }
            "--canary" => {
                config.canary_mode = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "service" => {
                // service 子命令由后面处理
                break;
            }
            _ => {
                i += 1;
            }
        }
    }

    config
}

fn print_help() {
    println!("XJP Deploy Agent - 私有云部署代理");
    println!();
    println!("USAGE:");
    println!("    xjp-deploy-agent [OPTIONS] [COMMAND]");
    println!();
    println!("OPTIONS:");
    println!("    --port <PORT>    Override the listening port");
    println!("    --canary         Run in canary mode (disables auto-update)");
    println!("    -h, --help       Print help information");
    println!();
    println!("COMMANDS:");
    println!("    service          Windows service management");
    println!();
    println!("EXAMPLES:");
    println!("    xjp-deploy-agent                      # Normal mode");
    println!("    xjp-deploy-agent --port 19999         # Custom port");
    println!("    xjp-deploy-agent --port 19999 --canary  # Canary health check");
}

fn main() {
    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();

    // Handle service commands on Windows
    #[cfg(windows)]
    {
        if args.len() >= 2 && args[1] == "service" {
            handle_service_command(&args);
            return;
        }
    }

    // Parse runtime config from command line
    let config = parse_args();

    // Suppress unused variable warning on non-Windows
    #[cfg(not(windows))]
    let _ = &args;

    // Normal console mode - run with tokio runtime
    let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        xjp_deploy_agent::init_and_run_agent_with_config(config).await;
    });
}

/// Handle Windows service commands
#[cfg(windows)]
fn handle_service_command(args: &[String]) {
    use xjp_deploy_agent::services::windows_service;

    if args.len() < 3 {
        println!("Usage: xjp-deploy-agent.exe service <command>");
        println!();
        println!("Commands:");
        println!("  install    Install as Windows service");
        println!("  uninstall  Uninstall the Windows service");
        println!("  start      Start the service");
        println!("  stop       Stop the service");
        println!("  status     Show service status");
        println!("  run        Run as service (called by SCM)");
        return;
    }

    let command = &args[2];
    let result = match command.as_str() {
        "install" => {
            println!("Installing Windows service...");
            windows_service::install_service()
        }
        "uninstall" => {
            println!("Uninstalling Windows service...");
            windows_service::uninstall_service()
        }
        "start" => {
            println!("Starting Windows service...");
            windows_service::start_service()
        }
        "stop" => {
            println!("Stopping Windows service...");
            windows_service::stop_service()
        }
        "status" => {
            match windows_service::query_service_status() {
                Ok(state) => {
                    println!("Service status: {:?}", state);
                    Ok(())
                }
                Err(e) => Err(e)
            }
        }
        "run" => {
            // This is called by the Service Control Manager
            // First, change to the executable directory
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(exe_dir) = exe_path.parent() {
                    let _ = std::env::set_current_dir(exe_dir);
                }
            }
            windows_service::start_service_dispatcher()
        }
        _ => {
            println!("Unknown service command: {}", command);
            return;
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
