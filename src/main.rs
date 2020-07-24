#![deny(warnings)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use structopt::StructOpt;
use warp::Filter;

use prometheus::core::AtomicU64;
use prometheus::core::GenericGauge;
use prometheus::{Encoder, TextEncoder};

type U64Gauge = GenericGauge<AtomicU64>;
type CounterType = u64;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "timetravel_police",
    about = "A prometheus daemon watching for filesystem rollbacks."
)]
enum Opt {
    #[structopt(name = "run")]
    Run {
        #[structopt(short, long, default_value = "60")]
        nap_duration: u64,

        #[structopt(short, long, default_value = "0.0.0.0:9088")]
        listen_address: SocketAddr,

        #[structopt(parse(from_os_str))]
        file: PathBuf,
    },

    #[cfg(windows)]
    #[structopt(name = "service")]
    Service {
        #[structopt(short, long, default_value = "60")]
        nap_duration: u64,

        #[structopt(short, long, default_value = "0.0.0.0:9088")]
        listen_address: SocketAddr,

        #[structopt(parse(from_os_str))]
        file: PathBuf,
    },

    #[cfg(windows)]
    #[structopt(name = "install")]
    Install { args: Vec<String> },

    #[cfg(windows)]
    #[structopt(name = "uninstall")]
    Uninstall {},
}

fn parse_file_content(file_content: &[u8]) -> Option<CounterType> {
    let decoded = match std::str::from_utf8(file_content) {
        Err(_) => {
            log::warn!("invalid utf-8 content");
            return None;
        }
        Ok(res) => res.trim_end(),
    };

    if let Ok(parsed) = decoded.parse::<CounterType>() {
        Some(parsed)
    } else {
        log::warn!("invalid usize: {}", decoded);
        None
    }
}

async fn load_counter(gauge: &Box<U64Gauge>, filename: &PathBuf) -> CounterType {
    let counter = match tokio::fs::read(filename).await {
        Ok(file_content) => parse_file_content(file_content.as_slice()),
        Err(_) => None,
    };
    let counter = counter.unwrap_or(0);
    gauge.set(counter);
    counter
}

async fn ticker(gauge: Box<U64Gauge>, filename: &PathBuf, nap_duration: u64) -> () {
    loop {
        log::debug!("tick");
        tokio::time::delay_for(Duration::from_secs(nap_duration)).await;

        let mut fs_counter = load_counter(&gauge, filename).await;

        fs_counter += 1;

        match tokio::fs::write(filename, fs_counter.to_string()).await {
            Err(e) => log::warn!("failed to write to {:?}: {}", filename, e),
            Ok(_) => (),
        }
    }
}

async fn prometheus_server() -> Result<impl warp::Reply, Infallible> {
    let mut buffer = Vec::new();
    TextEncoder::new()
        .encode(&prometheus::gather(), &mut buffer)
        .unwrap();
    Ok(Box::new(buffer))
}

async fn run(nap_duration: u64, listen_address: SocketAddr, file: PathBuf) {
    let gauge = Box::new(
        U64Gauge::new(
            "timetravel_police_counter",
            "A counter that may decrease if time travel happens",
        )
        .unwrap(),
    );
    prometheus::default_registry()
        .register(gauge.clone())
        .unwrap();

    load_counter(&gauge, &file).await;

    let routes = warp::any().and_then(prometheus_server);
    let server = warp::serve(routes).run(listen_address);
    tokio::join!(server, ticker(gauge.clone(), &file, nap_duration));
}

#[cfg(not(windows))]
#[tokio::main]
async fn main() {
    env_logger::init();

    match Opt::from_args() {
        Opt::Run {
            nap_duration,
            listen_address,
            file,
        } => run(nap_duration, listen_address, file).await,
    };
}

