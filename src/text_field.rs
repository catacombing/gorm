//! Text input UI element.

use std::io::Read;
use std::mem;
use std::ops::{Bound, Range, RangeBounds};
use std::rc::Rc;

use _text_input::zwp_text_input_v3::ChangeCause;
use calloop::LoopHandle;
use pangocairo::pango::SCALE as PANGO_SCALE;
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client as _text_input;
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use tracing::{error, warn};

use crate::State;
use crate::config::Config;
use crate::geometry::{Position, Size};
use crate::renderer::{TextLayout, TextOptions, Texture, TextureBuilder};

/// Horizontal padding inside the text input at scale 1.
const PADDING: f64 = 15.;

/// Maximum number of surrounding bytes submitted to IME.
///
/// The value `4000` is chosen to match the maximum Wayland protocol message
/// size, a higher value will lead to errors.
const MAX_SURROUNDING_BYTES: usize = 4000;

/// Text input field.
pub struct TextField {
    event_loop: LoopHandle<'static, State>,

    layout: TextLayout,
    cursor_index: i32,
    cursor_offset: i32,
    scroll_offset: f64,

    selection: Option<Range<i32>>,

    touch_state: TouchState,

    submit_handler: Box<dyn FnMut(String)>,

    preedit: (String, i32, i32),
    change_cause: ChangeCause,

    config: Rc<Config>,

    width: f64,
    scale: f64,

    texture: Option<Texture>,

    text_input_dirty: bool,
    focused: bool,
    dirty: bool,
}

impl TextField {
    pub fn new(config: Rc<Config>, event_loop: LoopHandle<'static, State>) -> Self {
        let font_family = config.font.monospace_family.clone();
        let font_size = config.font.size(1.);
        Self {
            event_loop,
            config,
            layout: TextLayout::new(font_family, font_size, 1.),
            submit_handler: Box::new(|_| {}),
            change_cause: ChangeCause::Other,
            text_input_dirty: true,
            dirty: true,
            scale: 1.,
            cursor_offset: Default::default(),
            scroll_offset: Default::default(),
            cursor_index: Default::default(),
            touch_state: Default::default(),
            selection: Default::default(),
            focused: Default::default(),
            preedit: Default::default(),
            texture: Default::default(),
            width: Default::default(),
        }
    }

    /// Check whether this text field requires a redraw.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Get the input's OpenGL texture.
    ///
    /// # Safety
    ///
    /// The correct OpenGL context **must** be current or this will attempt to
    /// delete invalid OpenGL textures.
    pub unsafe fn texture(&mut self, size: Size) -> &Texture {
        if mem::take(&mut self.dirty) {
            if let Some(texture) = self.texture.take() {
                texture.delete();
            }
            self.texture = Some(self.draw(size));
        }

        self.texture.as_ref().unwrap()
    }

    /// Draw the input's content into an OpenGL texture.
    pub fn draw(&mut self, size: Size) -> Texture {
        // Draw background color.
        let size = size.into();
        let builder = TextureBuilder::new(&self.config, size);
        builder.clear(self.config.colors.alt_background.as_f64());

        // Set text rendering options.
        let padding = (PADDING * self.scale).round();
        let mut text_options = TextOptions::new();
        text_options.cursor_position(self.cursor_index());
        text_options.preedit(self.preedit.clone());
        text_options.position(Position::new(padding, 0.));
        text_options.size(Size::new(size.width - 2 * padding as i32, size.height));

        // Show cursor or selection when focused.
        if self.focused {
            if self.selection.is_some() {
                text_options.selection(self.selection.clone());
            } else {
                text_options.show_cursor();
            }
        }

        // Ensure font family and size are up to date.
        self.layout.set_font(&self.config.font.family, self.config.font.size(1.));

        // Draw input text.
        builder.rasterize(&self.layout, &text_options);

        builder.build()
    }

    /// Update return key handler.
    pub fn set_submit_handler(
        &mut self,
        handler: Box<dyn FnMut(String)>,
    ) -> Box<dyn FnMut(String)> {
        mem::replace(&mut self.submit_handler, handler)
    }

