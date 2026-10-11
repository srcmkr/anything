//! The settings window. There is at most one; opening it again brings the
//! existing one to the front.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use anything_core::exclude_is_outside;
use anything_proto::State;
use rust_i18n::t;
use gpui::{
    App, AppContext as _, Axis, Context, Entity, Global, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, Styled as _, Subscription, TitlebarOptions, Window,
    Task, WindowBounds, WindowHandle, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Root, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsPanel},
    sidebar::{Sidebar, SidebarMenu, SidebarMenuItem},
    v_flex,
};

use crate::{
    service::{self, Action, Info, Outcome},
    settings::{self, Appearance, Language, Settings},
    state::{Geometry, UiState},
    x11::X11Window,
};

struct SettingsWindow(Option<WindowHandle<Root>>);

impl Global for SettingsWindow {}

/// Where the settings window was last, kept with the other window state.
struct Placement(Option<Geometry>);

impl Global for Placement {}

pub fn init(placement: Option<Geometry>, cx: &mut App) {
    cx.set_global(Placement(placement));
}

pub fn placement(cx: &App) -> Option<Geometry> {
    cx.try_global::<Placement>().and_then(|p| p.0)
}

pub fn open(cx: &mut App) {
    if let Some(Some(handle)) = cx.try_global::<SettingsWindow>().map(|w| w.0)
        && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }
    let saved = placement(cx);
    let window_size = saved.map_or(size(px(760.), px(520.)), |g| size(px(g.width.max(560.)), px(g.height.max(360.))));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(window_size, cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(t!("settings.title").into_owned().into()),
            ..Default::default()
        }),
        app_id: Some("anything".into()),
        ..Default::default()
    };
    let handle = cx
        .open_window(options, |window, cx| {
            settings::apply_theme(window, cx);
            window.observe_window_appearance(settings::apply_theme).detach();
            // Also sets the icon.
            let x11 = X11Window::new(window).map(Rc::new);
            if let (Some(x11), Some(g)) = (&x11, saved) {
                x11.set_position(g.x, g.y);
                x11.apply_position();
            }
            window.on_window_should_close(cx, {
                let x11 = x11.clone();
                move |window, cx| {
                    remember_placement(x11.as_deref(), window, cx);
                    let mut state = UiState::load();
                    state.settings_window = placement(cx);
                    state.save();
                    true
                }
            });
            let view = cx.new(|cx| SettingsView::new(x11, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .ok();
    cx.set_global(SettingsWindow(handle));
}

/// Notes where the settings window is now. Its position is only known on X11.
fn remember_placement(x11: Option<&X11Window>, window: &Window, cx: &mut App) {
    let size = window.bounds().size;
    let position = x11.and_then(X11Window::current_position).or(placement(cx).map(|g| (g.x, g.y)));
    let geometry = position.map(|(x, y)| Geometry { x, y, width: f32::from(size.width), height: f32::from(size.height) });
    cx.set_global(Placement(geometry));
}

/// Opens the system folder dialog and hands the chosen folder to `chosen`.
fn pick_folder(prompt: SharedString, cx: &mut App, chosen: impl FnOnce(PathBuf, &mut App) + 'static) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(prompt),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(mut paths))) = picked.await
            && let Some(dir) = paths.pop()
        {
            cx.update(|cx| chosen(dir, cx));
        }
    })
    .detach();
}

/// A muted box showing a path or pattern, shrinking with an ellipsis.
fn value_box(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .child(text.into())
}

fn hint(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div().text_xs().text_color(cx.theme().warning).child(text.into())
}

fn appearance_item() -> SettingItem {
    let options = Appearance::ALL
        .iter()
        .map(|(_, key)| ((*key).into(), t!(format!("appearance.{key}")).into_owned().into()))
        .collect();
    SettingItem::new(
        t!("settings.appearance"),
        SettingField::dropdown(
            options,
            |cx: &App| settings::get(cx).appearance.key().into(),
            |key: SharedString, cx: &mut App| {
                settings::update(cx, |s| s.appearance = Appearance::from_key(&key));
            },
        )
        .default_value(Appearance::System.key()),
    )
    .description(t!("settings.appearance.desc").into_owned())
}

fn language_item() -> SettingItem {
    let options = Language::ALL
        .iter()
        .map(|(language, key)| {
            let label: SharedString = match language {
                Language::System => t!("language.system").into_owned().into(),
                // Each language under its own name.
                Language::English => "English".into(),
                Language::German => "Deutsch".into(),
                Language::Spanish => "Español".into(),
                Language::French => "Français".into(),
                Language::Portuguese => "Português (Brasil)".into(),
                Language::Russian => "Русский".into(),
                Language::Chinese => "简体中文".into(),
            };
            ((*key).into(), label)
        })
        .collect();
    SettingItem::new(
        t!("settings.language"),
        SettingField::dropdown(
            options,
            |cx: &App| settings::get(cx).language.key().into(),
            |key: SharedString, cx: &mut App| settings::update(cx, |s| s.language = Language::from_key(&key)),
        )
        .default_value(Language::System.key()),
    )
    .description(t!("settings.language.desc").into_owned())
}

fn skip_caches_item() -> SettingItem {
    SettingItem::new(
        t!("settings.skip_caches"),
        SettingField::switch(
            |cx: &App| settings::get(cx).skip_caches,
            |on: bool, cx: &mut App| settings::update(cx, |s| s.skip_caches = on),
        )
        .default_value(true),
    )
    .description(t!("settings.skip_caches.desc", names = anything_core::CACHES.join(", ")).into_owned())
}

fn skip_node_modules_item() -> SettingItem {
    SettingItem::new(
        t!("settings.skip_node_modules"),
        SettingField::switch(
            |cx: &App| settings::get(cx).skip_node_modules,
            |on: bool, cx: &mut App| settings::update(cx, |s| s.skip_node_modules = on),
        )
        .default_value(true),
    )
    .description(t!("settings.skip_node_modules.desc").into_owned())
}

fn autostart_item() -> SettingItem {
    SettingItem::new(
        t!("settings.autostart"),
        SettingField::switch(
            |cx: &App| settings::get(cx).autostart,
            |enabled: bool, cx: &mut App| settings::update(cx, |s| s.autostart = enabled),
        )
        .default_value(false),
    )
    .description(t!("settings.autostart.desc").into_owned())
}

/// A folder setting with a "Choose…" button and reset to its default.
fn folder_item(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    // `title` and `description` are translation keys.
    get: fn(&Settings) -> &PathBuf,
    set: fn(&mut Settings, PathBuf),
    default: fn() -> PathBuf,
) -> SettingItem {
    SettingItem::new(
        t!(title),
        SettingField::render(move |_, _, cx: &mut App| {
            let dir = get(settings::get(cx)).display().to_string();
            h_flex()
                .w_full()
                .gap_2()
                .child(value_box(dir, cx))
                .child(Button::new(id).outline().label(t!("button.choose")).on_click(move |_, _, cx| {
                    pick_folder(t!(title).into_owned().into(), cx, move |dir, cx| {
                        settings::update(cx, |s| set(s, dir))
                    });
                }))
        })
        .on_reset(
            move |cx: &App| *get(settings::get(cx)) != default(),
            move |_, cx: &mut App| settings::update(cx, |s| set(s, default())),
        ),
    )
    .layout(Axis::Vertical)
    .description(t!(description).into_owned())
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix('~'), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

fn include_item() -> SettingItem {
    SettingItem::new(
        t!("settings.include"),
        SettingField::render(|_, _, cx: &mut App| {
            let include = settings::get(cx).scan_include.clone();
            v_flex()
                .w_full()
                .gap_1()
                .when(include.is_empty(), |list| {
                    list.child(hint(t!("settings.include.empty").into_owned(), cx))
                })
                .children(include.iter().enumerate().map(|(i, folder)| {
                    let missing = !expand_home(folder).is_dir();
                    v_flex()
                        .child(
                            h_flex().gap_2().child(value_box(folder.clone(), cx)).child(
                                Button::new(("remove-include", i))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Close)
                                    .tooltip(t!("button.remove"))
                                    .on_click(move |_, _, cx| {
                                        settings::update(cx, |s| {
                                            s.scan_include.remove(i);
                                        })
                                    }),
                            ),
                        )
                        .when(missing, |row| row.child(hint(t!("settings.include.missing").into_owned(), cx)))
                }))
                .child(h_flex().pt_1().child(
                    Button::new("add-include").outline().small().icon(IconName::Plus).label(t!("button.add_folder")).on_click(
                        |_, _, cx| {
                            pick_folder(t!("dialog.include").into_owned().into(), cx, |dir, cx| {
                                let dir = dir.display().to_string();
                                settings::update(cx, |s| {
                                    if !s.scan_include.contains(&dir) {
                                        s.scan_include.push(dir);
                                    }
                                });
                            });
                        },
                    ),
                ))
        })
        .on_reset(
            |cx: &App| settings::get(cx).scan_include != settings::default_scan_include(),
            |_, cx: &mut App| settings::update(cx, |s| s.scan_include = settings::default_scan_include()),
        ),
    )
    .layout(Axis::Vertical)
    .description(t!("settings.include.desc").into_owned())
}

fn previews_item() -> SettingItem {
    SettingItem::new(
        t!("settings.previews"),
        SettingField::switch(
            |cx: &App| settings::get(cx).previews,
            |enabled: bool, cx: &mut App| settings::update(cx, |s| s.previews = enabled),
        )
        .default_value(true),
    )
    .description(t!("settings.previews.desc").into_owned())
}

fn clear_previews_item() -> SettingItem {
    SettingItem::new(
        t!("settings.clear_previews"),
        SettingField::render(|_, _, _| {
            Button::new("clear-previews").outline().label(t!("button.clear_previews")).on_click(|_, _, cx| {
                let dir = settings::get(cx).thumbnail_dir.clone();
                crate::thumbs::clear(&dir);
                let next = cx.try_global::<crate::thumbs::CacheCleared>().map_or(0, |c| c.0) + 1;
                cx.set_global(crate::thumbs::CacheCleared(next));
            })
        }),
    )
    .description(t!("settings.clear_previews.desc").into_owned())
}

fn rescan_item() -> SettingItem {
    SettingItem::new(
        t!("settings.rescan"),
        SettingField::render(|_, _, _| {
            Button::new("rescan")
                .outline()
                .label(t!("button.rescan"))
                .on_click(|_, _, cx| crate::source::request_rescan(cx))
        }),
    )
    .description(t!("settings.rescan.desc").into_owned())
}

fn add_exclude(pattern: String, cx: &mut App) {
    let pattern = pattern.trim().to_owned();
    if pattern.is_empty() {
        return;
    }
    settings::update(cx, |s| {
        if !s.scan_exclude.contains(&pattern) {
            s.scan_exclude.push(pattern);
        }
    });
}

fn exclude_item(input: Entity<InputState>) -> SettingItem {
    SettingItem::new(
        t!("settings.exclude"),
        SettingField::render(move |_, _, cx: &mut App| {
            let settings = settings::get(cx);
            let (include, exclude) = (settings.scan_include.clone(), settings.scan_exclude.clone());
            let input = input.clone();
            v_flex()
                .w_full()
                .gap_1()
                .children(exclude.iter().enumerate().map(|(i, pattern)| {
                    let warning = exclude_is_outside(&include, pattern)
                        .then(|| t!("settings.exclude.outside").into_owned());
                    v_flex()
                        .child(
                            h_flex().gap_2().child(value_box(pattern.clone(), cx)).child(
                                Button::new(("remove-exclude", i))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Close)
                                    .tooltip(t!("button.remove"))
                                    .on_click(move |_, _, cx| {
                                        settings::update(cx, |s| {
                                            s.scan_exclude.remove(i);
                                        })
                                    }),
                            ),
                        )
                        .when_some(warning, |row, text| row.child(hint(text, cx)))
                }))
                .child(
                    h_flex()
                        .pt_1()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(Button::new("add-exclude").outline().small().icon(IconName::Plus).label(t!("button.add")).on_click({
                            let input = input.clone();
                            move |_, window, cx| {
                                let pattern = input.read(cx).value().to_string();
                                input.update(cx, |input, cx| input.set_value("", window, cx));
                                add_exclude(pattern, cx);
                            }
                        }))
                        .child(Button::new("add-exclude-folder").outline().small().label(t!("button.add_folder")).on_click(
                            |_, _, cx| {
                                pick_folder(t!("dialog.exclude").into_owned().into(), cx, |dir, cx| {
                                    add_exclude(dir.display().to_string(), cx)
                                });
                            },
                        )),
                )
        })
        .on_reset(
            |cx: &App| !settings::get(cx).scan_exclude.is_empty(),
            |_, cx: &mut App| settings::update(cx, |s| s.scan_exclude.clear()),
        ),
    )
    .layout(Axis::Vertical)
    .description(t!("settings.exclude.desc").into_owned())
}

