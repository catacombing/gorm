//! Wayland window rendering.

use std::collections::HashMap;
use std::mem;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use _text_input::zwp_text_input_v3::{ChangeCause, ContentHint, ContentPurpose, ZwpTextInputV3};
use calloop::{LoopHandle, futures};
use glutin::display::{Display, DisplayApiPreference};
use pangocairo::pango::Alignment;
use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};
use smithay_client_toolkit::compositor::{CompositorState, Region};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client as _text_input;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::{Window as XdgWindow, WindowDecorations};
use tracing::error;

use crate::config::{Config, Input};
use crate::dbus::AccessPoint;
use crate::geometry::{Position, Size, rect_contains};
use crate::renderer::{Renderer, Svg, TextLayout, TextOptions, Texture, TextureBuilder};
use crate::text_field::TextField;
use crate::wayland::ProtocolStates;
use crate::{Error, State, dbus, gl};

/// Logical height of each connection entry at scale 1.
const ENTRY_HEIGHT: u32 = 50;

/// Height for the svg and text buttons at scale 1.
const BUTTON_HEIGHT: u32 = 50;

/// Height of text input fields at scale 1.
const INPUT_HEIGHT: u32 = 40;

/// Padding around all application content at scale 1.
const OUTSIDE_PADDING: f64 = 10.;

/// Vertical padding around buttons at scale 1.
const BUTTON_PADDING: f64 = 20.;

/// Horizontal padding around connection list entries at scale 1.
const ENTRY_X_PADDING: f64 = 6.;

/// Vertical padding between connection list entries at scale 1.
const ENTRY_Y_PADDING: f64 = 2.;

/// Width and height of connection list icons at scale 1.
const ENTRY_ICON_SIZE: f64 = 32.;

/// Horizontal padding around connection list icons at scale 1.
const ENTRY_ICON_PADDING: f64 = 8.;

/// Wayland window.
pub struct Window {
    event_loop: LoopHandle<'static, State>,
    queue: QueueHandle<State>,
    connection: Connection,
    viewport: WpViewport,
    renderer: Renderer,
    xdg: XdgWindow,

    textures: AccessPointTextures,
    disconnect_button: TextButton,
    details: AccessPointDetails,
    connect_button: TextButton,
    forget_button: TextButton,
    password_field: TextField,
    refresh_button: SvgButton,
    toggle_button: SvgButton,
    back_button: SvgButton,
    view: View,

    velocity: ScrollVelocity,
    touch_state: TouchState,
    scroll_offset: f64,

    ime_cause: Option<ChangeCause>,
    text_input: Option<TextInput>,

    initial_configure_done: bool,
    stalled: bool,
    dirty: bool,

    config: Rc<Config>,

    size: Size,
    scale: f64,
}

