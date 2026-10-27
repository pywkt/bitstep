//! Passing the keys the editor does not use on to the host's window.
//!
//! On Linux the plugin window takes the keyboard focus when it is clicked
//! (egui-baseview asks for it), and baseview's X11 backend drops every key the
//! editor does not use, where on Windows and macOS it hands them to the host.
//! So once the window had been clicked, Bitwig never saw Space and the
//! transport could not be started from it. This module sends those keys on to
//! the window the editor is embedded in, which is where they would have gone
//! had the editor not been listening.
//!
//! Only transport and shortcut keys are passed on — Space, anything held with
//! Ctrl, and F1–F12 — and never while a text field is being typed in. Keys the
//! editor uses itself (Escape, Enter, Tab, the arrows, Delete) stay here, so a
//! key meant for a popover cannot delete something in the host.

use egui::{Key, Modifiers};

/// X11 modifier bits, as a key event's `state` carries them.
const SHIFT_MASK: u16 = 1;
const CONTROL_MASK: u16 = 1 << 2;
const ALT_MASK: u16 = 1 << 3;

/// The X keysym of Space.
const KEYSYM_SPACE: u32 = 0x20;

/// The X keysym of F1; F2..F12 follow it.
const KEYSYM_F1: u32 = 0xFFBE;

/// The X keysym to pass `key` on to the host as, when it is held with
/// `modifiers` — or `None` when the editor keeps it.
pub fn forwarded_keysym(key: Key, modifiers: Modifiers) -> Option<u32> {
    if key == Key::Space {
        return Some(KEYSYM_SPACE);
    }
    let name = key.name();
    if let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=12).contains(&number)
    {
        return Some(KEYSYM_F1 + number - 1);
    }
    // Letters and digits only with Ctrl: on their own they are typing, and
    // Bitwig's shortcuts for them (save, undo, copy…) all use Ctrl.
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if modifiers.ctrl && c.is_ascii_alphanumeric() => {
            // Lower case: the keysym on the key itself, with Shift in the
            // state the way a real key press carries it.
            Some(u32::from(c.to_ascii_lowercase()))
        }
        _ => None,
    }
}

/// `modifiers` as the `state` field of an X key event.
pub fn x11_state(modifiers: Modifiers) -> u16 {
    let mut state = 0;
    if modifiers.shift {
        state |= SHIFT_MASK;
    }
    if modifiers.ctrl {
        state |= CONTROL_MASK;
    }
    if modifiers.alt {
        state |= ALT_MASK;
    }
    state
}

/// Sends keys to the window the editor is embedded in. One per open window.
pub struct KeyForwarder {
    #[cfg(target_os = "linux")]
    target: x11::Target,
}

impl KeyForwarder {
    /// Connects for the editor window behind `frame`. `None` when there is no
    /// X11 host window to send to — a standalone window, or not Linux — in
    /// which case keys simply stay with the editor, as before.
    #[cfg(target_os = "linux")]
    pub fn connect(frame: &nice_plug_egui::Frame) -> Option<KeyForwarder> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = frame.baseview_window().window_handle().ok()?;
        let own = match handle.as_raw() {
            RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok()?,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            _ => return None,
        };
        x11::Target::connect(own).map(|target| KeyForwarder { target })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn connect(_frame: &nice_plug_egui::Frame) -> Option<KeyForwarder> {
        // Elsewhere baseview already hands unused keys to the host.
        None
    }

    /// Sends one key press or release.
    pub fn send(&self, keysym: u32, pressed: bool, modifiers: Modifiers) {
        #[cfg(target_os = "linux")]
        self.target.send(keysym, pressed, x11_state(modifiers));
        #[cfg(not(target_os = "linux"))]
        let _ = (keysym, pressed, modifiers);
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::collections::HashMap;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ConnectionExt, EventMask, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, KeyPressEvent,
    };
    use x11rb::rust_connection::RustConnection;

    /// The host's window and a connection of our own to reach it with.
    pub struct Target {
        conn: RustConnection,
        root: u32,
        window: u32,
        /// Keysym → the keycode that types it, from the server's keymap.
        keycodes: HashMap<u32, u8>,
    }