/// What the "Service" page shows: the last state read, the action under
/// way, and how the last one ended.
#[derive(Default)]
struct ServicePanel {
    info: Option<Info>,
    busy: Option<Action>,
    outcome: Option<Outcome>,
}

impl Global for ServicePanel {}

fn panel(cx: &App) -> &ServicePanel {
    cx.global::<ServicePanel>()
}

/// Reads the service state in the background and shows it.
fn refresh_service(cx: &mut App) {
    let task = cx.background_spawn(async { service::probe() });
    cx.spawn(async move |cx| {
        let info = task.await;
        cx.update(|cx| {
            if panel(cx).info.as_ref() != Some(&info) {
                cx.global_mut::<ServicePanel>().info = Some(info);
            }
        });
    })
    .detach();
}

fn run_service_action(action: Action, cx: &mut App) {
    let Some(info) = panel(cx).info.clone() else { return };
    if panel(cx).busy.is_some() {
        return;
    }
    let state = cx.global_mut::<ServicePanel>();
    state.busy = Some(action);
    state.outcome = None;
    let task = cx.background_spawn(async move { service::run(action, &info) });
    cx.spawn(async move |cx| {
        let outcome = task.await;
        cx.update(|cx| {
            let state = cx.global_mut::<ServicePanel>();
            state.busy = None;
            state.outcome = Some(outcome);
            refresh_service(cx);
        });
    })
    .detach();
}

