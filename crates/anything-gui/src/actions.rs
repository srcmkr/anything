//! What can be done with a result: open it (with a chosen program), show it
//! in its folder, copy it or parts of its path, move it to the trash or
//! delete it, show its properties. Bound to keys inside the result list or
//! grid, and offered in the context menu.

use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, ClipboardItem, KeyBinding, Window, actions};
use gpui_component::{
    WindowExt as _,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
};
use rust_i18n::t;

use crate::{demo, desktop};

actions!(
    anything,
    [OpenItem, OpenWith, RevealItem, CopyPath, CopyName, CopyFolder, CopyFile, TrashItem, DeleteItem, ShowProperties]
);

/// Key context of the result list and grid.
pub const RESULTS: &str = "AnythingResults";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", OpenItem, Some(RESULTS)),
        KeyBinding::new("shift-enter", OpenWith, Some(RESULTS)),
        KeyBinding::new("ctrl-enter", RevealItem, Some(RESULTS)),
        KeyBinding::new("ctrl-c", CopyPath, Some(RESULTS)),
        KeyBinding::new("ctrl-shift-c", CopyName, Some(RESULTS)),
        KeyBinding::new("delete", TrashItem, Some(RESULTS)),
        KeyBinding::new("shift-delete", DeleteItem, Some(RESULTS)),
        KeyBinding::new("alt-enter", ShowProperties, Some(RESULTS)),
    ]);
}

/// Copying a file for a file manager to paste works through X11 (also
/// XWayland).
fn can_copy_files() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// The context menu. Holding Shift while opening it offers deleting for good
/// instead of the trash.
pub fn menu(menu: PopupMenu, window: &mut Window, cx: &mut App) -> PopupMenu {
    let offers = desktop::offers();
    let copy = PopupMenu::build(window, cx, |menu, _, _| {
        menu.menu(t!("menu.copy_path"), Box::new(CopyPath))
            .menu(t!("menu.copy_name"), Box::new(CopyName))
            .menu(t!("menu.copy_folder"), Box::new(CopyFolder))
            .menu_with_disabled(t!("menu.copy_file"), Box::new(CopyFile), !can_copy_files())
    });
    let menu = menu
        .menu(t!("menu.open"), Box::new(OpenItem))
        .menu_with_disabled(t!("menu.open_with"), Box::new(OpenWith), !offers.open_with)
        .menu(t!("menu.reveal"), Box::new(RevealItem))
        .separator()
        .item(PopupMenuItem::submenu(t!("menu.copy"), copy))
        .separator();
    let menu = if window.modifiers().shift {
        menu.menu(t!("menu.delete"), Box::new(DeleteItem))
    } else {
        menu.menu(t!("menu.trash"), Box::new(TrashItem))
    };
    menu.menu_with_disabled(t!("menu.properties"), Box::new(ShowProperties), !offers.properties)
}

pub enum Do {
    Open,
    OpenWith,
    Reveal,
    CopyPath,
    CopyName,
    CopyFolder,
    CopyFile,
    Properties,
}

fn copied(text: String, note: &str, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    window.push_notification(Notification::success(t!(note)), cx);
}

/// Runs `job` off the UI thread and reports a failure in the window.
pub fn in_background(window: &mut Window, cx: &mut App, job: impl FnOnce() -> Result<(), String> + Send + 'static) {
    let task = cx.background_spawn(async move { job() });
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                cx.update(|window, cx| {
                    window.push_notification(Notification::error(t!("note.failed", error = error)), cx)
                })
                .ok();
            }
        })
        .detach();
}

/// `window_id` is the X11 window, for the program chooser to belong to.
pub fn run(what: Do, path: PathBuf, window_id: Option<u32>, window: &mut Window, cx: &mut App) {
    let name = || path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match what {
        Do::Open | Do::OpenWith | Do::Reveal | Do::CopyFile | Do::Properties if !demo::may_open(&path) => {
            // Fake entries must never reach the real file system, even when a
            // file of that name happens to exist there.
            window.push_notification(
                Notification::info(path.display().to_string()).title(t!("note.demo_only")),
                cx,
            );
        }
        Do::Open => cx.open_with_system(&path),
        Do::OpenWith => in_background(window, cx, move || desktop::open_with(&path, window_id)),
        Do::Reveal => cx.reveal_path(&path),
        Do::CopyPath => copied(path.display().to_string(), "note.path_copied", window, cx),
        Do::CopyName => copied(name(), "note.name_copied", window, cx),
        Do::CopyFolder => {
            let folder = path.parent().unwrap_or(Path::new("/")).display().to_string();
            copied(folder, "note.folder_copied", window, cx)
        }
        Do::CopyFile => match crate::x11::copy_files(vec![path]) {
            Ok(()) => window.push_notification(Notification::success(t!("note.file_copied")), cx),
            Err(error) => window.push_notification(Notification::error(t!("note.failed", error = error)), cx),
        },
        Do::Properties => in_background(window, cx, move || desktop::show_properties(&path)),
    }
}