impl Window {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        protocol_states: &ProtocolStates,
        connection: Connection,
        queue: QueueHandle<State>,
        config: Rc<Config>,
    ) -> Result<Self, Error> {
        // Get EGL display.
        let display = NonNull::new(connection.backend().display_ptr().cast()).unwrap();
        let wayland_display = WaylandDisplayHandle::new(display);
        let raw_display = RawDisplayHandle::Wayland(wayland_display);
        let egl_display = unsafe { Display::new(raw_display, DisplayApiPreference::Egl)? };

        // Create the XDG shell window.
        let decorations = WindowDecorations::RequestServer;
        let surface = protocol_states.compositor.create_surface(&queue);
        let xdg = protocol_states.xdg_shell.create_window(surface, decorations, &queue);
        xdg.set_title("Gorm");
        xdg.set_app_id("Gorm");
        xdg.commit();

        // Create OpenGL renderer.
        let wl_surface = xdg.wl_surface();
        let renderer = Renderer::new(egl_display, wl_surface.clone());

        // Create surface's Wayland global handles.
        if let Some(fractional_scale) = &protocol_states.fractional_scale {
            fractional_scale.fractional_scaling(&queue, wl_surface);
        }
        let viewport = protocol_states.viewporter.viewport(&queue, wl_surface);

        // Default to a reasonable default size.
        let size = Size { width: 360, height: 720 };

        // Initialize UI texture caches.
        let textures = AccessPointTextures::new(config.clone());
        let details = AccessPointDetails::new(config.clone());
        let disconnect_button = TextButton::new(config.clone(), "Disconnect");
        let connect_button = TextButton::new(config.clone(), "Connect");
        let forget_button = TextButton::new(config.clone(), "Forget");
        let refresh_button = SvgButton::new(config.clone(), Svg::Refresh);
        let back_button = SvgButton::new(config.clone(), Svg::ArrowLeft);
        let toggle_button = SvgButton::new_toggle(config.clone(), Svg::Wifi100, Svg::WifiDisabled);
        let mut password_field = TextField::new(config.clone(), event_loop.clone());

        // Setup submit handler for password field.
        let submit_loop = event_loop.clone();
        let _ = password_field.set_submit_handler(Box::new(move |password| {
            let async_loop = submit_loop.clone();
            submit_loop.insert_idle(move |state| {
                let access_point = match &state.window.view {
                    View::Details(access_point) => access_point,
                    View::List => return,
                };

                let path = access_point.path.clone();
                let ssid = access_point.ssid.clone();

                spawn_async(&async_loop, "password connect failed", async move {
                    dbus::connect(path.as_ref(), &ssid, Some(password)).await
                });
            });
        }));

        Ok(Self {
            disconnect_button,
            connect_button,
            password_field,
            refresh_button,
            forget_button,
            toggle_button,
            back_button,
            connection,
            event_loop,
            textures,
            renderer,
            viewport,
            details,
            config,
            queue,
            size,
            xdg,
            stalled: true,
            dirty: true,
            scale: 1.,
            initial_configure_done: Default::default(),
            scroll_offset: Default::default(),
            touch_state: Default::default(),
            text_input: Default::default(),
            ime_cause: Default::default(),
            velocity: Default::default(),
            view: Default::default(),
        })
    }

    /// Check whether UI needs redraw.
    pub fn dirty(&self) -> bool {
        let password_field_dirty = match &self.view {
            View::Details(access_point) if self.password_field.dirty() => {
                access_point.private && access_point.profile.is_none()
            },
            _ => false,
        };

        self.dirty || password_field_dirty || self.velocity.is_moving()
    }

    /// Redraw the window.
    pub fn draw(&mut self) {
        if !self.dirty() {
            self.stalled = true;
            return;
        }
        self.initial_configure_done = true;
        self.dirty = false;

        // Update IME state.
        if self.password_field.take_text_input_dirty() {
            self.update_text_input();
        }

        // Animate scroll velocity.
        self.velocity.apply(&self.config.input, &mut self.scroll_offset);

        // Ensure offset is correct in case tabs were closed or window size changed.
        self.clamp_scroll_offset();

        // Update viewporter logical render size.
        //
        // NOTE: This must be done every time we draw with Sway; it is not
        // persisted when drawing with the same surface multiple times.
        self.viewport.set_destination(self.size.width as i32, self.size.height as i32);

        // Mark entire window as damaged.
        let wl_surface = self.xdg.wl_surface();
        wl_surface.damage(0, 0, self.size.width as i32, self.size.height as i32);

        // Get geometry required for rendering.
        let padding = (OUTSIDE_PADDING * self.scale).round() as f32;
        let toggle_button_pos: Position<f32> = self.toggle_button_position().into();
        let disconnect_button_pos = self.disconnect_button_position().into();
        let mut connect_button_pos = self.connect_button_position().into();
        let password_field_pos = self.password_field_position().into();
        let password_field_size = self.password_field_size();
        let refresh_button_pos = self.refresh_button_position().into();
        let forget_button_pos = self.forget_button_position().into();
        let back_button_pos = self.back_button_position().into();
        let entry_size = self.entry_size();
        let list_end = toggle_button_pos.y - (BUTTON_PADDING * self.scale).round() as f32;

        // Render the window content.
        let physical_size = self.size * self.scale;
        self.renderer.draw(physical_size, |renderer| unsafe {
            // Delete unused WiFi textures.
            self.textures.free_unused_textures();

            // Draw background.
            let [r, g, b] = self.config.colors.background.as_f32();
            gl::ClearColor(r, g, b, 1.);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            match &self.view {
                View::List => {
                    // Scissor crop bottom entry, to not overlap the buttons.
                    gl::Enable(gl::SCISSOR_TEST);
                    gl::Scissor(
                        0,
                        physical_size.height as i32 - list_end as i32,
                        physical_size.width as i32,
                        physical_size.height as i32,
                    );

                    // Draw individual list entries..
                    let mut texture_pos = Position::new(padding, list_end);
                    texture_pos.y += self.scroll_offset as f32;
                    for i in (0..self.textures.access_points.len()).rev() {
                        // Render only AP entries within the viewport.
                        texture_pos.y -= entry_size.height as f32;
                        if texture_pos.y < list_end && texture_pos.y > -(entry_size.height as f32) {
                            let texture = self.textures.texture(i, entry_size.into(), self.scale);
                            renderer.draw_texture_at(texture, texture_pos, None);
                        }

                        // Add padding after the tab.
                        texture_pos.y -= (ENTRY_Y_PADDING * self.scale) as f32
                    }

                    gl::Disable(gl::SCISSOR_TEST);

                    // Draw WiFi state toggle button.
                    let toggle_texture = self.toggle_button.texture();
                    renderer.draw_texture_at(toggle_texture, toggle_button_pos, None);

                    // Draw refresh button.
                    let refresh_texture = self.refresh_button.texture();
                    renderer.draw_texture_at(refresh_texture, refresh_button_pos, None);
                },
                View::Details(access_point) => {
                    // Render AP buttons.
                    if access_point.connected {
                        let forget_texture = self.forget_button.texture();
                        renderer.draw_texture_at(forget_texture, forget_button_pos, None);

                        let disconnect_texture = self.disconnect_button.texture();
                        renderer.draw_texture_at(disconnect_texture, disconnect_button_pos, None);
                    } else {
                        if access_point.profile.is_some() {
                            let forget_texture = self.forget_button.texture();
                            renderer.draw_texture_at(forget_texture, forget_button_pos, None);

                            connect_button_pos = disconnect_button_pos;
                        } else if access_point.private {
                            let password_texture = self.password_field.texture(password_field_size);
                            renderer.draw_texture_at(password_texture, password_field_pos, None);
                        }

                        let connect_texture = self.connect_button.texture();
                        renderer.draw_texture_at(connect_texture, connect_button_pos, None);
                    }

                    // Render AP details.
                    let texture = self.details.texture(access_point);
                    let button_padding = (BUTTON_PADDING * self.scale).round() as f32;
                    let y = if access_point.private && access_point.profile.is_none() {
                        password_field_pos.y - texture.height as f32 - button_padding
                    } else {
                        connect_button_pos.y - texture.height as f32 - button_padding
                    };
                    renderer.draw_texture_at(texture, Position::new(padding, y), None);

                    // Render footer button.
                    let back_texture = self.back_button.texture();
                    renderer.draw_texture_at(back_texture, back_button_pos, None);
                },
            }
        });

        // Request a new frame.
        wl_surface.frame(&self.queue, wl_surface.clone());

        // Apply surface changes.
        wl_surface.commit();
    }

    /// Unstall the renderer.
    ///
    /// This will render a new frame if there currently is no frame request
    /// pending.
    pub fn unstall(&mut self) {
        // Ignore if unstalled or request came from background engine.
        if !mem::take(&mut self.stalled) {
            return;
        }

        // Redraw immediately to unstall rendering.
        self.draw();
        let _ = self.connection.flush();
    }

    /// Update the active WiFi connections.
    pub fn set_access_points(&mut self, access_points: Vec<AccessPoint>) {
        self.textures.access_points = access_points;
        self.dirty = true;
        self.unstall();
    }

    /// Update WiFi toggle status.
    pub fn set_status(&mut self, enabled: bool) {
        if self.toggle_button.enabled != enabled {
            self.toggle_button.set_enabled(enabled);
            self.dirty = true;
            self.unstall();
        }
    }

    /// Update the window's logical size.
    pub fn set_size(&mut self, compositor: &CompositorState, size: Option<Size>) {
        let size = match size {
            Some(size) if size != self.size => size,
            // Use current size to trigger initial draw if no dimensions were provided.
            None if !self.initial_configure_done => self.size,
            Some(_) | None => return,
        };

        self.size = size;
        self.dirty = true;

        // Update the window's opaque region.
        //
        // This is done here since it can only change on resize, but the commit happens
        // atomically on redraw.
        if let Ok(region) = Region::new(compositor) {
            region.add(0, 0, size.width as i32, size.height as i32);
            self.xdg.wl_surface().set_opaque_region(Some(region.wl_region()));
        }

        // Update UI elements.
        self.disconnect_button.set_geometry(self.disconnect_button_size(), self.scale);
        self.connect_button.set_geometry(self.connect_button_size(), self.scale);
        self.refresh_button.set_geometry(self.refresh_button_size(), self.scale);
        self.forget_button.set_geometry(self.forget_button_size(), self.scale);
        self.toggle_button.set_geometry(self.toggle_button_size(), self.scale);
        self.back_button.set_geometry(self.back_button_size(), self.scale);
        self.details.set_geometry(self.max_details_size(), self.scale);
        self.password_field.set_width(self.password_field_size().width as f64);
        self.textures.dirty = true;

        self.unstall();
    }

    /// Update the window's DPI factor.
    pub fn set_scale_factor(&mut self, scale: f64) {
        if self.scale == scale {
            return;
        }

        self.scale = scale;
        self.dirty = true;

        // Update UI elements.
        self.disconnect_button.set_geometry(self.disconnect_button_size(), self.scale);
        self.connect_button.set_geometry(self.connect_button_size(), self.scale);
        self.refresh_button.set_geometry(self.refresh_button_size(), self.scale);
        self.forget_button.set_geometry(self.forget_button_size(), self.scale);
        self.toggle_button.set_geometry(self.toggle_button_size(), self.scale);
        self.back_button.set_geometry(self.back_button_size(), self.scale);
        self.details.set_geometry(self.max_details_size(), self.scale);
        self.password_field.set_scale(self.scale);
        self.textures.dirty = true;

        self.unstall();
    }

    /// Handle config updates.
    pub fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;

        self.disconnect_button.set_config(self.config.clone());
        self.connect_button.set_config(self.config.clone());
        self.password_field.set_config(self.config.clone());
        self.refresh_button.set_config(self.config.clone());
        self.forget_button.set_config(self.config.clone());
        self.toggle_button.set_config(self.config.clone());
        self.back_button.set_config(self.config.clone());
        self.textures.set_config(self.config.clone());
        self.details.set_config(self.config.clone());

        self.unstall();
    }

    /// Handle touch press.
    pub fn touch_down(&mut self, time: u32, logical_position: Position<f64>) {
        // Cancel velocity when a new touch sequence starts.
        self.velocity.set(0.);

        // Convert position to physical space.
        let position = logical_position * self.scale;
        self.touch_state.position = position;
        self.touch_state.start = position;

        // Get button geometries.
        let disconnect_button_position = self.disconnect_button_position();
        let disconnect_button_size = self.disconnect_button_size().into();
        let connect_button_position = self.connect_button_position();
        let connect_button_size = self.connect_button_size().into();
        let password_field_position = self.password_field_position();
        let password_field_size = self.password_field_size().into();
        let refresh_button_position = self.refresh_button_position();
        let refresh_button_size = self.refresh_button_size().into();
        let forget_button_position = self.forget_button_position();
        let forget_button_size = self.forget_button_size().into();
        let toggle_button_position = self.toggle_button_position();
        let toggle_button_size = self.toggle_button_size().into();
        let back_button_position = self.back_button_position();
        let back_button_size = self.back_button_size().into();

        // Check current view state.
        let (details, details_saved, details_connected) = match &self.view {
            View::Details(access_point) => {
                (true, access_point.profile.is_some(), access_point.connected)
            },
            _ => (false, false, false),
        };

        // Handle password field separately, to ensure focus is always updated.
        if details && rect_contains(password_field_position, password_field_size, position) {
            // Forward touch event.
            self.password_field.touch_down(time, position - password_field_position);
            self.password_field.set_focused(true);

            self.touch_state.action = TouchAction::PasswordInput;
            self.ime_cause = Some(ChangeCause::Other);

            self.unstall();

            return;
        } else {
            self.password_field.set_focused(false);
        }

        if details && rect_contains(back_button_position, back_button_size, position) {
            self.touch_state.action = TouchAction::BackTap;
        } else if (details && !details_connected)
            && (rect_contains(connect_button_position, connect_button_size, position)
                || details_saved)
            && (rect_contains(disconnect_button_position, disconnect_button_size, position)
                || !details_saved)
        {
            self.touch_state.action = TouchAction::ConnectTap;
        } else if (details && details_saved)
            && rect_contains(forget_button_position, forget_button_size, position)
        {
            self.touch_state.action = TouchAction::ForgetTap;
        } else if (details && details_connected)
            && rect_contains(disconnect_button_position, disconnect_button_size, position)
        {
            self.touch_state.action = TouchAction::DisconnectTap;
        } else if !details && rect_contains(refresh_button_position, refresh_button_size, position)
        {
            self.touch_state.action = TouchAction::RefreshTap;
        } else if !details && rect_contains(toggle_button_position, toggle_button_size, position) {
            self.touch_state.action = TouchAction::ToggleTap;
        } else if !details && let Some(id) = self.entry_at(position) {
            self.touch_state.action = TouchAction::EntryTap(id);
        } else {
            self.touch_state.action = TouchAction::None;
        }

        // Ensure password focus update is rendered.
        self.unstall();
    }

    /// Handle touch release.
    pub fn touch_motion(&mut self, logical_position: Position<f64>) {
        // Update touch position.
        let position = logical_position * self.scale;
        let old_position = mem::replace(&mut self.touch_state.position, position);

        // Handle transition from entry tap to drag.
        match self.touch_state.action {
            TouchAction::EntryTap(_) | TouchAction::EntryDrag => {
                // Ignore dragging until tap distance limit is exceeded.
                let max_tap_distance = self.config.input.max_tap_distance;
                let delta = self.touch_state.position - self.touch_state.start;
                if delta.x.powi(2) + delta.y.powi(2) <= max_tap_distance {
                    return;
                }
                self.touch_state.action = TouchAction::EntryDrag;

                // Calculate current scroll velocity.
                let delta = self.touch_state.position.y - old_position.y;
                self.velocity.set(delta);

                // Immediately start moving the tabs list.
                let old_offset = self.scroll_offset;
                self.scroll_offset += delta;
                self.clamp_scroll_offset();
                self.dirty |= self.scroll_offset != old_offset;

                self.unstall();
            },
            TouchAction::PasswordInput => {
                let password_field_position = self.password_field_position();
                self.password_field.touch_motion(position - password_field_position);
                self.ime_cause = Some(ChangeCause::Other);
                self.unstall();
            },
            _ => (),
        }
    }

    /// Handle touch release.
    pub fn touch_up(&mut self) {
        match (&self.view, self.touch_state.action) {
            // Connect to a WiFi network.
            (View::Details(access_point), TouchAction::ConnectTap) => {
                let (button_position, button_size) = if access_point.profile.is_some() {
                    (self.disconnect_button_position(), self.disconnect_button_size().into())
                } else {
                    (self.connect_button_position(), self.connect_button_size().into())
                };
                let position = self.touch_state.position;

                if rect_contains(button_position, button_size, position) {
                    let password = self.password_field.text();
                    let profile = (*access_point.profile).clone();
                    let path = access_point.path.clone();
                    let ssid = access_point.ssid.clone();
                    let private = access_point.private;

                    spawn_async(&self.event_loop, "disconnect failed", async move {
                        match profile {
                            Some(profile) => dbus::reconnect(&*path, profile).await,
                            None if private || password.is_empty() => {
                                dbus::connect(&*path, &ssid, None).await
                            },
                            None => dbus::connect(&*path, &ssid, Some(password)).await,
                        }
                    });
                }
            },
            // Disconnect from a WiFi network.
            (View::Details(access_point), TouchAction::DisconnectTap) => {
                let button_position = self.disconnect_button_position();
                let button_size = self.disconnect_button_size().into();
                let position = self.touch_state.position;

                if rect_contains(button_position, button_size, position) {
                    let ssid = access_point.ssid.clone();
                    spawn_async(&self.event_loop, "disconnect failed", async move {
                        dbus::disconnect(&ssid).await
                    });
                }
            },
            // Forget a WiFi network.
            (View::Details(access_point), TouchAction::ForgetTap) => {
                let button_position = self.forget_button_position();
                let button_size = self.forget_button_size().into();
                let position = self.touch_state.position;

                if rect_contains(button_position, button_size, position)
                    && let Some(profile) = (*access_point.profile).clone()
                {
                    spawn_async(&self.event_loop, "disconnect failed", dbus::forget(profile));
                }
            },
            // Go to previous UI page.
            (View::Details(_), TouchAction::BackTap) => {
                let button_position = self.back_button_position();
                let button_size = self.back_button_size().into();
                let position = self.touch_state.position;

                if rect_contains(button_position, button_size, position) {
                    self.view = View::List;
                    self.dirty = true;
                    self.unstall();
                }
            },
            // Handle password input touch release.
            (View::Details(_), TouchAction::PasswordInput) => {
                let input_position = self.password_field_position();
                let input_size = self.password_field_size().into();
                let position = self.touch_state.position;

                if rect_contains(input_position, input_size, position) {
                    self.ime_cause = Some(ChangeCause::Other);
                    self.password_field.touch_up();
                    self.unstall();
                }
            },
            // Toggle WiFi state.
            (View::List, TouchAction::ToggleTap) => {
                let button_position = self.toggle_button_position();
                let button_size = self.toggle_button_size().into();
                let position = self.touch_state.position;
                let enabled = self.toggle_button.enabled;

                if rect_contains(button_position, button_size, position) {
                    spawn_async(
                        &self.event_loop,
                        "state toggle failed",
                        dbus::set_enabled(!enabled),
                    );
                }
            },
            // Refresh WiFi AP list.
            (View::List, TouchAction::RefreshTap) => {
                let button_position = self.refresh_button_position();
                let button_size = self.refresh_button_size().into();
                let position = self.touch_state.position;

                if rect_contains(button_position, button_size, position) {
                    spawn_async(&self.event_loop, "AP refresh failed", dbus::refresh());
                }
            },
            // Open details page for an AP.
            (View::List, TouchAction::EntryTap(index)) => {
                if let Some(access_point) = self.textures.access_points.get(index) {
                    self.view = View::Details(access_point.clone());
                    self.dirty = true;
                    self.unstall();
                }
            },
            _ => (),
        }
    }

    /// Handle keyboard key press.
    pub fn press_key(&mut self, _raw: u32, keysym: Keysym, modifiers: Modifiers) {
        if self.password_field.focused() {
            self.ime_cause = Some(ChangeCause::Other);
            self.password_field.press_key(keysym, modifiers);
            self.unstall();
        }
    }

    /// Paste text into the window.
    pub fn paste(&mut self, text: &str) {
        self.password_field.paste(text);
        self.unstall();
    }

    /// Handle IME focus.
    pub fn text_input_enter(&mut self, text_input: ZwpTextInputV3) {
        self.text_input = Some(text_input.into());
        self.update_text_input();
        self.unstall();
    }

    /// Handle IME focus loss.
    pub fn text_input_leave(&mut self) {
        self.text_input = None;
        self.unstall();
    }

    /// Delete text around the current cursor position.
    pub fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        self.password_field.delete_surrounding_text(before_length, after_length);
        self.unstall();
    }

    /// Insert text at the current cursor position.
    pub fn commit_string(&mut self, text: String) {
        self.password_field.commit_string(&text);
        self.unstall();
    }

    /// Set preedit text at the current cursor position.
    pub fn set_preedit_string(&mut self, text: String, cursor_begin: i32, cursor_end: i32) {
        self.password_field.set_preedit_string(text, cursor_begin, cursor_end);
        self.unstall();
    }

    /// Get the window's Wayland event queue.
    pub fn wayland_queue(&self) -> &QueueHandle<State> {
        &self.queue
    }

    /// Apply pending text input changes.
    fn update_text_input(&mut self) {
        let origin = self.password_field_position();

        let text_input = match &mut self.text_input {
            Some(text_input) => text_input,
            None => return,
        };

        // Disable IME without any input element focused.
        if !self.password_field.focused() {
            text_input.disable();
            return;
        }

        text_input.enable();

        let (text, cursor_start, cursor_end) = self.password_field.surrounding_text();
        text_input.set_surrounding_text(text, cursor_start, cursor_end);

        let cause = self.ime_cause.take().unwrap_or(ChangeCause::InputMethod);
        text_input.set_text_change_cause(cause);

        text_input.set_content_type(ContentHint::SensitiveData, ContentPurpose::Password);

        // Update logical cursor rectangle.
        let (mut position, size) = self.password_field.cursor_rect();
        position += origin;
        text_input.set_cursor_rectangle(position.x, position.y, size.width, size.height);

        text_input.commit();
    }

    /// Physical size of an entry's texture in the AP list.
    fn entry_size(&self) -> Size {
        Size::new(self.size.width - 2 * OUTSIDE_PADDING as u32, ENTRY_HEIGHT) * self.scale
    }

    /// Physical size of the AP details texture.
    fn max_details_size(&self) -> Size {
        let padding = (OUTSIDE_PADDING * self.scale).round() as u32;
        let mut size = self.size * self.scale;
        size.height -= self.disconnect_button_size().height + 2 * padding;
        size.width -= 2 * padding;
        size
    }

    /// Physical size of the "<-" button.
    fn back_button_size(&self) -> Size {
        Size::new(BUTTON_HEIGHT, BUTTON_HEIGHT) * self.scale
    }

    /// Physical position of the "<-" button.
    fn back_button_position(&self) -> Position<f64> {
        let padding = (OUTSIDE_PADDING * self.scale).round() as u32;
        let button_size = self.back_button_size();
        let size = self.size * self.scale;

        let x = size.width - padding - button_size.width;
        let y = size.height - padding - button_size.height;

        Position::new(x, y).into()
    }

    /// Physical size of the WiFi toggle button.
    fn toggle_button_size(&self) -> Size {
        self.back_button_size()
    }

    /// Physical position of the WiFi toggle button.
    fn toggle_button_position(&self) -> Position<f64> {
        let padding = (OUTSIDE_PADDING * self.scale).round() as u32;
        let button_size = self.toggle_button_size();
        let size = self.size * self.scale;

        let y = size.height - padding - button_size.height;

        Position::new(padding, y).into()
    }

    /// Physical size of the AP refresh button.
    fn refresh_button_size(&self) -> Size {
        self.back_button_size()
    }

    /// Physical position of the AP refresh button.
    fn refresh_button_position(&self) -> Position<f64> {
        let padding = (OUTSIDE_PADDING * self.scale).round() as u32;
        let button_size = self.refresh_button_size();
        let size = self.size * self.scale;

        let x = size.width - padding - button_size.width;
        let y = size.height - padding - button_size.height;

        Position::new(x, y).into()
    }

    /// Physical size of the "disconnect" button.
    fn disconnect_button_size(&self) -> Size {
        let width = (self.size.width as f64 * 0.4).round() as u32;
        Size::new(width, BUTTON_HEIGHT) * self.scale
    }

    /// Physical position of the "disconnect" button.
    fn disconnect_button_position(&self) -> Position<f64> {
        let back_button_position = self.back_button_position();
        let button_size = self.disconnect_button_size();

        let outside_padding = (OUTSIDE_PADDING * self.scale).round();
        let button_padding = (BUTTON_PADDING * self.scale).round();
        let size = self.size * self.scale;

        let x = (size.width - button_size.width) as f64 - outside_padding;
        let y = back_button_position.y - button_size.height as f64 - button_padding;

        Position::new(x, y)
    }

    /// Physical size of the "connect" button.
    fn connect_button_size(&self) -> Size {
        self.disconnect_button_size()
    }

    /// Physical position of the "connect" button.
    fn connect_button_position(&self) -> Position<f64> {
        let back_button_position = self.back_button_position();
        let button_size = self.connect_button_size();

        let button_padding = (BUTTON_PADDING * self.scale).round();
        let size = self.size * self.scale;

        let x = ((size.width as f64 - button_size.width as f64) / 2.).round();
        let y = back_button_position.y - button_size.height as f64 - button_padding;

        Position::new(x, y)
    }

    /// Physical size of the "forget" button.
    fn forget_button_size(&self) -> Size {
        self.disconnect_button_size()
    }

    /// Physical position of the "forget" button.
    fn forget_button_position(&self) -> Position<f64> {
        let mut position = self.disconnect_button_position();
        position.x = (OUTSIDE_PADDING * self.scale).round();
        position
    }

    /// Physical size of the password input.
    fn password_field_size(&self) -> Size {
        let width = self.size.width - 2 * OUTSIDE_PADDING as u32;
        Size::new(width, INPUT_HEIGHT) * self.scale
    }

    /// Physical position of the password input.
    fn password_field_position(&self) -> Position<f64> {
        let connect_button_position = self.connect_button_position();
        let outside_padding = (OUTSIDE_PADDING * self.scale).round();
        let button_padding = (BUTTON_PADDING * self.scale).round();
        let password_field_size = self.password_field_size();

        let y = connect_button_position.y - password_field_size.height as f64 - button_padding;

        Position::new(outside_padding, y)
    }

    /// Get AP index at the specified location.
    fn entry_at(&self, mut position: Position<f64>) -> Option<usize> {
        let outside_padding = (OUTSIDE_PADDING * self.scale).round();
        let button_padding = (BUTTON_PADDING * self.scale).round();
        let entry_padding = (ENTRY_Y_PADDING * self.scale).round();
        let entries_end_y = self.toggle_button_position().y - button_padding;
        let entries_size_int = self.entry_size();
        let entries_size: Size<f64> = entries_size_int.into();

        // Check if position is beyond AP list or outside of the horizontal boundaries.
        if position.x < outside_padding
            || position.x >= outside_padding + entries_size.width
            || position.y < outside_padding
            || position.y >= entries_end_y
        {
            return None;
        }

        // Apply current scroll offset.
        position.y -= self.scroll_offset;

        // Check if position is inside the separator.
        let bottom_relative = (entries_end_y - position.y).round();
        let relative_y =
            entries_size.height - 1. - (bottom_relative % (entries_size.height + entry_padding));
        if relative_y < 0. {
            return None;
        }

        // Find entry at the specified offset.
        let rindex = (bottom_relative / (entries_size.height + entry_padding).round()) as usize;
        let index = self.textures.access_points.len().saturating_sub(rindex + 1);

        Some(index)
    }

    /// Clamp AP list view viewport offset.
    fn clamp_scroll_offset(&mut self) {
        let old_offset = self.scroll_offset;
        let max_offset = self.max_scroll_offset() as f64;
        self.scroll_offset = self.scroll_offset.clamp(0., max_offset);

        // Cancel velocity after reaching the scroll limit.
        if old_offset != self.scroll_offset {
            self.velocity.set(0.);
            self.dirty = true;
        }
    }

    /// Get maximum AP list scroll offset.
    fn max_scroll_offset(&self) -> usize {
        let button_padding = (BUTTON_PADDING * self.scale).round() as usize;
        let entry_padding = (ENTRY_Y_PADDING * self.scale).round() as usize;
        let outside_padding = (OUTSIDE_PADDING * self.scale).round() as usize;
        let toggle_button_position = self.toggle_button_position();
        let entry_height = self.entry_size().height;

        // Calculate height available for AP entries.
        let available_height = toggle_button_position.y as usize - button_padding - outside_padding;

        // Calculate height of all AP entries.
        let entry_count = self.textures.access_points.len();
        let entry_height =
            (entry_count * (entry_height as usize + entry_padding)).saturating_sub(entry_padding);

        // Calculate list content outside the viewport.
        entry_height.saturating_sub(available_height)
    }
}