/// Green when it runs and answers, yellow on the way there or back, red
/// when it failed, grey when it is off or not installed.
fn service_state_color(info: &Info, cx: &App) -> gpui::Hsla {
    let theme = cx.theme();
    if info.manual() {
        return theme.green;
    }
    if !info.installed {
        return theme.muted_foreground;
    }
    match (info.active_state.as_str(), info.answer) {
        ("failed", _) => theme.red,
        ("active", Some((State::Ready, _))) => theme.green,
        ("active" | "activating" | "reloading" | "deactivating", _) => theme.yellow,
        _ => theme.muted_foreground,
    }
}

fn service_state_text(info: &Info) -> String {
    let count = |n: u64| crate::i18n::number(n as usize, crate::i18n::current());
    if info.manual() {
        let entries = info.answer.map_or(0, |(_, n)| n);
        return t!("service.state.manual", count = count(entries)).into_owned();
    }
    if !info.installed {
        return t!("service.state.not_installed").into_owned();
    }
    match (info.active_state.as_str(), info.answer) {
        ("failed", _) => t!("service.state.failed").into_owned(),
        ("active", Some((State::Ready, n))) => t!("service.state.running", count = count(n)).into_owned(),
        ("active", Some((State::Scanning, n))) => t!("service.state.scanning", count = count(n)).into_owned(),
        ("active", Some((State::Loading, _))) => t!("service.state.loading").into_owned(),
        ("active" | "activating" | "reloading", _) => t!("service.state.starting").into_owned(),
        ("deactivating", _) => t!("service.state.stopping").into_owned(),
        _ => t!("service.state.off").into_owned(),
    }
}