    impl Target {
        /// Finds the window `own` is embedded in. `None` if it is a top-level
        /// window (nothing to forward to) or the server cannot be reached.
        pub fn connect(own: u32) -> Option<Target> {
            let (conn, screen) = x11rb::connect(None).ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            let tree = conn.query_tree(own).ok()?.reply().ok()?;
            if tree.parent == x11rb::NONE || tree.parent == root {
                return None;
            }

            let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
            let count = max.checked_sub(min)?.checked_add(1)?;
            let map = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
            let per = usize::from(map.keysyms_per_keycode).max(1);
            let mut keycodes = HashMap::new();
            for (index, keysyms) in map.keysyms.chunks(per).enumerate() {
                let Ok(code) = u8::try_from(usize::from(min) + index) else {
                    break;
                };
                // The plain and shifted symbols; the first key that has one
                // wins, which is the main keyboard's rather than the keypad's.
                for &keysym in keysyms.iter().take(2) {
                    keycodes.entry(keysym).or_insert(code);
                }
            }
            Some(Target { conn, root, window: tree.parent, keycodes })
        }

        /// Sends one key event to the host window. Propagated, so it reaches
        /// whichever of the host's windows above ours listens for keys.
        /// Failures are ignored: a key that does not arrive is all it costs.
        pub fn send(&self, keysym: u32, pressed: bool, state: u16) {
            let Some(&detail) = self.keycodes.get(&keysym) else {
                return;
            };
            let (response_type, mask) = match pressed {
                true => (KEY_PRESS_EVENT, EventMask::KEY_PRESS),
                false => (KEY_RELEASE_EVENT, EventMask::KEY_RELEASE),
            };
            let event = KeyPressEvent {
                response_type,
                detail,
                sequence: 0,
                time: x11rb::CURRENT_TIME,
                root: self.root,
                event: self.window,
                child: x11rb::NONE,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: state.into(),
                same_screen: true,
            };
            if let Ok(cookie) = self.conn.send_event(true, self.window, mask, event) {
                cookie.ignore_error();
            }
            let _ = self.conn.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn space_and_function_keys_always_go_to_the_host() {
        assert_eq!(forwarded_keysym(Key::Space, NONE), Some(0x20));
        assert_eq!(forwarded_keysym(Key::Space, Modifiers::SHIFT), Some(0x20));
        assert_eq!(forwarded_keysym(Key::F1, NONE), Some(0xFFBE));
        assert_eq!(forwarded_keysym(Key::F12, NONE), Some(0xFFC9));
    }

    #[test]
    fn letters_and_digits_go_only_with_ctrl() {
        assert_eq!(forwarded_keysym(Key::S, NONE), None);
        assert_eq!(forwarded_keysym(Key::S, Modifiers::CTRL), Some(u32::from(b's')));
        assert_eq!(forwarded_keysym(Key::Num1, Modifiers::CTRL), Some(u32::from(b'1')));
    }

    #[test]
    fn keys_the_editor_uses_stay_with_it() {
        for key in [Key::Escape, Key::Enter, Key::Tab, Key::Delete, Key::Backspace, Key::ArrowLeft]
        {
            assert_eq!(forwarded_keysym(key, NONE), None, "{key:?}");
            assert_eq!(forwarded_keysym(key, Modifiers::CTRL), None, "{key:?} with Ctrl");
        }
        // Not a function key, even though the name starts with F.
        assert_eq!(forwarded_keysym(Key::F13, NONE), None);
    }

    #[test]
    fn modifiers_map_to_x11_state_bits() {
        assert_eq!(x11_state(NONE), 0);
        assert_eq!(x11_state(Modifiers::SHIFT), 1);
        assert_eq!(x11_state(Modifiers::CTRL), 4);
        assert_eq!(x11_state(Modifiers::ALT), 8);
        assert_eq!(x11_state(Modifiers::CTRL | Modifiers::SHIFT), 5);
    }
}