#[cfg(windows)]
fn main() {
    use std::ffi::OsString;
    use tokio::runtime::Runtime;

    log_panics::init();

    let options = Opt::from_args();
    let mut rt = Runtime::new().unwrap();
    rt.block_on(async {
        match options {
            Opt::Service {
                nap_duration: _,
                listen_address: _,
                file: _,
            } => {
                let _ = winlog::init(service::SERVICE_NAME);
                log::info!("starting the service");
                service::run()
            }
            Opt::Run {
                nap_duration,
                listen_address,
                file,
            } => {
                env_logger::init();
                println!("running...");
                Ok(run(nap_duration, listen_address, file).await)
            }
            Opt::Install { args } => {
                let res = service::install(args.iter().map(OsString::from).collect());
                println!("done installing");
                res
            }
            Opt::Uninstall {} => {
                let res = service::uninstall();
                println!("done uninstalling");
                res
            }
        }
    })
    .unwrap()
}

#[cfg(windows)]
mod service {
    use std::sync::Arc;
    use std::{ffi::OsString, net::SocketAddr, path::PathBuf, time::Duration};
    use structopt::StructOpt;
    use tokio::sync::Notify;

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
        Result,
    };

    use tokio::runtime::Runtime;

    pub const SERVICE_NAME: &str = "timetravel_police";
    pub const SERVICE_DISPLAY_NAME: &str = "Time Travel Police Prometheus Daemon";
    pub const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    pub fn run() -> Result<()> {
        log::info!("registering the service entry point");
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    define_windows_service!(ffi_service_main, my_service_main);

    // Service entry function which is called on background thread by the system with service
    // parameters. There is no stdout or stderr at this point so make sure to configure the log
    // output to file if needed.
    pub fn my_service_main(_args: Vec<OsString>) {
        // the service arguments passed via the handler aren't the same as the command
        // line arguments of the process. we only care about the command line arguments of the
        // process.

        use super::Opt;

        log::info!("parsing options");
        let options = Opt::from_args();

        log::info!("done parsing options");
        let mut rt = Runtime::new().unwrap();

        log::info!("done creating the runtime");
        rt.block_on(async {
            match options {
                Opt::Service {
                    nap_duration,
                    listen_address,
                    file,
                } => {
                    log::info!("running the service...");
                    run_service(nap_duration, listen_address, file).await
                }
                _ => panic!(),
            }
            .unwrap();
        });
    }

    pub async fn run_service(
        nap_duration: u64,
        listen_address: SocketAddr,
        file: PathBuf,
    ) -> Result<()> {
        log::info!("creating the notification channel");
        // Create a channel to be able to poll a stop event from the service worker loop.
        let notify = Arc::new(Notify::new());

        // Define system service event handler that will be receiving service events.
        let notify_sender = notify.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                // Notifies a service to report its current status information to the service
                // control manager. Always return NoError even if not implemented.
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,

                // Handle stop
                ServiceControl::Stop => {
                    notify_sender.notify();
                    ServiceControlHandlerResult::NoError
                }

                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        log::info!("registering the event handler");
        // Register system service event handler.
        // The returned status handle should be used to report service status changes to the system.
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

        log::info!("setting the status");
        // Tell the system that service is running
        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        log::info!("selecting");
        tokio::select! {
            _ = super::run(nap_duration, listen_address, file) => (),
            _ = async { notify.clone().notified().await } => (),
        };

        log::info!("tell windows the service stopped");
        // Tell the system that service has stopped.
        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    pub fn install(args: Vec<OsString>) -> windows_service::Result<()> {
        winlog::register(SERVICE_NAME);

        let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
        let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

        let service_binary_path = ::std::env::current_exe().unwrap();
        let service_info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: service_binary_path,
            launch_arguments: args,
            dependencies: vec![],
            account_name: None, // run as System
            account_password: None,
        };
        let _service = service_manager.create_service(&service_info, ServiceAccess::empty())?;
        Ok(())
    }

    pub fn uninstall() -> windows_service::Result<()> {
        use std::thread;

        winlog::deregister(SERVICE_NAME);

        let manager_access = ServiceManagerAccess::CONNECT;
        let service_manager = ServiceManager::local_computer(None::<&str>, manager_access)?;

        let service_access =
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
        let service = service_manager.open_service(SERVICE_NAME, service_access)?;

        loop {
            let service_status = service.query_status()?;
            if service_status.current_state == ServiceState::Stopped {
                break;
            }

            service.stop()?;
            // Wait for service to stop
            thread::sleep(Duration::from_secs(1));
        }

        service.delete()?;
        Ok(())
    }
}