fn action_button(id: &'static str, label: &'static str, action: Action, primary: bool, busy: bool) -> Button {
    let button = Button::new(id).label(t!(label)).disabled(busy).on_click(move |_, _, cx| run_service_action(action, cx));
    if primary { button.primary() } else { button.outline() }
}

fn service_item() -> SettingItem {
    SettingItem::new(
        t!("service.title"),
        SettingField::render(|_, _, cx: &mut App| {
            let panel = panel(cx);
            let Some(info) = panel.info.clone() else {
                return v_flex().child(t!("service.reading").into_owned());
            };
            let busy = panel.busy.is_some();
            let running = info.installed && matches!(info.active_state.as_str(), "active" | "activating" | "reloading");
            let mut buttons = h_flex().w_full().gap_2().flex_wrap();
            if info.manual() {
                // Started by hand (development): nothing to switch here.
            } else if !info.installed {
                buttons = buttons.child(
                    action_button("service-install", "button.install", Action::Install, true, busy || info.bundled.is_none()),
                );
            } else {
                if info.outdated {
                    buttons = buttons.child(action_button("service-update", "button.update", Action::Install, true, busy));
                }
                if running {
                    buttons = buttons
                        .child(action_button("service-stop", "button.stop", Action::Stop, false, busy))
                        .child(action_button("service-restart", "button.restart", Action::Restart, false, busy));
                } else {
                    buttons = buttons.child(action_button("service-start", "button.start", Action::Start, !info.outdated, busy));
                }
                // Removing is set apart and marked: it also deletes the index.
                // The package's service is removed with the package.
                if !info.packaged {
                    buttons = buttons.child(div().flex_1()).child(
                        Button::new("service-uninstall")
                            .danger()
                            .label(t!("button.uninstall"))
                            .disabled(busy)
                            .on_click(|_, _, cx| run_service_action(Action::Uninstall, cx)),
                    );
                }
            }
            let note: Option<(String, Option<String>)> = match &panel.outcome {
                _ if busy => Some((t!("service.busy").into_owned(), None)),
                Some(Outcome::Refused) => Some((t!("service.refused").into_owned(), None)),
                Some(Outcome::Failed(error)) => Some((t!("service.failed", error = error).into_owned(), None)),
                Some(Outcome::NoPrompt(command)) => Some((t!("service.no_prompt").into_owned(), Some(command.clone()))),
                Some(Outcome::Done | Outcome::Cancelled) | None => None,
            };
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_none().size(px(10.)).rounded_full().bg(service_state_color(&info, cx)))
                        .child(div().font_weight(gpui::FontWeight::MEDIUM).child(service_state_text(&info))),
                )
                .when(info.manual(), |col| col.child(hint(t!("service.manual_hint").into_owned(), cx)))
                .when(info.outdated && !info.manual(), |col| col.child(hint(t!("service.outdated").into_owned(), cx)))
                .when(!info.installed && info.bundled.is_none() && !info.manual(), |col| {
                    col.child(hint(t!("service.missing").into_owned(), cx))
                })
                .child(buttons)
                .when_some(note, |col, (text, command)| {
                    col.child(hint(text, cx)).when_some(command, |col, command| {
                        col.child(
                            h_flex().gap_2().child(value_box(command.clone(), cx)).child(
                                Button::new("service-copy").outline().small().label(t!("button.copy")).on_click(
                                    move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(command.clone())),
                                ),
                            ),
                        )
                    })
                })
        }),
    )
    .layout(Axis::Vertical)
    .description(t!("service.desc").into_owned())
}

