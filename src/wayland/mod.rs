//! Wayland protocol handling.

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{Transform, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

use crate::geometry::Size;
use crate::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManager};
use crate::wayland::viewporter::Viewporter;
use crate::{Error, State};

pub mod fractional_scale;
pub mod viewporter;

/// Wayland protocol globals.
#[derive(Debug)]
pub struct ProtocolStates {
    pub fractional_scale: Option<FractionalScaleManager>,
    pub compositor: CompositorState,
    pub xdg_shell: XdgShell,
    pub registry: RegistryState,
    pub viewporter: Viewporter,

    output: OutputState,
}

impl ProtocolStates {
    pub fn new(globals: &GlobalList, queue: &QueueHandle<State>) -> Result<Self, Error> {
        let registry = RegistryState::new(globals);
        let output = OutputState::new(globals, queue);
        let xdg_shell = XdgShell::bind(globals, queue)
            .map_err(|err| Error::WaylandProtocol("xdg_shell", err))?;
        let compositor = CompositorState::bind(globals, queue)
            .map_err(|err| Error::WaylandProtocol("wl_compositor", err))?;
        let viewporter = Viewporter::new(globals, queue)
            .map_err(|err| Error::WaylandProtocol("wp_viewporter", err))?;
        let fractional_scale = FractionalScaleManager::new(globals, queue).ok();

        Ok(Self { fractional_scale, compositor, viewporter, xdg_shell, registry, output })
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _surface: &WlSurface,
        factor: i32,
    ) {
        if self.protocol_states.fractional_scale.is_none() {
            self.window.set_scale_factor(factor as f64);
        }
    }

    fn frame(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
        self.window.draw();
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: Transform,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}
delegate_compositor!(State);

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.protocol_states.output
    }

    fn new_output(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }
}
delegate_output!(State);

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.terminated = true;
    }

    fn configure(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let size = configure.new_size.0.zip(configure.new_size.1);
        let size = size.map(|(w, h)| Size::new(w.get(), h.get()));
        self.window.set_size(&self.protocol_states.compositor, size);
    }
}
delegate_xdg_shell!(State);
delegate_xdg_window!(State);

impl FractionalScaleHandler for State {
    fn scale_factor_changed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _surface: &WlSurface,
        factor: f64,
    ) {
        self.window.set_scale_factor(factor);
    }
}

impl ProvidesRegistryState for State {
    registry_handlers![OutputState];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.protocol_states.registry
    }
}
delegate_registry!(State);