/// Active UI view.
#[derive(Default)]
enum View {
    /// WiFi AP overview.
    #[default]
    List,
    /// WiFi AP information and management.
    Details(AccessPoint),
}

/// Texture cache for available network connections.
struct AccessPointTextures {
    textures: HashMap<AccessPointKey, Texture>,
    access_points: Vec<AccessPoint>,
    name_layout: TextLayout,
    sub_layout: TextLayout,
    config: Rc<Config>,
    dirty: bool,
}

impl AccessPointTextures {
    fn new(config: Rc<Config>) -> Self {
        let font_family = config.font.family.clone();
        let name_layout = TextLayout::new(font_family.clone(), config.font.size(1.), 1.);
        let sub_layout = TextLayout::new(font_family, config.font.size(0.75), 1.);

        Self {
            name_layout,
            sub_layout,
            config,
            access_points: Default::default(),
            textures: Default::default(),
            dirty: Default::default(),
        }
    }

    /// Render the texture for an available AP.
    ///
    /// This will automatically take care of caching rendered textures.
    fn texture(&mut self, index: usize, texture_size: Size<i32>, scale: f64) -> &Texture {
        let access_point = &self.access_points[index];
        self.textures.entry(AccessPointKey::new(access_point)).or_insert_with(|| {
            // Ensure layouts' scale and font are up to date.
            let font_family = &self.config.font.family;
            self.name_layout.set_font(font_family, self.config.font.size(1.));
            self.name_layout.set_scale(scale);
            self.sub_layout.set_font(font_family, self.config.font.size(0.75));
            self.sub_layout.set_scale(scale);

            // Initialize as opaque texture.
            let builder = TextureBuilder::new(&self.config, texture_size);
            builder.clear(self.config.colors.alt_background.as_f64());

            let x_padding = (ENTRY_X_PADDING * scale).round();
            let width = texture_size.width - 2 * x_padding as i32;

            // Render connection strength SVG.
            let svg = match access_point.strength {
                88.. => Svg::Wifi100,
                63.. => Svg::Wifi75,
                38.. => Svg::Wifi50,
                13.. => Svg::Wifi25,
                _ => Svg::Wifi0,
            };
            let icon_padding = (ENTRY_ICON_PADDING * scale).round();
            let icon_size = (ENTRY_ICON_SIZE * scale).round();
            let strength_x = x_padding + icon_padding;
            let icon_y = (texture_size.height as f64 - icon_size) / 2.;
            builder.rasterize_svg(svg, strength_x, icon_y, icon_size, icon_size);

            // Render accessibility SVG.
            let svg = if access_point.private { Svg::Private } else { Svg::Public };
            let pub_x = texture_size.width as f64 - x_padding - icon_padding - icon_size;
            builder.rasterize_svg(svg, pub_x, icon_y, icon_size, icon_size);

            // Calculate text constraints.
            let name_height = self.name_layout.line_height();
            let sub_height = self.sub_layout.line_height();
            let y_padding = ((texture_size.height - name_height - sub_height) / 2) as f64;
            let text_width = width - 2 * icon_size as i32 - 4 * icon_padding as i32;
            let text_x = strength_x + icon_size + icon_padding;

            // Render AP name text.

            let name = if access_point.ssid.trim().is_empty() {
                &access_point.bssid
            } else {
                &access_point.ssid
            };
            self.name_layout.set_text(name);

            let mut text_options = TextOptions::new();
            text_options.text_color(self.config.colors.foreground.as_f64());
            text_options.position(Position::new(text_x, y_padding));
            text_options.size(Size::new(text_width, name_height));
            builder.rasterize(&self.name_layout, &text_options);

            // Rasterize subtitle text.

            let sub_text = if access_point.connected {
                format!("{} MHz - Connected", access_point.frequency)
            } else {
                format!("{} MHz", access_point.frequency)
            };
            self.sub_layout.set_text(&sub_text);

            text_options.position(Position::new(text_x, y_padding + name_height as f64));
            text_options.size(Size::new(text_width, sub_height));
            text_options.text_color(self.config.colors.alt_foreground.as_f64());
            builder.rasterize(&self.sub_layout, &text_options);

            builder.build()
        })
    }

