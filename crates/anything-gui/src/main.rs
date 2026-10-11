//! Anything: instant file name search.
//!
//! Lives in the tray; a click on the icon brings up the search window with
//! the cursor in the search field.

mod about;
mod actions;
mod bench;
mod demo;
mod desktop;
mod format;
mod grid;
mod i18n;
mod icon;
mod instance;
mod settings;
mod results;
mod service;
mod settings_window;
mod source;
mod state;
mod thumbnailers;
mod thumbs;
mod tray;
mod update;
mod view;
mod x11;

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use anything_core::synthetic_index_with;
use source::Source;
use gpui::{App, AppContext as _, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_component::Root;

use crate::{bench::Bench, tray::TrayEvent, view::SearchView};

rust_i18n::i18n!("locales", fallback = "en");

// Icons beyond the set gpui-component embeds by itself.
gpui_kit_assets::icon_assets!(ExtraIcons, [
    List, LayoutGrid, Image, Film, Music, FileText, FileSpreadsheet, Presentation, FileArchive,
    FileCode, File, Folder, Github, Bug, Copy, Database, Languages, HardDrive,
]);

/// gpui-component's own assets plus [`ExtraIcons`].
struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(icon) => Ok(Some(icon)),
            None => gpui_kit_assets::Assets::new("").load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut all = gpui_kit_assets::Assets::new("").list(path)?;
        all.extend(ExtraIcons.list(path)?);
        Ok(all)
    }
}

/// Size of the fake index until the real one exists.
const FAKE_ENTRIES: usize = 5_000_000;

fn main() {
    let bench = Bench::enabled();
    let show_at_start = bench || std::env::args().any(|arg| arg == "--show");

    let (events, received) = async_channel::unbounded();
    if !bench && !instance::claim(events.clone()) {
        eprintln!("anything: already running; showing its window");
        return;
    }
    let ui = if bench { state::UiState::default() } else { state::UiState::load() };

    // Benchmarks always use the fake index, so their numbers compare.
    let demo_mode = bench || std::env::var_os("ANYTHING_DEMO").is_some();
    demo::set_demo_mode(demo_mode);
    let source = if demo_mode {
        let start = Instant::now();
        let demo = demo::entries();
        if demo.is_empty() {
            eprintln!("anything: no demo files; run dev/make-demo.sh for previews");
        }
        let index = Arc::new(synthetic_index_with(FAKE_ENTRIES, 42, &demo));
        eprintln!("anything: fake index with {} entries in {:?}", index.len(), start.elapsed());
        Source::Demo(index)
    } else {
        // Use the system service if it runs; otherwise scan in this process.
        let socket = source::service_socket();
        match anything_proto::Client::connect(&socket) {
            Ok(client) => {
                eprintln!("anything: using the service at {}", socket.display());
                Source::Service { socket, client: Arc::new(std::sync::Mutex::new(Some(client))) }
            }
            Err(_) => Source::Disk,
        }
    };

    let tray = tray::spawn(events.clone());
    let skipped_version = ui.skipped_version.clone();
    let bench = bench.then(|| Rc::new(RefCell::new(Bench::new())));

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // Our translations of its texts, see locales/components.yml.
            gpui_component::_rust_i18n_extend(_rust_i18n_backend(), "ui");
            settings::init(cx);
            settings_window::init(ui.settings_window, cx);
            if bench.is_none() {
                update::init(skipped_version, events, cx);
                desktop::init();
            }
            if let Some(tray) = tray.clone() {
                cx.set_global(update::TrayUpdate(Box::new(move |version| tray.set_update(version))));
            }
            if let Some(tray) = tray.clone() {
                cx.set_global(i18n::TrayRefresh(Box::new(move || tray.refresh())));
            }
            i18n::apply(cx);
            actions::bind_keys(cx);
            let window_size = ui
                .window
                .map_or(size(px(1100.), px(680.)), |g| size(px(g.width.max(480.)), px(g.height.max(320.))));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(window_size, cx)),
                titlebar: Some(TitlebarOptions { title: Some("Anything".into()), ..Default::default() }),
                app_id: Some("anything".into()),
                show: show_at_start,
                focus: show_at_start,
                ..Default::default()
            };
            let mut search_view = None;
            let window = cx
                .open_window(options, |window, cx| {
                    settings::apply_theme(window, cx);
                    let view = cx.new(|cx| SearchView::new(source, bench, show_at_start, ui, window, cx));
                    window
                        .observe_window_appearance(settings::apply_theme)
                        .detach();
                    // The close button hides; quitting goes through the tray menu.
                    window.on_window_should_close(cx, {
                        let view = view.clone();
                        move |window, cx| {
                            view.update(cx, |view, cx| view.hide(window, cx));
                            false
                        }
                    });
                    search_view = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open the window");
            let view = search_view.unwrap();
            // Development aid for screenshots: open the settings right away.
            if std::env::var_os("ANYTHING_SETTINGS").is_some() {
                settings_window::open(cx);
            }

            cx.spawn(async move |cx| {
                while let Ok(event) = received.recv().await {
                    match event {
                        TrayEvent::Quit => {
                            window
                                .update(cx, |_, window, cx| view.read(cx).save_state(window, cx))
                                .ok();
                            instance::release();
                            cx.update(|cx| cx.quit());
                            break;
                        }
                        TrayEvent::Settings => {
                            cx.update(settings_window::open);
                            continue;
                        }
                        TrayEvent::Restart => {
                            window
                                .update(cx, |_, window, cx| view.read(cx).save_state(window, cx))
                                .ok();
                            instance::release();
                            // The updated copy; after "Install for all users", the new one.
                            let app = anything_update::layout::Install::current()
                                .app()
                                .unwrap_or_else(|| anything_update::layout::LOCAL.app.into());
                            match std::process::Command::new(&app).arg("--show").spawn() {
                                Ok(_) => {
                                    cx.update(|cx| cx.quit());
                                    break;
                                }
                                Err(err) => eprintln!("anything: cannot start {}: {err}", app.display()),
                            }
                            continue;
                        }
                        _ => {}
                    }
                    window
                        .update(cx, |_, window, cx| {
                            view.update(cx, |view, cx| match event {
                                TrayEvent::Toggle(requested) => view.toggle(requested, window, cx),
                                // Also brings an already visible window to the front.
                                TrayEvent::Show(requested) => view.show(requested, window, cx),
                                _ => {}
                            })
                        })
                        .ok();
                }
            })
            .detach();
        });
}