/// The About page's top: a card with icon, name, claim and version, a line
/// of what the index holds right now, and the links.
fn about_item() -> SettingItem {
    SettingItem::new(
        "",
        SettingField::render(|_, _, cx: &mut App| {
            static ICON: std::sync::OnceLock<std::sync::Arc<gpui::Image>> = std::sync::OnceLock::new();
            let icon = ICON
                .get_or_init(|| std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Svg, crate::icon::SVG.to_vec())))
                .clone();
            let theme = cx.theme();
            let (muted, border) = (theme.muted_foreground, theme.border);
            let lang = crate::i18n::current();

            // The version, with how it stands against the latest release.
            let state = cx.global::<crate::update::Updates>();
            let (dot, status) = match crate::update::newer(cx) {
                Some(_) => (theme.red, t!("about.badge.update")),
                None if state.latest.is_some() => (theme.green, t!("about.badge.current")),
                None => (muted, t!("about.badge.unknown")),
            };
            let badge = h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .rounded_full()
                .border_1()
                .border_color(border)
                .bg(theme.background.opacity(0.6))
                .text_sm()
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(format!("v{}", crate::about::VERSION)))
                .child(div().size(px(8.)).rounded_full().bg(dot))
                .child(div().text_color(muted).child(status.into_owned()));

            let card = v_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_6()
                .py_6()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(border)
                // The teal of the icon, the same in light and dark.
                .bg(gpui::linear_gradient(
                    160.,
                    gpui::linear_color_stop(gpui::Hsla::from(gpui::rgb(0x1f7f97)).opacity(0.30), 0.),
                    gpui::linear_color_stop(gpui::Hsla::from(gpui::rgb(0x105365)).opacity(0.04), 1.),
                ))
                .child(gpui::img(icon).size(px(88.)))
                .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).child("Anything"))
                .child(div().max_w(px(420.)).text_center().child(t!("about.tagline").into_owned()))
                .child(div().pt_1().child(badge));

            // What Anything does for you right now.
            let mut facts: Vec<(gpui_kit_assets::IconName, String)> = Vec::new();
            if let Some(entries) = crate::source::index_size(cx) {
                facts.push((
                    gpui_kit_assets::IconName::Database,
                    t!("about.stats.files", count = crate::i18n::number(entries, lang)).into_owned(),
                ));
            }
            facts.push(if crate::source::using_service(cx) {
                (gpui_kit_assets::IconName::HardDrive, t!("about.stats.service").into_owned())
            } else {
                (gpui_kit_assets::IconName::HardDrive, t!("about.stats.own").into_owned())
            });
            facts.push((
                gpui_kit_assets::IconName::Languages,
                t!("about.stats.languages", count = crate::i18n::Lang::ALL.len()).into_owned(),
            ));
            let facts = h_flex().w_full().justify_center().flex_wrap().gap_x_5().gap_y_1().text_sm().text_color(muted).children(
                facts.into_iter().map(|(icon, text)| h_flex().gap_1p5().child(Icon::new(icon).size_4()).child(text)),
            );

            let link = |id: &'static str, icon: gpui_kit_assets::IconName, label: &'static str, url: String| {
                Button::new(id).outline().icon(icon).label(t!(label)).on_click(move |_, _, cx| cx.open_url(&url))
            };
            let buttons = h_flex()
                .w_full()
                .justify_center()
                .flex_wrap()
                .gap_2()
                .child(link("about-source", gpui_kit_assets::IconName::Github, "about.source", crate::about::REPOSITORY.to_owned()))
                .child(link("about-issues", gpui_kit_assets::IconName::Bug, "about.issues", crate::about::issues_url()))
                .child(Button::new("about-copy").outline().icon(gpui_kit_assets::IconName::Copy).label(t!("about.copy")).on_click(
                    |_, window, cx| {
                        let text = crate::about::report(panel(cx).info.as_ref(), crate::source::using_service(cx));
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        window.push_notification(Notification::success(t!("about.copied")), cx);
                    },
                ));

            v_flex().w_full().gap_4().child(card).child(facts).child(buttons)
        }),
    )
    .layout(Axis::Vertical)
}