    /// Cleanup unused textures.
    ///
    /// # Safety
    ///
    /// The correct OpenGL context **must** be current or this will attempt to
    /// delete invalid OpenGL textures.
    unsafe fn free_unused_textures(&mut self) {
        // Clear cache on full redraw requests or prune unused textures.
        if mem::take(&mut self.dirty) {
            unsafe { self.clear() };
        } else {
            self.textures.retain(|key, texture| {
                let retain = self.access_points.iter().any(|c| &AccessPointKey::new(c) == key);

                // Release OpenGL texture.
                if !retain {
                    texture.delete();
                }

                retain
            });
        }
    }

    /// Remove all cached textures.
    ///
    /// # Safety
    ///
    /// The correct OpenGL context **must** be current or this will attempt to
    /// delete invalid OpenGL textures.
    unsafe fn clear(&mut self) {
        for texture in self.textures.values() {
            texture.delete();
        }
        self.textures.clear();
    }

    /// Update the configuration.
    fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;
    }
}

/// Texture cache key for WiFi connections.
#[derive(Hash, Eq, PartialEq, Clone)]
struct AccessPointKey {
    bssid: Arc<String>,
    connected: bool,
    private: bool,
    strength: u8,
}

impl AccessPointKey {
    fn new(access_point: &AccessPoint) -> Self {
        Self {
            bssid: access_point.bssid.clone(),
            connected: access_point.connected,
            strength: access_point.strength,
            private: access_point.private,
        }
    }
}