    /// Set the field width in pixels.
    pub fn set_width(&mut self, width: f64) {
        self.width = width;

        // Ensure cursor is visible.
        self.update_scroll_offset();

        self.dirty = true;
    }

    /// Set the text's scale.
    pub fn set_scale(&mut self, scale: f64) {
        self.layout.set_scale(scale);
        self.scale = scale;
        self.dirty = true;
    }

    /// Update the configuration.
    pub fn set_config(&mut self, config: Rc<Config>) {
        self.config = config;
        self.dirty = true;
    }

    /// Handle new key press.
    pub fn press_key(&mut self, keysym: Keysym, modifiers: Modifiers) {
        // Ignore input with logo/alt key held.
        if modifiers.logo || modifiers.alt {
            return;
        }

        match (keysym, modifiers.shift, modifiers.ctrl) {
            (Keysym::Return, false, false) => self.submit(),
            (Keysym::Left, false, false) => {
                match self.selection.take() {
                    Some(selection) => {
                        self.cursor_index = selection.start;
                        self.cursor_offset = 0;
                    },
                    None => self.move_cursor(-1),
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Right, false, false) => {
                match self.selection.take() {
                    Some(selection) => {
                        let text_len = self.text().len() as i32;
                        if selection.end >= text_len {
                            self.cursor_index = text_len - 1;
                            self.cursor_offset = 1;
                        } else {
                            self.cursor_index = selection.end;
                            self.cursor_offset = 0;
                        }
                    },
                    None => self.move_cursor(1),
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::BackSpace, false, false) => {
                match self.selection.take() {
                    Some(selection) => self.delete_selected(selection),
                    None => {
                        // Find byte index of character after the cursor.
                        let end_index = self.cursor_index() as usize;

                        // Find byte index of character before the cursor and update the cursor.
                        self.move_cursor(-1);
                        let start_index = self.cursor_index() as usize;

                        // Remove all bytes in the range from the text.
                        let mut text = self.text();
                        text.drain(start_index..end_index);
                        self.layout.set_text(&text);

                        // Ensure cursor is still visible.
                        self.update_scroll_offset();
                    },
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::Delete, false, false) => {
                match self.selection.take() {
                    Some(selection) => self.delete_selected(selection),
                    None => {
                        // Ignore DEL if cursor is the end of the input.
                        let mut text = self.text();
                        if text.len() as i32 == self.cursor_index + self.cursor_offset {
                            return;
                        }

                        // Find byte index of character after the cursor.
                        let start_index = self.cursor_index() as usize;

                        // Find byte index of end of the character after the cursor.
                        //
                        // We use cursor motion here to ensure grapheme clusters are handled
                        // appropriately.
                        self.move_cursor(1);
                        let end_index = self.cursor_index() as usize;
                        self.move_cursor(-1);

                        // Remove all bytes in the range from the text.
                        text.drain(start_index..end_index);
                        self.layout.set_text(&text);
                    },
                }

                self.text_input_dirty = true;
                self.dirty = true;
            },
            (Keysym::XF86_Copy, ..) | (Keysym::C, true, true) => {
                // Get selected text.
                let text = match self.selection_text() {
                    Some(text) => text.to_owned(),
                    None => return,
                };

                self.event_loop.insert_idle(move |state| {
                    let serial = state.clipboard.next_serial();
                    let copy_paste_source = state
                        .protocol_states
                        .data_device_manager
                        .create_copy_paste_source(state.window.wayland_queue(), ["text/plain"]);
                    copy_paste_source.set_selection(&state.protocol_states.data_device, serial);
                    state.clipboard.source = Some(copy_paste_source);
                    state.clipboard.text = text;
                });
            },
            (Keysym::XF86_Paste, ..) | (Keysym::V, true, true) => {
                self.event_loop.insert_idle(|state| {
                    // Get available Wayland text selection.
                    let selection_offer =
                        match state.protocol_states.data_device.data().selection_offer() {
                            Some(selection_offer) => selection_offer,
                            None => return,
                        };
                    let mut pipe = match selection_offer.receive("text/plain".into()) {
                        Ok(pipe) => pipe,
                        Err(err) => {
                            warn!("Clipboard paste failed: {err}");
                            return;
                        },
                    };

                    // Read text from pipe.
                    let mut text = String::new();
                    if let Err(err) = pipe.read_to_string(&mut text) {
                        error!("Failed to read from clipboard pipe: {err}");
                        return;
                    }

                    // Paste text into text box.
                    state.window.paste(&text);
                });
            },
            (keysym, _, false) => {
                // Delete selection before writing new text.
                if let Some(selection) = self.selection.take() {
                    self.delete_selected(selection);
                }

                if let Some(key_char) = keysym.key_char() {
                    // Add character to text.
                    let index = self.cursor_index() as usize;
                    let mut text = self.text();
                    text.insert(index, key_char);
                    self.layout.set_text(&text);

                    // Move cursor behind the new character.
                    self.move_cursor(1);

                    self.text_input_dirty = true;
                    self.dirty = true;
                }
            },
            _ => (),
        }
    }

    /// Handle touch press events.
    pub fn touch_down(&mut self, time: u32, mut position: Position<f64>) {
        // Account for padding.
        position.x -= (PADDING * self.scale).round();

        // Get byte offset from X/Y position.
        let x = ((position.x - self.scroll_offset) * PANGO_SCALE as f64).round() as i32;
        let y = (position.y * PANGO_SCALE as f64).round() as i32;
        let (_, index, offset) = self.layout.xy_to_index(x, y);
        let byte_index = self.cursor_byte_index(index, offset);

        // Update touch state.
        self.touch_state.down(&self.config, time, position, byte_index, self.focused);
    }

    /// Handle touch motion events.
    pub fn touch_motion(&mut self, mut position: Position<f64>) {
        // Account for padding.
        position.x -= (PADDING * self.scale).round();

        // Update touch state.
        let delta = self.touch_state.motion(&self.config, position, self.selection.as_ref());

        // Handle touch drag actions.
        let action = self.touch_state.action;
        match action {
            // Scroll through text.
            TouchAction::Drag => {
                self.scroll_offset += delta.x;
                self.clamp_scroll_offset();

                self.dirty = true;
            },
            // Modify selection boundaries.
            TouchAction::DragSelectionStart | TouchAction::DragSelectionEnd
                if self.selection.is_some() =>
            {
                // Get byte offset from X/Y position.
                let x = ((position.x - self.scroll_offset) * PANGO_SCALE as f64).round() as i32;
                let y = (position.y * PANGO_SCALE as f64).round() as i32;
                let (_, index, offset) = self.layout.xy_to_index(x, y);
                let byte_index = self.cursor_byte_index(index, offset);

                // Update selection if it is at least one character wide.
                let selection = self.selection.as_mut().unwrap();
                let modifies_start = action == TouchAction::DragSelectionStart;
                if modifies_start && byte_index != selection.end {
                    selection.start = byte_index;
                } else if !modifies_start && byte_index != selection.start {
                    selection.end = byte_index;
                }

                // Swap modified side when input carets "overtake" each other.
                if selection.start > selection.end {
                    mem::swap(&mut selection.start, &mut selection.end);
                    self.touch_state.action = if modifies_start {
                        TouchAction::DragSelectionEnd
                    } else {
                        TouchAction::DragSelectionStart
                    };
                }

                // Ensure selection end stays visible.
                self.update_scroll_offset();

                self.text_input_dirty = true;
                self.dirty = true;
            },
            // Ignore touch motion for tap actions.
            _ => (),
        }
    }

    /// Handle touch release events.
    pub fn touch_up(&mut self) {
        // Ignore release handling for drag actions.
        if matches!(
            self.touch_state.action,
            TouchAction::Drag
                | TouchAction::DragSelectionStart
                | TouchAction::DragSelectionEnd
                | TouchAction::Focus
        ) {
            return;
        }

        // Get byte offset from X/Y position.
        let position = self.touch_state.last_position;
        let x = ((position.x - self.scroll_offset) * PANGO_SCALE as f64).round() as i32;
        let y = (position.y * PANGO_SCALE as f64).round() as i32;
        let (_, index, offset) = self.layout.xy_to_index(x, y);
        let byte_index = self.cursor_byte_index(index, offset);

        // Handle single/double/triple-taps.
        match self.touch_state.action {
            // Update cursor index on tap.
            TouchAction::Tap => {
                self.cursor_index = index;
                self.cursor_offset = offset;

                self.clear_selection();

                self.text_input_dirty = true;
                self.dirty = true;
            },
            // Select entire word at touch location.
            TouchAction::DoubleTap => {
                let text = self.text();
                let mut word_start = 0;
                let mut word_end = text.len() as i32;
                for (i, c) in text.char_indices() {
                    let i = i as i32;
                    if i + 1 < byte_index && !c.is_alphanumeric() {
                        word_start = i + 1;
                    } else if i > byte_index && !c.is_alphanumeric() {
                        word_end = i;
                        break;
                    }
                }
                self.select(word_start..word_end);
            },
            // Select everything.
            TouchAction::TripleTap => self.select(..),
            TouchAction::Drag
            | TouchAction::DragSelectionStart
            | TouchAction::DragSelectionEnd
            | TouchAction::Focus => {
                unreachable!()
            },
        }

        // Ensure focus when receiving touch input.
        self.set_focused(true);
    }

    /// Delete text around the current cursor position.
    pub fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        // Calculate removal boundaries.
        let mut text = self.text();
        let index = self.cursor_index() as usize;
        let end = (index + after_length as usize).min(text.len());
        let start = index.saturating_sub(before_length as usize);

        // Remove all bytes in the range from the text.
        text.drain(index..end);
        text.drain(start..index);
        self.layout.set_text(&text);

        // Update cursor position.
        self.cursor_index = start as i32;
        self.cursor_offset = 0;

        // Ensure cursor is visible.
        self.update_scroll_offset();

        // Set reason for next IME update.
        self.change_cause = ChangeCause::InputMethod;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Insert text at the current cursor position.
    pub fn commit_string(&mut self, text: &str) {
        // Set reason for next IME update.
        self.change_cause = ChangeCause::InputMethod;

        self.paste(text);
    }

    /// Set preedit text at the current cursor position.
    pub fn set_preedit_string(&mut self, text: String, cursor_begin: i32, cursor_end: i32) {
        // Delete selection as soon as preedit starts.
        if !text.is_empty() {
            if let Some(selection) = self.selection.take() {
                self.delete_selected(selection);
            }
        }

        self.preedit = (text, cursor_begin, cursor_end);

        // Ensure preedit end is visible.
        self.update_scroll_offset();

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Paste text into the input element.
    pub fn paste(&mut self, text: &str) {
        // Delete selection before writing new text.
        if let Some(selection) = self.selection.take() {
            self.delete_selected(selection);
        }

        // Add text to input element.
        let index = self.cursor_index() as usize;
        let mut input_text = self.text();
        input_text.insert_str(index, text);
        self.layout.set_text(&input_text);

        // Move cursor behind the new characters.
        self.cursor_index += text.len() as i32;

        // Ensure cursor is visible.
        self.update_scroll_offset();

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get current focus state.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Set input focus.
    pub fn set_focused(&mut self, focused: bool) {
        // Update selection on focus change.
        if focused && !self.focused {
            self.select(..);
        } else if !focused && self.focused {
            self.clear_selection();
        }

        self.focused = focused;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get surrounding text for IME.
    ///
    /// This will return at most `MAX_SURROUNDING_BYTES` bytes plus the current
    /// cursor positions relative to the surrounding text's origin.
    pub fn surrounding_text(&self) -> (String, i32, i32) {
        let cursor_index = self.cursor_index().max(0) as usize;
        let text = self.text();

        // Get up to half of `MAX_SURROUNDING_BYTES` after the cursor.
        let mut end = cursor_index + MAX_SURROUNDING_BYTES / 2;
        if end >= text.len() {
            end = text.len();
        } else {
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
        };

        // Get as many bytes as available before the cursor.
        let remaining = MAX_SURROUNDING_BYTES - (end - cursor_index);
        let mut start = cursor_index.saturating_sub(remaining);
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }

        let (cursor_start, cursor_end) = match &self.selection {
            Some(selection) => (selection.start, selection.end),
            None => (cursor_index as i32, cursor_index as i32),
        };

        (text[start..end].into(), cursor_start - start as i32, cursor_end - start as i32)
    }

    /// Get the current cursor geometry.
    pub fn cursor_rect(&self) -> (Position<i32>, Size<i32>) {
        let (cursor_rect, _) = self.layout.cursor_pos(self.cursor_index());
        let padding = (PADDING * self.scale).round() as i32;

        let x = padding + cursor_rect.x() / PANGO_SCALE;
        let y = cursor_rect.y() / PANGO_SCALE;

        let width = cursor_rect.width() / PANGO_SCALE;
        let height = cursor_rect.height() / PANGO_SCALE;

        (Position::new(x, y), Size::new(width, height))
    }

    /// Retrieve and reset current IME dirtiness state.
    pub fn take_text_input_dirty(&mut self) -> bool {
        mem::take(&mut self.text_input_dirty)
    }

    /// Get current text content.
    pub fn text(&self) -> String {
        self.layout.text().to_string()
    }

    /// Modify text selection.
    fn select<R>(&mut self, range: R)
    where
        R: RangeBounds<i32>,
    {
        let mut start = match range.start_bound() {
            Bound::Included(start) => *start,
            Bound::Excluded(start) => *start + 1,
            Bound::Unbounded => i32::MIN,
        };
        start = start.max(0);
        let mut end = match range.end_bound() {
            Bound::Included(end) => *end + 1,
            Bound::Excluded(end) => *end,
            Bound::Unbounded => i32::MAX,
        };
        end = end.min(self.text().len() as i32);

        if start < end {
            self.selection = Some(start..end);

            // Ensure selection end is visible.
            self.update_scroll_offset();

            self.text_input_dirty = true;
            self.dirty = true;
        } else {
            self.clear_selection();
        }
    }

    /// Clear text selection.
    fn clear_selection(&mut self) {
        self.selection = None;

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Delete the selected text.
    ///
    /// This automatically places the cursor at the start of the selection.
    fn delete_selected(&mut self, selection: Range<i32>) {
        // Remove selected text from input.
        let range = selection.start as usize..selection.end as usize;
        let mut text = self.text();
        text.drain(range);
        self.layout.set_text(&text);

        // Update cursor.
        if selection.start > 0 && selection.start == text.len() as i32 {
            self.cursor_index = selection.start - 1;
            self.cursor_offset = 1;
        } else {
            self.cursor_index = selection.start;
            self.cursor_offset = 0;
        }

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get selection text.
    fn selection_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let range = selection.start as usize..selection.end as usize;
        Some(self.text()[range].to_owned())
    }

    /// Submit current text input.
    fn submit(&mut self) {
        let text = self.text();
        (self.submit_handler)(text);

        self.set_focused(false);
    }

    /// Move the text input cursor.
    fn move_cursor(&mut self, positions: i32) {
        for _ in 0..positions.abs() {
            let direction = positions;
            let (cursor, offset) = self.layout.move_cursor_visually(
                true,
                self.cursor_index,
                self.cursor_offset,
                direction,
            );

            if (0..i32::MAX).contains(&cursor) {
                self.cursor_index = cursor;
                self.cursor_offset = offset;
            } else {
                break;
            }
        }

        // Ensure cursor is always visible.
        self.update_scroll_offset();

        self.text_input_dirty = true;
        self.dirty = true;
    }

    /// Get current cursor's byte offset.
    fn cursor_index(&self) -> i32 {
        self.cursor_byte_index(self.cursor_index, self.cursor_offset)
    }

    /// Convert a cursor's index and offset to a byte offset.
    fn cursor_byte_index(&self, index: i32, mut offset: i32) -> i32 {
        // Offset is character based, so we translate it to bytes here.
        if offset > 0 {
            let text = self.text();
            while !text.is_char_boundary((index + offset) as usize) {
                offset += 1;
            }
        }

        index + offset
    }

    /// Update the scroll offset based on cursor position.
    ///
    /// This will scroll towards the cursor to ensure it is always visible.
    fn update_scroll_offset(&mut self) {
        // For cursor ranges we jump twice to make both ends visible when possible.
        if let Some(selection) = &self.selection {
            let end = selection.end;
            self.update_scroll_offset_to(selection.start);
            self.update_scroll_offset_to(end);
        } else if self.preedit.0.is_empty() {
            self.update_scroll_offset_to(self.cursor_index());
        } else {
            self.update_scroll_offset_to(self.preedit.1);
            self.update_scroll_offset_to(self.preedit.2);
        }
    }

    /// Update the scroll offset to include a specific cursor index.
    fn update_scroll_offset_to(&mut self, cursor_index: i32) {
        let (cursor_rect, _) = self.layout.cursor_pos(cursor_index);
        let cursor_x = cursor_rect.x() as f64 / PANGO_SCALE as f64;

        // Scroll cursor back into the visible range.
        let delta = cursor_x + self.scroll_offset - self.width;
        if delta > 0. {
            self.scroll_offset -= delta;
            self.dirty = true;
        } else if cursor_x + self.scroll_offset < 0. {
            self.scroll_offset = -cursor_x;
            self.dirty = true;
        }

        self.clamp_scroll_offset();
    }

    /// Clamp the scroll offset to the field's limits.
    fn clamp_scroll_offset(&mut self) {
        let min_offset = -(self.layout.pixel_size().0 as f64 - self.width).max(0.);
        let clamped_offset = self.scroll_offset.min(0.).max(min_offset);
        self.dirty |= clamped_offset != self.scroll_offset;
        self.scroll_offset = clamped_offset;
    }
}

/// Touch event tracking.
#[derive(Default)]
struct TouchState {
    action: TouchAction,
    last_time: u32,
    last_position: Position<f64>,
    last_motion_position: Position<f64>,
    start_byte_index: i32,
}

impl TouchState {
    /// Update state from touch down event.
    fn down(
        &mut self,
        config: &Config,
        time: u32,
        position: Position<f64>,
        byte_index: i32,
        focused: bool,
    ) {
        // Update touch action.
        let delta = position - self.last_position;
        self.action = if !focused {
            TouchAction::Focus
        } else if self.last_time + config.input.max_multi_tap.as_millis() as u32 >= time
            && delta.x.powi(2) + delta.y.powi(2) <= config.input.max_tap_distance
        {
            match self.action {
                TouchAction::Tap => TouchAction::DoubleTap,
                TouchAction::DoubleTap => TouchAction::TripleTap,
                _ => TouchAction::Tap,
            }
        } else {
            TouchAction::Tap
        };

        // Reset touch origin state.
        self.start_byte_index = byte_index;
        self.last_motion_position = position;
        self.last_position = position;
        self.last_time = time;
    }

    /// Update state from touch motion event.
    ///
    /// Returns the distance moved since the last touch down or motion.
    fn motion(
        &mut self,
        config: &Config,
        position: Position<f64>,
        selection: Option<&Range<i32>>,
    ) -> Position<f64> {
        // Update incremental delta.
        let delta = position - self.last_motion_position;
        self.last_motion_position = position;

        // Never transfer out of drag/multi-tap states.
        if self.action != TouchAction::Tap {
            return delta;
        }

        // Ignore drags below the tap deadzone.
        let max_tap_distance = config.input.max_tap_distance;
        let delta = position - self.last_position;
        if delta.x.powi(2) + delta.y.powi(2) <= max_tap_distance {
            return delta;
        }

        // Check if touch motion started on selection caret, with one character leeway.
        self.action = match selection {
            Some(selection) => {
                let start_delta = (self.start_byte_index - selection.start).abs();
                let end_delta = (self.start_byte_index - selection.end).abs();

                if end_delta <= start_delta && end_delta < 2 {
                    TouchAction::DragSelectionEnd
                } else if start_delta < 2 {
                    TouchAction::DragSelectionStart
                } else {
                    TouchAction::Drag
                }
            },
            _ => TouchAction::Drag,
        };

        delta
    }
}

/// Intention of a touch sequence.
#[derive(Default, PartialEq, Eq, Copy, Clone, Debug)]
enum TouchAction {
    #[default]
    Tap,
    DoubleTap,
    TripleTap,
    Drag,
    DragSelectionStart,
    DragSelectionEnd,
    Focus,
}
