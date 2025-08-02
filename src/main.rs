use std::{env, process};

use smithay_client_toolkit::reexports::client::globals::{
    self, BindError, GlobalError, GlobalList,
};
use smithay_client_toolkit::reexports::client::{
    ConnectError, Connection, DispatchError, QueueHandle,
};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::wayland::ProtocolStates;
use crate::window::Window;

mod geometry;
mod renderer;
mod wayland;
mod window;

mod gl {
    #![allow(clippy::all, unsafe_op_in_unsafe_fn)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

fn main() {
    // Setup logging.
    let directives = env::var("RUST_LOG").unwrap_or("warn,gorm=info".into());
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    FmtSubscriber::builder().with_env_filter(env_filter).with_line_number(true).init();

    info!("Started Gorm");

    if let Err(err) = run() {
        error!("[CRITICAL] {err}");
        process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    // Initialize Wayland connection.
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = globals::registry_queue_init(&connection)?;
    let mut state = State::new(&connection, &globals, &queue.handle())?;

    // Start event loop.
    while !state.terminated {
        queue.blocking_dispatch(&mut state)?;
    }

    Ok(())
}

/// Application state.
struct State {
    protocol_states: ProtocolStates,

    window: Window,

    terminated: bool,
}

impl State {
    fn new(
        connection: &Connection,
        globals: &GlobalList,
        queue: &QueueHandle<Self>,
    ) -> Result<Self, Error> {
        let protocol_states = ProtocolStates::new(globals, queue)?;

        // Create the Wayland window.
        let window = Window::new(&protocol_states, connection, queue)?;

        Ok(Self { protocol_states, window, terminated: Default::default() })
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Wayland protocol error for {0}: {1}")]
    WaylandProtocol(&'static str, #[source] BindError),
    #[error("{0}")]
    WaylandDispatch(#[from] DispatchError),
    #[error("{0}")]
    WaylandConnect(#[from] ConnectError),
    #[error("{0}")]
    WaylandGlobal(#[from] GlobalError),
    #[error("{0}")]
    Glutin(#[from] glutin::error::Error),
}