/// WiFi connection details text.
struct AccessPointDetails {
    last_bssid: Option<Arc<String>>,
    texture: Option<Texture>,
    config: Rc<Config>,
    layout: TextLayout,
    max_size: Size,
    dirty: bool,
    scale: f64,
}

impl AccessPointDetails {
    fn new(config: Rc<Config>) -> Self {
        let font_family = config.font.family.clone();
        let layout = TextLayout::new(font_family, config.font.size(1.), 1.);
        layout.set_height(i32::MIN);

        Self {
            layout,
            config,
            scale: 1.,
            last_bssid: Default::default(),
            max_size: Default::default(),
            texture: Default::default(),
            dirty: Default::default(),
        }
    }

    /// Get the rendered texture.
    ///
    /// # Safety
    ///
    /// This is only safe to call while the OpenGL context for the settings UI's
    /// renderer is bound.
    unsafe fn texture(&mut self, access_point: &AccessPoint) -> &Texture {
        // Ensure texture is up to date.
        if mem::take(&mut self.dirty)
            || self.last_bssid.as_ref().is_none_or(|bssid| bssid != &access_point.bssid)
        {
            // Ensure texture is cleared while program is bound.
            if let Some(texture) = self.texture.take() {
                texture.delete();
            }
            self.last_bssid = Some(access_point.bssid.clone());
            self.texture = Some(self.draw(access_point));
        }

        self.texture.as_ref().unwrap()
    }

