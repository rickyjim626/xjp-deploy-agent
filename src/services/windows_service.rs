//! Windows Service Support
//!
//! Allows the deploy agent to run as a Windows service with proper lifecycle management.
//!
//! Usage:
//! - Install service: `xjp-deploy-agent.exe service install`
//! - Uninstall service: `xjp-deploy-agent.exe service uninstall`
//! - Start service: `sc start xjp-deploy-agent` or `net start xjp-deploy-agent`
//! - Stop service: `sc stop xjp-deploy-agent` or `net stop xjp-deploy-agent`

#![cfg(windows)]

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
        ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

/// Service name used in Windows Service Control Manager
pub const SERVICE_NAME: &str = "xjp-deploy-agent";
/// Display name shown in Services panel
pub const SERVICE_DISPLAY_NAME: &str = "XJP Deploy Agent";
/// Service description
pub const SERVICE_DESCRIPTION: &str = "XiaoJinPro Deploy Agent - Manages deployments, tunnels, and auto-updates";

/// Install the service
pub fn install_service() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CREATE_SERVICE,
    )?;

    // Get the path to the current executable
    let service_binary_path = std::env::current_exe()?;

    // Get the directory containing the executable (for working directory)
    let work_dir = service_binary_path
        .parent()
        .ok_or("Cannot determine service directory")?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem account
        account_password: None,
    };

    let service = manager.create_service(&service_info, ServiceAccess::CHANGE_CONFIG)?;

    // Set service description
    service.set_description(SERVICE_DESCRIPTION)?;

    println!("Service '{}' installed successfully!", SERVICE_NAME);
    println!("Binary path: {}", service_binary_path.display());
    println!("Working directory: {}", work_dir.display());
    println!();
    println!("To start the service, run:");
    println!("  sc start {}", SERVICE_NAME);
    println!("  or");
    println!("  net start {}", SERVICE_NAME);

    Ok(())
}

/// Uninstall the service
pub fn uninstall_service() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;

    let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)?;

    // Check if service is running
    let status = service.query_status()?;
    if status.current_state != ServiceState::Stopped {
        println!("Service is running. Stopping...");
        stop_service_internal()?;
        // Wait a moment for the service to stop
        std::thread::sleep(Duration::from_secs(2));
    }

    service.delete()?;
    println!("Service '{}' uninstalled successfully!", SERVICE_NAME);

    Ok(())
}

/// Start the service via SCM
pub fn start_service() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;

    let service = manager.open_service(SERVICE_NAME, ServiceAccess::START)?;
    service.start::<OsString>(&[])?;
    println!("Service '{}' started!", SERVICE_NAME);

    Ok(())
}

/// Stop the service via SCM
pub fn stop_service() -> Result<(), Box<dyn std::error::Error>> {
    stop_service_internal()?;
    println!("Service '{}' stopped!", SERVICE_NAME);
    Ok(())
}

fn stop_service_internal() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;

    let service = manager.open_service(SERVICE_NAME, ServiceAccess::STOP)?;
    service.stop()?;

    Ok(())
}

/// Query service status
pub fn query_service_status() -> Result<ServiceState, Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;

    let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)?;
    let status = service.query_status()?;

    Ok(status.current_state)
}

/// Check if running as a Windows service
pub fn is_running_as_service() -> bool {
    // Check if we were started with "service run" arguments
    let args: Vec<String> = std::env::args().collect();
    args.len() >= 3 && args[1] == "service" && args[2] == "run"
}

// Note: schedule_service_restart has been moved to services::restart::RestartManager

// Define the Windows service entry point
define_windows_service!(ffi_service_main, service_main);

/// Main entry point when running as a service
fn service_main(arguments: Vec<OsString>) {
    if let Err(e) = run_service(arguments) {
        eprintln!("Service error: {}", e);
    }
}

/// Run the service
fn run_service(_arguments: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    // Create a channel to receive stop events
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    // Define system service event handler
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                // Send shutdown signal
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Register system service event handler
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // Set service status to running
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Change to the directory containing the executable
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let _ = std::env::set_current_dir(exe_dir);
        }
    }

    // Run the actual agent in a separate thread
    let _agent_handle = std::thread::spawn(|| {
        // Create a new tokio runtime for the service
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            crate::init_and_run_agent().await;
        });
    });

    // Wait for stop signal
    let _ = shutdown_rx.recv();

    // Set service status to stop pending
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;

    // TODO: Signal the agent to shutdown gracefully
    // For now, we just exit

    // Set service status to stopped
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

/// Start the service dispatcher (called from main when running as service)
pub fn start_service_dispatcher() -> Result<(), Box<dyn std::error::Error>> {
    // This call will block until the service is stopped
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}