/// Licence and thanks, quietly at the bottom.
fn about_footer_item() -> SettingItem {
    SettingItem::new(
        "",
        SettingField::render(|_, _, cx: &mut App| {
            v_flex()
                .w_full()
                .items_center()
                .gap_1()
                .pt_4()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t!("about.thanks").into_owned())
                .child(t!("about.license").into_owned())
        }),
    )
    .layout(Axis::Vertical)
}

fn updates_switch_item() -> SettingItem {
    SettingItem::new(
        t!("settings.updates"),
        SettingField::switch(
            |cx: &App| settings::get(cx).check_updates,
            |enabled: bool, cx: &mut App| {
                settings::update(cx, |s| s.check_updates = enabled);
                if enabled {
                    crate::update::check(cx);
                }
            },
        )
        .default_value(true),
    )
    .description(t!("settings.updates.desc").into_owned())
}

/// Whether this Anything is current, and installing it for all users.
fn update_status_item() -> SettingItem {
    use crate::update;
    SettingItem::new(
        t!("update.status.title"),
        SettingField::render(|_, _, cx: &mut App| {
            let state = cx.global::<update::Updates>();
            let newer = update::newer(cx);
            let text = match &newer {
                Some(release) => t!("update.status.newer", version = release.version).into_owned(),
                None if state.latest.is_some() => t!("update.status.current").into_owned(),
                None if state.failed => t!("update.status.unknown").into_owned(),
                None if !settings::get(cx).check_updates => t!("update.status.off").into_owned(),
                None => t!("update.status.pending").into_owned(),
            };
            let dot = match &newer {
                Some(_) => cx.theme().red,
                None if state.latest.is_some() => cx.theme().green,
                None => cx.theme().muted_foreground,
            };
            let busy = state.busy;
            let (can_update, build) = (update::can_update(), update::is_build());
            let mut buttons = h_flex().gap_2().flex_wrap();
            if let Some(release) = newer {
                buttons = buttons.child(if can_update {
                    Button::new("about-update")
                        .primary()
                        .label(t!("update.install"))
                        .disabled(busy)
                        .on_click(|_, _, cx| update::install(Action::Update, cx))
                } else {
                    Button::new("about-download")
                        .primary()
                        .label(t!("update.download"))
                        .on_click(move |_, _, cx| cx.open_url(&release.page))
                });
            }
            if build {
                buttons = buttons.child(
                    Button::new("about-install-app")
                        .outline()
                        .label(t!("about.install_app"))
                        .disabled(busy || service::bundled().is_none())
                        .on_click(|_, _, cx| update::install(Action::InstallApp, cx)),
                );
            }
            let note = match &state.outcome {
                _ if busy => Some(t!("update.installing").into_owned()),
                Some(Outcome::Refused) => Some(t!("service.refused").into_owned()),
                Some(Outcome::Failed(error)) => Some(t!("update.failed", error = error).into_owned()),
                Some(Outcome::NoPrompt(command)) => Some(format!("{} {command}", t!("service.no_prompt"))),
                _ => None,
            };
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .flex_wrap()
                        .child(h_flex().gap_2().child(div().flex_none().size(px(10.)).rounded_full().bg(dot)).child(text))
                        .child(div().flex_1())
                        .child(buttons),
                )
                .when(build, |col| col.child(div().text_xs().text_color(cx.theme().muted_foreground).child(t!("about.install_app.desc").into_owned())))
                .when_some(note, |col, note| col.child(hint(note, cx)))
        }),
    )
    .layout(Axis::Vertical)
}