    /// Draw the button into an OpenGL texture.
    fn draw(&mut self, access_point: &AccessPoint) -> Texture {
        // Ensure layout scale and font are up to date.
        self.layout.set_font(&self.config.font.family, self.config.font.size(1.));
        self.layout.set_scale(self.scale);

        // Update layout's text.
        let layout_text = format!(
            "SSID: {}\nBSSID: {}\nFrequency: {} MHz\nSecurity: {}\nConnection Strength: \
             {}%\nProfile saved: {}",
            access_point.ssid,
            access_point.bssid,
            access_point.frequency,
            access_point.private,
            access_point.strength,
            access_point.profile.is_some(),
        );
        self.layout.set_text(&layout_text);

        // Calculate required texture size.
        let (mut width, mut height) = self.layout.pixel_size();
        width = width.min(self.max_size.width as i32);
        height = height.min(self.max_size.height as i32);
        let size = Size::new(width, height);

        // Initialize as opaque texture.
        let builder = TextureBuilder::new(&self.config, size);
        builder.clear(self.config.colors.background.as_f64());

        // Render AP properties.
        let mut text_options = TextOptions::new();
        text_options.text_color(self.config.colors.foreground.as_f64());
        text_options.ellipsize(false);
        builder.rasterize(&self.layout, &text_options);

        builder.build()
    }

