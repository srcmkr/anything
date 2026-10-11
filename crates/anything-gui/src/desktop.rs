//! What the desktop does with a file: choosing a program to open it with
//! (xdg-desktop-portal), the file manager's properties dialog
//! (`org.freedesktop.FileManager1`), the trash (freedesktop trash spec) and
//! deleting. All of it blocks; call it off the UI thread.

use std::{
    collections::HashMap,
    fs::File,
    path::Path,
    sync::OnceLock,
};

use zbus::zvariant::{Fd, Value};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const FILE_MANAGER: &str = "org.freedesktop.FileManager1";

/// What this desktop offers, found out once in the background.
#[derive(Clone, Copy, Default, Debug)]
pub struct Offers {
    /// A program chooser ("Open with …").
    pub open_with: bool,
    /// A file manager that shows properties.
    pub properties: bool,
}

static OFFERS: OnceLock<Offers> = OnceLock::new();

/// Looks for the portal and the file manager, without blocking the caller.
pub fn init() {
    std::thread::Builder::new()
        .name("anything-desktop".into())
        .spawn(|| {
            let offers = zbus::blocking::Connection::session()
                .ok()
                .and_then(|bus| zbus::blocking::fdo::DBusProxy::new(&bus).ok())
                .map(|dbus| {
                    // Both are started on demand, so ask what can be activated too.
                    let names = dbus.list_activatable_names().unwrap_or_default();
                    let there = |name: &str| {
                        names.iter().any(|n| n.as_str() == name)
                            || name.try_into().is_ok_and(|n| dbus.name_has_owner(n).unwrap_or(false))
                    };
                    Offers { open_with: there(PORTAL), properties: there(FILE_MANAGER) }
                })
                .unwrap_or_default();
            OFFERS.set(offers).ok();
        })
        .ok();
}

pub fn offers() -> Offers {
    OFFERS.get().copied().unwrap_or_default()
}

/// `file:///…` with every byte that needs it escaped.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

fn session() -> Result<zbus::blocking::Connection, String> {
    zbus::blocking::Connection::session().map_err(|e| e.to_string())
}

/// Lets the user pick the program to open `path` with. `window` is the X11
/// window the chooser belongs to, if known.
pub fn open_with(path: &Path, window: Option<u32>) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let parent = window.map_or_else(String::new, |id| format!("x11:{id:x}"));
    let options: HashMap<&str, Value> = HashMap::from([("ask", Value::from(true))]);
    session()?
        .call_method(
            Some(PORTAL),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenFile",
            &(parent, Fd::from(&file), options),
        )
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The file manager's properties dialog for `path`.
pub fn show_properties(path: &Path) -> Result<(), String> {
    session()?
        .call_method(
            Some(FILE_MANAGER),
            "/org/freedesktop/FileManager1",
            Some(FILE_MANAGER),
            "ShowItemProperties",
            &(vec![file_uri(path)], ""),
        )
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Moves `path` to the trash.
pub fn trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

/// Brings `path` back from the trash: the item deleted last from there.
pub fn untrash(path: &Path) -> Result<(), String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err("no such item".into());
    };
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;
    let item = items
        .into_iter()
        .filter(|item| item.original_parent == parent && item.name == name)
        .max_by_key(|item| item.time_deleted)
        .ok_or_else(|| "not in the trash any more".to_owned())?;
    trash::os_limited::restore_all([item]).map_err(|e| e.to_string())
}

/// Deletes `path` for good; a folder with everything in it. A link is
/// removed itself, never what it points to.
pub fn delete(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let result = if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    result.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_links_not_their_target() {
        let dir = std::env::temp_dir().join(format!("anything-delete-{}", std::process::id()));
        let target = dir.join("target");
        std::fs::create_dir_all(target.join("inner")).unwrap();
        std::fs::write(target.join("inner/file"), b"x").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        delete(&link).unwrap();
        assert!(!link.exists() && target.join("inner/file").exists());
        delete(&target).unwrap();
        assert!(!target.exists());
        assert!(delete(&target).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(file_uri(Path::new("/a b/ü")), "file:///a%20b/%C3%BC");
    }
}
