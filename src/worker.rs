use gumdrop::Options;
use nng::*;
use serde::{Deserialize, Serialize};
use std::io::BufWriter;
use std::sync::atomic::Ordering;
use std::{thread, time};
use url::Url;

const EMPTY_ARGS: Vec<&str> = vec![];

use crate::manager::SwanlingUserInitializer;
use crate::metrics::{SwanlingErrorMetrics, SwanlingRequestMetrics, SwanlingTaskMetrics};
use crate::swanling::{SwanlingUser, SwanlingUserCommand};
use crate::{get_worker_id, AttackMode, SwanlingAttack, SwanlingConfiguration, WORKER_ID};

/// Workers send GaggleMetrics to the Manager process to be aggregated together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GaggleMetrics {
    /// Load test hash, used to ensure all Workers are running the same load test.
    WorkerInit(u64),
    /// Swanling request metrics.
    Requests(SwanlingRequestMetrics),
    /// Swanling task metrics.
    Tasks(SwanlingTaskMetrics),
    /// Swanling error metrics.
    Errors(SwanlingErrorMetrics),
}

// If pipe closes unexpectedly, panic.
fn pipe_closed(_pipe: Pipe, event: PipeEvent) {
    if event == PipeEvent::RemovePost {
        panic!("[{}] manager went away, exiting", get_worker_id());
    }
}

// If pipe closes during shutdown, just log it.
fn pipe_closed_during_shutdown(_pipe: Pipe, event: PipeEvent) {
    if event == PipeEvent::RemovePost {
        info!("[{}] manager went away", get_worker_id());
    }
}

// Helper that registers the shutdown pipe handler, avoiding a panic when we
// expect the manager to exit.
pub fn register_shutdown_pipe_handler(manager: &Socket) {
    manager
        .pipe_notify(pipe_closed_during_shutdown)
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("failed to set up new pipe handler");
}