struct SettingsView {
    exclude_input: Entity<InputState>,
    page: Page,
    /// Keeps the service state current while the window is open.
    _service_poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(x11: Option<Rc<X11Window>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let exclude_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("settings.exclude.placeholder")));
        let language_changed = cx.observe_global_in::<settings::AppSettings>(window, |this, window, cx| {
            window.set_window_title(&t!("settings.title"));
            this.exclude_input.update(cx, |input, cx| {
                input.set_placeholder(t!("settings.exclude.placeholder"), window, cx)
            });
        });
        let subscription = cx.subscribe_in(&exclude_input, window, |_, input, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let pattern = input.read(cx).value().to_string();
                input.update(cx, |input, cx| input.set_value("", window, cx));
                add_exclude(pattern, cx);
            }
        });
        if !cx.has_global::<ServicePanel>() {
            cx.set_global(ServicePanel::default());
        }
        // Kept current so quitting with the window open remembers it too.
        let moved = cx.observe_window_bounds(window, move |_, window, cx| remember_placement(x11.as_deref(), window, cx));
        let service_changed = cx.observe_global::<ServicePanel>(|_, cx| cx.notify());
        let source_changed = cx.observe_global::<crate::source::UsingService>(|_, cx| cx.notify());
        let updates_changed = cx.observe_global::<crate::update::Updates>(|_, cx| cx.notify());
        let service_poll = cx.spawn(async move |this, cx| {
            loop {
                if this.upgrade().is_none() {
                    break;
                }
                cx.update(refresh_service);
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
            }
        });
        Self {
            exclude_input,
            page: Page::from_env(),
            _service_poll: service_poll,
            _subscriptions: vec![subscription, language_changed, moved, service_changed, source_changed, updates_changed],
        }
    }
}

/// The pages of the settings, in the order of the navigation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    General,
    ScanFolders,
    ScanLeftOut,
    ScanIndex,
    Service,
    Previews,
    Storage,
    About,
}

impl Page {
    /// Development aid for screenshots: ANYTHING_SETTINGS=<number>, in the
    /// order of the top level (1 is Scan); 6 and 7 are Scan's other parts.
    fn from_env() -> Self {
        match std::env::var("ANYTHING_SETTINGS").ok().and_then(|p| p.parse::<u8>().ok()) {
            Some(1) => Page::ScanFolders,
            Some(2) => Page::Service,
            Some(3) => Page::Previews,
            Some(4) => Page::Storage,
            Some(5) => Page::About,
            Some(6) => Page::ScanLeftOut,
            Some(7) => Page::ScanIndex,
            _ => Page::General,
        }
    }

    fn is_scan(self) -> bool {
        matches!(self, Page::ScanFolders | Page::ScanLeftOut | Page::ScanIndex)
    }
}

impl SettingsView {
    fn nav_item(&self, label: impl Into<SharedString>, page: Page, cx: &mut Context<Self>) -> SidebarMenuItem {
        SidebarMenuItem::new(label).active(self.page == page).on_click(cx.listener(move |this, _, _, cx| {
            this.page = page;
            cx.notify();
        }))
    }