    /// Update the physical texture size and render scale.
    fn set_geometry(&mut self, size: Size, scale: f64) {
        self.max_size = size;
        self.scale = scale;
        self.dirty = true;
    }

    /// Update the configuration.
    fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;
    }
}

/// Button with a text label.
struct TextButton {
    texture: Option<Texture>,
    label: &'static str,
    config: Rc<Config>,
    layout: TextLayout,
    dirty: bool,
    scale: f64,
    size: Size,
}

impl TextButton {
    fn new(config: Rc<Config>, label: &'static str) -> Self {
        let font_family = config.font.family.clone();
        let layout = TextLayout::new(font_family, config.font.size(1.), 1.);
        layout.set_alignment(Alignment::Center);

        Self {
            layout,
            config,
            label,
            scale: 1.,
            texture: Default::default(),
            dirty: Default::default(),
            size: Default::default(),
        }
    }

    /// Get the rendered texture.
    ///
    /// # Safety
    ///
    /// This is only safe to call while the OpenGL context for the settings UI's
    /// renderer is bound.
    unsafe fn texture(&mut self) -> &Texture {
        // Ensure texture is up to date.
        if mem::take(&mut self.dirty) {
            // Ensure texture is cleared while program is bound.
            if let Some(texture) = self.texture.take() {
                texture.delete();
            }
            self.texture = Some(self.draw());
        }

        self.texture.as_ref().unwrap()
    }

    /// Draw the button into an OpenGL texture.
    fn draw(&mut self) -> Texture {
        // Initialize as opaque texture.
        let builder = TextureBuilder::new(&self.config, self.size.into());
        builder.clear(self.config.colors.alt_background.as_f64());

        // Ensure layout is up to date.
        self.layout.set_font(&self.config.font.family, self.config.font.size(1.));
        self.layout.set_scale(self.scale);
        self.layout.set_text(self.label);

        // Render text label.
        let mut text_options = TextOptions::new();
        text_options.text_color(self.config.colors.foreground.as_f64());
        builder.rasterize(&self.layout, &text_options);

        builder.build()
    }

    /// Update the physical texture size and render scale.
    fn set_geometry(&mut self, size: Size, scale: f64) {
        self.scale = scale;
        self.size = size;
        self.dirty = true;
    }

    /// Update the configuration.
    fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;
    }
}

/// Button with an SVG icon.
pub struct SvgButton {
    texture: Option<Texture>,
    on_svg: Svg,
    off_svg: Option<Svg>,
    enabled: bool,

    size: Size,
    scale: f64,

    config: Rc<Config>,
    dirty: bool,
}

impl SvgButton {
    pub fn new(config: Rc<Config>, svg: Svg) -> Self {
        Self {
            config,
            enabled: true,
            on_svg: svg,
            dirty: true,
            scale: 1.,
            off_svg: Default::default(),
            texture: Default::default(),
            size: Default::default(),
        }
    }

    /// Create a new SVG button with separate on/off state.
    pub fn new_toggle(config: Rc<Config>, on_svg: Svg, off_svg: Svg) -> Self {
        Self {
            config,
            on_svg,
            off_svg: Some(off_svg),
            enabled: true,
            dirty: true,
            scale: 1.,
            texture: Default::default(),
            size: Default::default(),
        }
    }