pub(crate) async fn worker_main(swanling_attack: &SwanlingAttack) -> SwanlingAttack {
    // Creates a TCP address.
    let address = format!(
        "tcp://{}:{}",
        swanling_attack.configuration.manager_host, swanling_attack.configuration.manager_port
    );
    info!("worker connecting to manager at {}", &address);

    // Create a request socket.
    let manager = Socket::new(Protocol::Req0)
        .map_err(|error| eprintln!("{:?} address({})", error, address))
        .expect("failed to create socket");

    manager
        .pipe_notify(pipe_closed)
        .map_err(|error| eprintln!("{:?}", error))
        .expect("failed to set up pipe handler");

    // Pause 1/10 of a second in case we're blocking on a cargo lock.
    thread::sleep(time::Duration::from_millis(100));
    // Connect to manager.
    let mut retries = 0;
    loop {
        match manager.dial(&address) {
            Ok(_) => break,
            Err(e) => {
                if retries >= 5 {
                    panic!("failed to communicate with manager at {}: {}.", &address, e);
                }
                debug!("failed to communicate with manager at {}: {}.", &address, e);
                let sleep_duration = time::Duration::from_millis(500);
                debug!(
                    "sleeping {:?} milliseconds waiting for manager...",
                    sleep_duration
                );
                thread::sleep(sleep_duration);
                retries += 1;
            }
        }
    }

    // Send manager the hash of the load test we are ready to run.
    push_metrics_to_manager(
        &manager,
        vec![GaggleMetrics::WorkerInit(swanling_attack.metrics.hash)],
        false,
    );

    let mut config: SwanlingConfiguration = SwanlingConfiguration::parse_args_default(&EMPTY_ARGS)
        .expect("failed to generate default configuration");
    let mut weighted_users: Vec<SwanlingUser> = Vec::new();
    let mut run_time: usize = 0;

    // Wait for the manager to send user parameters.
    info!("waiting for instructions from manager");
    let msg = manager
        .recv()
        .map_err(|error| eprintln!("{:?}", error))
        .expect("error receiving manager message");

    let initializers: Vec<SwanlingUserInitializer> = match serde_cbor::from_reader(msg.as_slice()) {
        Ok(i) => i,
        Err(_) => {
            let command: SwanlingUserCommand = match serde_cbor::from_reader(msg.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    panic!("invalid message received: {}", e);
                }
            };
            match command {
                SwanlingUserCommand::Exit => {
                    panic!("unexpected SwanlingUserCommand::Exit from manager during startup");
                }
                other => {
                    panic!("unknown command from manager: {:?}", other);
                }
            }
        }
    };

    let mut worker_id: usize = 0;
    // Allocate a state for each user that will be spawned.
    info!("initializing user states...");
    for initializer in initializers {
        if worker_id == 0 {
            worker_id = initializer.worker_id;
        }
        let user = SwanlingUser::new(
            initializer.task_sets_index,
            Url::parse(&initializer.base_url).unwrap(),
            initializer.min_wait,
            initializer.max_wait,
            &initializer.config,
            swanling_attack.metrics.hash,
        )
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("failed to create socket");

        // The initializer.config and run_time are the same for all users, only copy it
        // one time.
        if weighted_users.is_empty() {
            config = initializer.config;
            run_time = initializer.run_time;
        }
        weighted_users.push(user);
    }
    WORKER_ID.store(worker_id, Ordering::Relaxed);
    info!(
        "[{}] initialized {} user states",
        get_worker_id(),
        weighted_users.len()
    );

    info!("[{}] waiting for go-ahead from manager", get_worker_id());

    // Wait for the manager to send go-ahead to start the load test.
    loop {
        // Push metrics to manager to force a reply, waiting for SwanlingUserCommand::Run.
        push_metrics_to_manager(
            &manager,
            vec![GaggleMetrics::WorkerInit(swanling_attack.metrics.hash)],
            false,
        );
        let msg = manager
            .recv()
            .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
            .expect("error receiving manager message");

        let command: SwanlingUserCommand = serde_cbor::from_reader(msg.as_slice())
            .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
            .expect("invalid message received");

        match command {
            // Break out of loop and start the load test.
            SwanlingUserCommand::Run => break,
            // Exit worker process immediately.
            SwanlingUserCommand::Exit => {
                warn!(
                    "[{}] received SwanlingUserCommand::Exit command from manager",
                    get_worker_id()
                );
                std::process::exit(0);
            }
            // Sleep and then loop again.
            _ => {
                let sleep_duration = time::Duration::from_secs(1);
                debug!(
                    "[{}] sleeping {:?} second waiting for manager...",
                    get_worker_id(),
                    sleep_duration
                );
                thread::sleep(sleep_duration);
            }
        }
    }

    // Worker is officially starting the load test.
    info!(
        "[{}] entering gaggle mode, starting load test",
        get_worker_id()
    );
    let mut worker_swanling_attack = SwanlingAttack::initialize_with_config(config.clone())
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("failed to launch SwanlingAttack");

    worker_swanling_attack.started = Some(time::Instant::now());
    worker_swanling_attack.task_sets = swanling_attack.task_sets.clone();
    // Use the run_time from the Manager so Worker can shut down in a timely manner.
    worker_swanling_attack.run_time = run_time;
    worker_swanling_attack.weighted_users = weighted_users;
    // This is a Worker instance, not a Manager instance.
    worker_swanling_attack.configuration.manager = false;
    worker_swanling_attack.configuration.worker = true;
    // The request_log option is configured on the Worker.
    worker_swanling_attack.configuration.request_log =
        swanling_attack.configuration.request_log.to_string();
    // The request_format option is configured on the Worker.
    worker_swanling_attack.configuration.request_format =
        swanling_attack.configuration.request_format.clone();
    // The task_log option is configured on the Worker.
    worker_swanling_attack.configuration.task_log =
        swanling_attack.configuration.task_log.to_string();
    // The task_format option is configured on the Worker.
    worker_swanling_attack.configuration.task_format =
        swanling_attack.configuration.task_format.clone();
    // The error_log option is configured on the Worker.
    worker_swanling_attack.configuration.error_log =
        swanling_attack.configuration.error_log.to_string();
    // The error_format option is configured on the Worker.
    worker_swanling_attack.configuration.error_format =
        swanling_attack.configuration.error_format.clone();
    // The debug_log option is configured on the Worker.
    worker_swanling_attack.configuration.debug_log =
        swanling_attack.configuration.debug_log.to_string();
    // The debug_format option is configured on the Worker.
    worker_swanling_attack.configuration.debug_format =
        swanling_attack.configuration.debug_format.clone();
    // The throttle_requests option is set on the Worker.
    worker_swanling_attack.configuration.throttle_requests =
        swanling_attack.configuration.throttle_requests;
    worker_swanling_attack.attack_mode = AttackMode::Worker;
    worker_swanling_attack.defaults = swanling_attack.defaults.clone();

    worker_swanling_attack
        .start_attack(Some(manager))
        .await
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("failed to launch SwanlingAttack")
}

// Push metrics to manager.
pub fn push_metrics_to_manager(
    manager: &Socket,
    metrics: Vec<GaggleMetrics>,
    get_response: bool,
) -> bool {
    debug!("[{}] pushing metrics to manager", get_worker_id(),);
    let mut message = BufWriter::new(Message::new());

    serde_cbor::to_writer(&mut message, &metrics)
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("failed to serialize GaggleMetrics");

    manager
        .try_send(
            message
                .into_inner()
                .expect("failed to extract nng message from buffer"),
        )
        .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
        .expect("communication failure");

    if get_response {
        // Wait for server to reply.
        let msg = manager
            .recv()
            .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
            .expect("error receiving manager message");

        let command: SwanlingUserCommand = serde_cbor::from_reader(msg.as_slice())
            .map_err(|error| eprintln!("{:?} worker_id({})", error, get_worker_id()))
            .expect("invalid message");

        if command == SwanlingUserCommand::Exit {
            info!(
                "[{}] received SwanlingUserCommand::Exit command from manager",
                get_worker_id()
            );
            // Shutting down, register shutdown pipe handler.
            register_shutdown_pipe_handler(manager);
            return false;
        }
    }
    true
}
