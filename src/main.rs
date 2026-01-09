//! XJP Deploy Agent - 私有云部署代理
//!
//! Supports running as:
//! - Console application (default)
//! - Windows service (with `service` subcommand)
//!
//! Usage:
//! - Normal mode: `xjp-deploy-agent.exe`
//! - Install service: `xjp-deploy-agent.exe service install`
//! - Uninstall service: `xjp-deploy-agent.exe service uninstall`
//! - Run as service: `xjp-deploy-agent.exe service run` (called by SCM)

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

    // Suppress unused variable warning on non-Windows
    #[cfg(not(windows))]
    let _ = args;

    // Normal console mode - run with tokio runtime
    let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
    rt.block_on(async {
        xjp_deploy_agent::init_and_run_agent().await;
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