    /// Get this button's OpenGL texture.
    pub fn texture(&mut self) -> &Texture {
        // Ensure texture is up to date.
        if mem::take(&mut self.dirty) {
            // Ensure texture is cleared while program is bound.
            if let Some(texture) = self.texture.take() {
                texture.delete();
            }
            self.texture = Some(self.draw());
        }

        self.texture.as_ref().unwrap()
    }

    /// Draw the button into an OpenGL texture.
    pub fn draw(&self) -> Texture {
        // Clear with background color.
        let builder = TextureBuilder::new(&self.config, self.size.into());
        builder.clear(self.config.colors.alt_background.as_f64());

        // Draw button's icon.
        let svg = self.off_svg.filter(|_| !self.enabled).unwrap_or(self.on_svg);
        let icon_size = self.size.width.min(self.size.height) as f64 * 0.5;
        let icon_x = (self.size.width as f64 - icon_size) / 2.;
        let icon_y = (self.size.height as f64 - icon_size) / 2.;
        builder.rasterize_svg(svg, icon_x, icon_y, icon_size, icon_size);

        builder.build()
    }

    /// Set the physical size and scale of the button.
    fn set_geometry(&mut self, size: Size, scale: f64) {
        self.size = size;
        self.scale = scale;

        // Force redraw.
        self.dirty = true;
    }

    /// Update toggle state.
    fn set_enabled(&mut self, enabled: bool) {
        self.dirty |= self.enabled != enabled;
        self.enabled = enabled;
    }

    /// Update the configuration.
    fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;
    }
}

/// Touch event tracking.
#[derive(Default)]
struct TouchState {
    action: TouchAction,
    start: Position<f64>,
    position: Position<f64>,
}

/// Intention of a touch sequence.
#[derive(Default, Copy, Clone, PartialEq, Eq, Debug)]
enum TouchAction {
    #[default]
    None,
    EntryTap(usize),
    EntryDrag,
    DisconnectTap,
    PasswordInput,
    ConnectTap,
    RefreshTap,
    ForgetTap,
    ToggleTap,
    BackTap,
}

/// Scroll velocity state.
#[derive(Default)]
pub struct ScrollVelocity {
    last_tick: Option<Instant>,
    velocity: f64,
}

impl ScrollVelocity {
    /// Check if there is any velocity active.
    pub fn is_moving(&self) -> bool {
        self.velocity != 0.
    }

    /// Set the velocity.
    pub fn set(&mut self, velocity: f64) {
        self.velocity = velocity;
        self.last_tick = None;
    }

    /// Apply and update the current scroll velocity.
    pub fn apply(&mut self, input: &Input, scroll_offset: &mut f64) {
        // No-op without velocity.
        if self.velocity == 0. {
            return;
        }

        // Initialize velocity on the first tick.
        //
        // This avoids applying velocity while the user is still actively scrolling.
        let last_tick = match self.last_tick.take() {
            Some(last_tick) => last_tick,
            None => {
                self.last_tick = Some(Instant::now());
                return;
            },
        };

        // Calculate velocity steps since last tick.
        let now = Instant::now();
        let interval =
            ((now - last_tick).as_micros() / (input.velocity_interval as u128 * 1_000)) as f64;

        // Apply and update velocity.
        *scroll_offset += self.velocity * (1. - input.velocity_friction.powf(interval + 1.))
            / (1. - input.velocity_friction);
        self.velocity *= input.velocity_friction.powf(interval);

        // Request next tick if velocity is significant.
        if self.velocity.abs() > 1. {
            self.last_tick = Some(now);
        } else {
            self.velocity = 0.
        }
    }
}

/// Spawn an async taks on the calloop event loop.
fn spawn_async<F>(event_loop: &LoopHandle<'static, State>, error_message: &'static str, f: F)
where
    F: Future<Output = Result<(), zbus::Error>> + 'static,
{
    if let Err(err) = spawn_async_inner(event_loop, error_message, f) {
        error!("failed to spawn task: {err}");
    }
}

/// Spawn an async callop task without error handling.
fn spawn_async_inner<F>(
    event_loop: &LoopHandle<'static, State>,
    error_message: &'static str,
    f: F,
) -> Result<(), Error>
where
    F: Future<Output = Result<(), zbus::Error>> + 'static,
{
    let (executor, scheduler) = futures::executor()?;
    event_loop.insert_source(executor, move |result, _, _| {
        if let Err(err) = result {
            error!("{error_message}: {err}");
        }
    })?;
    scheduler.schedule(f)?;
    Ok(())
}

/// Text input with enabled-state tracking.
#[derive(Debug)]
pub struct TextInput {
    text_input: ZwpTextInputV3,
    enabled: bool,
}

impl From<ZwpTextInputV3> for TextInput {
    fn from(text_input: ZwpTextInputV3) -> Self {
        Self { text_input, enabled: false }
    }
}

impl TextInput {
    /// Enable text input on a surface.
    ///
    /// This is automatically debounced if the text input is already enabled.
    ///
    /// Does not automatically send a commit, to allow synchronized
    /// initialization of all IME state.
    pub fn enable(&mut self) {
        if self.enabled {
            return;
        }

        self.enabled = true;
        self.text_input.enable();
    }

    /// Disable text input on a surface.
    ///
    /// This is automatically debounced if the text input is already disabled.
    ///
    /// Contrary to `[Self::enable]`, this immediately sends a commit after
    /// disabling IME, since there's no need to synchronize with other
    /// events.
    pub fn disable(&mut self) {
        if !self.enabled {
            return;
        }

        self.enabled = false;
        self.text_input.disable();
        self.text_input.commit();
    }

    /// Set the surrounding text.
    pub fn set_surrounding_text(&self, text: String, cursor_index: i32, selection_anchor: i32) {
        self.text_input.set_surrounding_text(text, cursor_index, selection_anchor);
    }

    /// Indicate the cause of surrounding text change.
    pub fn set_text_change_cause(&self, cause: ChangeCause) {
        self.text_input.set_text_change_cause(cause);
    }

    /// Set text field content purpose and hint.
    pub fn set_content_type(&self, hint: ContentHint, purpose: ContentPurpose) {
        self.text_input.set_content_type(hint, purpose);
    }

    /// Set text field cursor position.
    pub fn set_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        self.text_input.set_cursor_rectangle(x, y, width, height);
    }

    /// Commit IME state.
    pub fn commit(&self) {
        self.text_input.commit();
    }
}
