//! The host's window around the editor: passing it the keys the editor does
//! not use, and keeping it from being pinned above the host's main window.
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
//! key meant for a popover cannot delete something in the host. So do undo
//! and redo ([`history_key`]): with the editor focused they step through
//! bitstep's own edits, as in any plugin with an undo of its own.
//!
//! Bitwig shows a plugin's editor in a desktop window of its own, which it
//! marks as belonging to its main window (`WM_TRANSIENT_FOR`). Window
//! managers keep such a window above its main window, so bitstep stayed on
//! top of Bitwig and came back up whenever Bitwig was focused. When the
//! editor's parent is that kind of window, the mark is taken off once the
//! window manager is showing it, and again whenever the host puts it back, so
//! the two windows stack like any others and Alt+Tab moves between them. The
//! first time, the window is also brought to the front and focused: Bitwig
//! opens it without the focus, which only its dialog mark had kept it in
//! front of the main window.

use egui::{Key, Modifiers};

/// X11 modifier bits, as a key event's `state` carries them.
const SHIFT_MASK: u16 = 1;
const CONTROL_MASK: u16 = 1 << 2;
const ALT_MASK: u16 = 1 << 3;

/// The X keysym of Space.
const KEYSYM_SPACE: u32 = 0x20;

/// The X keysym of F1; F2..F12 follow it.
const KEYSYM_F1: u32 = 0xFFBE;

/// Which way through the edit history a key steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryKey {
    Undo,
    Redo,
}

/// The undo/redo shortcut `key` held with `modifiers` stands for, if any:
/// Ctrl+Z undoes, Ctrl+Shift+Z and Ctrl+Y redo. Alt+anything is left alone.
pub fn history_key(key: Key, modifiers: Modifiers) -> Option<HistoryKey> {
    if !modifiers.command || modifiers.alt {
        return None;
    }
    match (key, modifiers.shift) {
        (Key::Z, false) => Some(HistoryKey::Undo),
        (Key::Z, true) | (Key::Y, false) => Some(HistoryKey::Redo),
        _ => None,
    }
}

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

/// The window the editor is embedded in: sends it keys and keeps it unpinned.
/// One per open window.
pub struct HostWindow {
    #[cfg(target_os = "linux")]
    target: x11::Target,
}

impl HostWindow {
    /// Connects for the editor window behind `frame`. The host's window is
    /// found when the editor window is put into it, which can be after the
    /// first frame (see [`x11::Target`]). `None` when there is no X11 display
    /// — not Linux, say — in which case keys simply stay with the editor.
    #[cfg(target_os = "linux")]
    pub fn connect(frame: &nice_plug_egui::Frame) -> Option<HostWindow> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = frame.baseview_window().window_handle().ok()?;
        let own = match handle.as_raw() {
            RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok()?,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            _ => return None,
        };
        x11::Target::connect(own).map(|target| HostWindow { target })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn connect(_frame: &nice_plug_egui::Frame) -> Option<HostWindow> {
        // Elsewhere baseview already hands unused keys to the host.
        None
    }

    /// Catches up with what happened to the windows since the last frame:
    /// the editor being put into the host's window, and the host pinning
    /// that window above its main window. Call once a frame. Cheap: it reads
    /// only events already waiting, and asks the server something only when
    /// one of them calls for it.
    pub fn update(&mut self) {
        #[cfg(target_os = "linux")]
        self.target.update();
    }

    /// Whether there is a host window to send keys to yet.
    pub fn has_host(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.target.has_host();
        #[cfg(not(target_os = "linux"))]
        false
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
mod x11;

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
    fn undo_and_redo_keys_are_recognised() {
        // On Linux a held Ctrl arrives as both `ctrl` and `command`.
        let ctrl = Modifiers::CTRL | Modifiers::COMMAND;
        assert_eq!(history_key(Key::Z, ctrl), Some(HistoryKey::Undo));
        assert_eq!(history_key(Key::Z, ctrl | Modifiers::SHIFT), Some(HistoryKey::Redo));
        assert_eq!(history_key(Key::Y, ctrl), Some(HistoryKey::Redo));
        assert_eq!(history_key(Key::Z, NONE), None, "plain Z is typing");
        assert_eq!(history_key(Key::Z, ctrl | Modifiers::ALT), None);
        assert_eq!(history_key(Key::Y, ctrl | Modifiers::SHIFT), None);
        assert_eq!(history_key(Key::S, ctrl), None, "save still goes to the host");
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
