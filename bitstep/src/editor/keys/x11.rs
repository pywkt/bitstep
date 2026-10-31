//! The X11 side of [`super::HostWindow`]. Uses only `x11rb`, through a
//! connection of its own (baseview keeps its connection to itself).
//!
//! The host's window is not known on the first frame. nice-plug creates the
//! editor window on its own, on the root window, when the host asks for the
//! editor, and puts it into the host's window only when the host passes
//! that window in, which can be after the editor has drawn a frame. So the
//! editor window's `ReparentNotify` events are listened for, and the host's
//! window is taken from them. Checking just once, on the first frame, left
//! the editor with no host window at all: keys were not passed on and the
//! window stayed pinned.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, EventMask,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, KeyPressEvent, Property,
};
use x11rb::rust_connection::RustConnection;

/// How long after a change to the host window it is checked for unpinning:
/// time for the window manager to finish showing it, raised above the main
/// window as its dialog, before it stops being one.
const SETTLE: Duration = Duration::from_millis(300);

/// The editor's window, the host's window it sits in, and a connection of
/// our own to reach them with.
pub struct Target {
    conn: RustConnection,
    root: u32,
    /// The editor's own window.
    own: u32,
    /// The window `own` sits in, once that is a host window rather than the
    /// root.
    host: Option<u32>,
    /// The `WM_STATE` and `_NET_ACTIVE_WINDOW` atoms; `NONE` if the server
    /// had none to give.
    wm_state: Atom,
    net_active_window: Atom,
    /// When to check whether the host window can be unpinned, if a check
    /// is due.
    check_at: Option<Instant>,
    /// Whether the host window has just been opened, so the first unpin
    /// should also bring it to the front.
    raise: bool,
    /// Keysym → the keycode that types it, from the server's keymap.
    keycodes: HashMap<u32, u8>,
}

impl Target {
    /// Connects for the editor window `own`. `None` only if the server
    /// cannot be reached; an editor window that has no host window yet is
    /// fine, and is picked up by [`Self::update`] once it gets one.
    pub fn connect(own: u32) -> Option<Target> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;

        // Hear about `own` moving into another window. Selected before the
        // tree is asked for, so a move in between is not missed.
        let events = ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY);
        conn.change_window_attributes(own, &events).ok()?.check().ok()?;

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

        let atom = |name: &[u8]| {
            conn.intern_atom(false, name)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or(x11rb::NONE, |reply| reply.atom)
        };
        let wm_state = atom(b"WM_STATE");
        let net_active_window = atom(b"_NET_ACTIVE_WINDOW");

        let mut target = Target {
            conn,
            root,
            own,
            host: None,
            wm_state,
            net_active_window,
            check_at: None,
            raise: false,
            keycodes,
        };
        if let Some(tree) = target.conn.query_tree(own).ok().and_then(|c| c.reply().ok()) {
            target.adopt(tree.parent);
        }
        Some(target)
    }

    /// Whether there is a host window to send keys to.
    pub fn has_host(&self) -> bool {
        self.host.is_some()
    }

    /// See [`super::HostWindow::update`].
    pub fn update(&mut self) {
        let mut parent = None;
        let mut check_pin = false;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                Event::ReparentNotify(event) if event.window == self.own => {
                    parent = Some(event.parent);
                }
                Event::PropertyNotify(event)
                    if Some(event.window) == self.host
                        && event.state == Property::NEW_VALUE
                        && (event.atom == u32::from(AtomEnum::WM_TRANSIENT_FOR)
                            || event.atom == self.wm_state) =>
                {
                    check_pin = true;
                }
                _ => {}
            }
        }
        if let Some(parent) = parent {
            self.adopt(parent);
        } else if check_pin {
            self.check_at = Some(Instant::now() + SETTLE);
        }
        if let Some(at) = self.check_at
            && Instant::now() >= at
        {
            self.check_at = None;
            if let Some(host) = self.host
                && self.unpin(host)
                && std::mem::take(&mut self.raise)
            {
                self.activate(host);
            }
        }
    }

    /// Takes `parent` as the host window, unless it is the root.
    fn adopt(&mut self, parent: u32) {
        if parent == x11rb::NONE || parent == self.root {
            self.host = None;
            self.check_at = None;
            self.raise = false;
            return;
        }
        self.host = Some(parent);
        self.raise = true;
        // Hear about the host pinning the window, or the window manager
        // taking it on, later.
        let events = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        if let Ok(cookie) = self.conn.change_window_attributes(parent, &events) {
            cookie.ignore_error();
        }
        self.check_at = Some(Instant::now() + SETTLE);
    }

    /// Takes `WM_TRANSIENT_FOR` off `window` once the window manager is
    /// showing it. See the module docs of [`super`] for why. Whether it was
    /// showing, so the mark was taken off.
    fn unpin(&self, window: u32) -> bool {
        if !self.is_shown(window) {
            return false;
        }
        if let Ok(cookie) =
            self.conn.delete_property(window, AtomEnum::WM_TRANSIENT_FOR.into())
        {
            cookie.ignore_error();
        }
        let _ = self.conn.flush();
        true
    }

    /// Asks the window manager to bring `window` to the front and focus it.
    ///
    /// Needed once, right after the window opens. Bitwig maps it with
    /// `_NET_WM_USER_TIME` 0, "do not focus", so the main window keeps the
    /// focus; as its dialog the window was still kept above it, but once
    /// unpinned it is an unfocused window like any other and drops behind
    /// the main window. The request is marked as coming from a pager (source
    /// 2), as a taskbar click would be, because the window manager turns
    /// down an application's own request from a process the user did not
    /// click in.
    fn activate(&self, window: u32) {
        /// EWMH's source indication for pagers and taskbars.
        const SOURCE_PAGER: u32 = 2;
        if self.net_active_window == x11rb::NONE {
            return;
        }
        let event = ClientMessageEvent::new(
            32,
            window,
            self.net_active_window,
            [SOURCE_PAGER, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0],
        );
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        if let Ok(cookie) = self.conn.send_event(false, self.root, mask, event) {
            cookie.ignore_error();
        }
        let _ = self.conn.flush();
    }

    /// Whether the window manager is showing `window` as a desktop window:
    /// it has marked it `WM_STATE` Normal. A panel inside one of the host's
    /// windows never has that mark, so it is never unpinned.
    ///
    /// Waiting for the mark matters: unpinned before then, the window is
    /// mapped as a new window of its own that the user did not click, and
    /// focus-stealing prevention holds it back behind the host's main
    /// window. Unpinned after, it is in front and only needs the focus (see
    /// [`Self::activate`]).
    fn is_shown(&self, window: u32) -> bool {
        /// ICCCM's `NormalState`.
        const NORMAL_STATE: u32 = 1;
        self.wm_state != x11rb::NONE
            && self
                .conn
                .get_property(false, window, self.wm_state, AtomEnum::ANY, 0, 1)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
                == Some(NORMAL_STATE)
    }

    /// Sends one key event to the host window. Propagated, so it reaches
    /// whichever of the host's windows above ours listens for keys.
    /// Failures are ignored: a key that does not arrive is all it costs.
    pub fn send(&self, keysym: u32, pressed: bool, state: u16) {
        let Some(host) = self.host else {
            return;
        };
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
            event: host,
            child: x11rb::NONE,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: state.into(),
            same_screen: true,
        };
        if let Ok(cookie) = self.conn.send_event(true, host, mask, event) {
            cookie.ignore_error();
        }
        let _ = self.conn.flush();
    }
}