    /// The navigation on the left; Scan has its parts below it.
    fn navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut scan_parts = vec![
            self.nav_item(t!("settings.scan.folders"), Page::ScanFolders, cx),
            self.nav_item(t!("settings.scan.left_out"), Page::ScanLeftOut, cx),
        ];
        // The service scans on its own.
        if !crate::source::using_service(cx) {
            scan_parts.push(self.nav_item(t!("settings.scan.index"), Page::ScanIndex, cx));
        }
        let scan = SidebarMenuItem::new(t!("settings.page.scan"))
            .default_open(true)
            .click_to_open(true)
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.page.is_scan() {
                    this.page = Page::ScanFolders;
                    cx.notify();
                }
            }))
            .children(scan_parts);
        Sidebar::new("settings-navigation").w(px(200.)).border_0().collapsible(false).child(
            SidebarMenu::new()
                .child(self.nav_item(t!("settings.page.general"), Page::General, cx))
                .child(scan)
                .child(self.nav_item(t!("service.page"), Page::Service, cx))
                .child(self.nav_item(t!("settings.page.previews"), Page::Previews, cx))
                .child(self.nav_item(t!("settings.page.folders"), Page::Storage, cx))
                .child(self.nav_item(t!("about.page"), Page::About, cx)),
        )
    }

    fn page(&self, page: Page) -> SettingPage {
        let scan = |part: &str| SettingPage::new(format!("{} › {}", t!("settings.page.scan"), t!(part))).resettable(true);
        match page {
            Page::General => SettingPage::new(t!("settings.page.general")).resettable(true).group(
                SettingGroup::new().item(appearance_item()).item(language_item()).item(autostart_item()),
            ),
            Page::ScanFolders => scan("settings.scan.folders")
                .group(SettingGroup::new().item(include_item()).item(exclude_item(self.exclude_input.clone()))),
            Page::ScanLeftOut => scan("settings.scan.left_out")
                .group(SettingGroup::new().item(skip_caches_item()).item(skip_node_modules_item())),
            Page::ScanIndex => scan("settings.scan.index").group(SettingGroup::new().item(rescan_item())),
            Page::Service => SettingPage::new(t!("service.page")).group(SettingGroup::new().item(service_item())),
            Page::Previews => SettingPage::new(t!("settings.page.previews")).resettable(true).group(
                SettingGroup::new()
                    .item(previews_item())
                    .item(clear_previews_item())
                    .item(folder_item(
                        "choose-thumbnail-dir",
                        "settings.thumbnails",
                        "settings.thumbnails.desc",
                        |s| &s.thumbnail_dir,
                        |s, dir| s.thumbnail_dir = dir,
                        settings::default_thumbnail_dir,
                    )),
            ),
            Page::Storage => SettingPage::new(t!("settings.page.folders")).resettable(true).group(
                SettingGroup::new().item(folder_item(
                    "choose-database-dir",
                    "settings.database",
                    "settings.database.desc",
                    |s| &s.database_dir,
                    |s, dir| s.database_dir = dir,
                    settings::default_database_dir,
                )),
            ),
            Page::About => SettingPage::new(t!("about.page")).group(
                SettingGroup::new()
                    .item(about_item())
                    .item(update_status_item())
                    .item(updates_switch_item())
                    .item(about_footer_item()),
            ),
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scan's index part only exists without the service.
        if self.page == Page::ScanIndex && crate::source::using_service(cx) {
            self.page = Page::ScanFolders;
        }
        let hidden = gpui::StyleRefinement { display: Some(gpui::Display::None), ..Default::default() };
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.navigation(cx))
            .child(
                div().flex_1().min_w_0().h_full().border_l_1().border_color(cx.theme().sidebar_border).child(
                    // The panel shows one page; its own sidebar (one entry,
                    // no sub-pages) stays hidden. One per page and language:
                    // its state, like the scroll position, belongs to the page.
                    SettingsPanel::new(SharedString::from(format!("settings-{}-{:?}", &*rust_i18n::locale(), self.page)))
                        .sidebar_width(px(0.))
                        .sidebar_size_range(px(0.)..px(0.))
                        .sidebar_style(&hidden)
                        .header_style(&hidden)
                        .page(self.page(self.page)),
                ),
            )
    }
}
