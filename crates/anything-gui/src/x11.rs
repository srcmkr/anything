//! Showing and hiding the window without destroying it.
//!
//! GPUI has no hide on Linux, so on X11 we unmap and map the window ourselves
//! through its X window id. That keeps the window, its GPU surface and its
//! state alive and makes showing it nearly free.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::Cell;

use x11rb::{
    connection::Connection,
    wrapper::ConnectionExt as _,
    properties::{WmSizeHints, WmSizeHintsSpecification},
    protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, Gravity,
        PropMode, UnmapNotifyEvent,
    },
    rust_connection::RustConnection,
};

/// Gives an X11 window the application icon (`_NET_WM_ICON`).
fn set_icon_on(conn: &RustConnection, window: u32) {
    let Some(atom) = conn
        .intern_atom(false, b"_NET_WM_ICON")
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.atom)
    else {
        return;
    };
    let data = crate::icon::net_wm_icon();
    conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &data).ok();
    conn.flush().ok();
}

pub struct X11Window {
    conn: RustConnection,
    root: u32,
    window: u32,
    net_active_window: u32,
    /// Where the window was when it was hidden, in root coordinates.
    position: Cell<Option<(i32, i32)>>,
}

impl X11Window {
    /// Returns `None` when the window is not an X11 window (e.g. Wayland).
    pub fn new(window: &gpui::Window) -> Option<Self> {
        let window = match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            RawWindowHandle::Xlib(handle) => handle.window as u32,
            _ => return None,
        };
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen].root;
        let net_active_window = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
        set_icon_on(&conn, window);
        Some(Self { conn, root, window, net_active_window, position: Cell::new(None) })
    }

    /// The X11 window id.
    pub fn id(&self) -> u32 {
        self.window
    }

    /// Where the window content is on screen right now.
    pub fn current_position(&self) -> Option<(i32, i32)> {
        let reply = self.conn.translate_coordinates(self.window, self.root, 0, 0).ok()?.reply().ok()?;
        Some((reply.dst_x.into(), reply.dst_y.into()))
    }

    /// Where the window was last seen or should appear next.
    pub fn remembered_position(&self) -> Option<(i32, i32)> {
        self.position.get()
    }

    /// Makes the next [`show`](Self::show) put the window here.
    pub fn set_position(&self, x: i32, y: i32) {
        self.position.set(Some((x, y)));
    }

    pub fn hide(&self) {
        if let Some(position) = self.current_position() {
            self.position.set(Some(position));
        }
        self.conn.unmap_window(self.window).ok();
        // ICCCM 4.1.4: a synthetic UnmapNotify tells the window manager the
        // window is withdrawn, not just iconified.
        let event = UnmapNotifyEvent {
            response_type: x11rb::protocol::xproto::UNMAP_NOTIFY_EVENT,
            sequence: 0,
            event: self.root,
            window: self.window,
            from_configure: false,
        };
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .ok();
        self.conn.flush().ok();
    }

    pub fn show(&self) {
        self.apply_position();
        self.conn.map_window(self.window).ok();
        self.activate();
    }

    /// Puts the window where it was. A withdrawn window gets placed anew when
    /// mapped again, so the position goes back as user specified, with static
    /// gravity so it refers to the window itself and not to the decorations.
    pub fn apply_position(&self) {
        if let Some((x, y)) = self.position.get() {
            let mut hints = WmSizeHints::get_normal_hints(&self.conn, self.window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .flatten()
                .unwrap_or_default();
            hints.position = Some((WmSizeHintsSpecification::UserSpecified, x, y));
            hints.win_gravity = Some(Gravity::STATIC);
            hints.set_normal_hints(&self.conn, self.window).ok();
            self.conn
                .configure_window(self.window, &ConfigureWindowAux::new().x(x).y(y))
                .ok();
            self.conn.flush().ok();
        }
    }

    fn activate(&self) {
        // Source 2 ("pager") asks the window manager to really activate the
        // window instead of just flashing it in the task bar.
        let event = ClientMessageEvent::new(
            32,
            self.window,
            self.net_active_window,
            [2, x11rb::CURRENT_TIME, 0, 0, 0],
        );
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .ok();
        self.conn.flush().ok();
    }
}

/// Puts files on the clipboard the way file managers do, so that pasting in
/// one copies them. Serves the clipboard from a thread of its own until
/// another program takes it over.
pub fn copy_files(paths: Vec<std::path::PathBuf>) -> Result<(), String> {
    use x11rb::protocol::{
        Event,
        xproto::{CreateWindowAux, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT},
    };
    let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| -> Result<u32, String> {
        Ok(conn.intern_atom(false, name).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
    };
    let clipboard = atom(b"CLIPBOARD")?;
    let targets = atom(b"TARGETS")?;
    let utf8 = atom(b"UTF8_STRING")?;
    let uri_list = atom(b"text/uri-list")?;
    let gnome = atom(b"x-special/gnome-copied-files")?;
    let uris: Vec<String> = paths.iter().map(|p| crate::desktop::file_uri(p)).collect();
    let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
    let uri_text = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let gnome_text = format!("copy\n{}", uris.join("\n"));

    let owner = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(0, owner, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new())
        .map_err(|e| e.to_string())?;
    conn.set_selection_owner(owner, clipboard, x11rb::CURRENT_TIME).map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let got = conn.get_selection_owner(clipboard).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    if got.owner != owner {
        return Err("the clipboard is busy".into());
    }
    std::thread::Builder::new()
        .name("anything-clipboard".into())
        .spawn(move || {
            while let Ok(event) = conn.wait_for_event() {
                match event {
                    Event::SelectionRequest(request) => {
                        let property = if request.property == x11rb::NONE { request.target } else { request.property };
                        let answered = if request.target == targets {
                            conn.change_property32(PropMode::REPLACE, request.requestor, property, AtomEnum::ATOM, &[targets, utf8, uri_list, gnome])
                                .is_ok()
                        } else if request.target == utf8 {
                            conn.change_property8(PropMode::REPLACE, request.requestor, property, utf8, text.as_bytes()).is_ok()
                        } else if request.target == uri_list {
                            conn.change_property8(PropMode::REPLACE, request.requestor, property, uri_list, uri_text.as_bytes()).is_ok()
                        } else if request.target == gnome {
                            conn.change_property8(PropMode::REPLACE, request.requestor, property, gnome, gnome_text.as_bytes()).is_ok()
                        } else {
                            false
                        };
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: request.time,
                            requestor: request.requestor,
                            selection: request.selection,
                            target: request.target,
                            property: if answered { property } else { x11rb::NONE },
                        };
                        conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).ok();
                        conn.flush().ok();
                    }
                    // Someone else copied something.
                    Event::SelectionClear(_) => break,
                    _ => {}
                }
            }
            conn.destroy_window(owner).ok();
            conn.flush().ok();
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}
