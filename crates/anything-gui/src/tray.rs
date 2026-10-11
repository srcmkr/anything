//! Status notifier (tray) icon. Runs on its own thread and reports clicks to
//! the GPUI side through a channel.

use std::time::Instant;

use ksni::{blocking::TrayMethods as _, menu::StandardItem};
use rust_i18n::t;

pub enum TrayEvent {
    /// Left click: show or hide the window. Carries the click time so the
    /// window can report how long it took to appear.
    Toggle(Instant),
    Show(Instant),
    Settings,
    Quit,
    /// After an update: start the installed copy and quit.
    Restart,
}

pub struct AnythingTray {
    events: async_channel::Sender<TrayEvent>,
    /// A newer version to offer.
    update: Option<String>,
}

impl ksni::Tray for AnythingTray {
    fn id(&self) -> String {
        "anything".into()
    }

    fn title(&self) -> String {
        "Anything".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        crate::icon::SIZES
            .iter()
            .map(|&size| ksni::Icon {
                width: size as i32,
                height: size as i32,
                data: crate::icon::argb_bytes(size),
            })
            .collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Anything".into(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.events.send_blocking(TrayEvent::Toggle(Instant::now())).ok();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut items = Vec::new();
        if let Some(version) = &self.update {
            items.push(
                StandardItem {
                    label: t!("tray.update", version = version).into(),
                    icon_name: "software-update-available".into(),
                    // The window shows the update with its buttons.
                    activate: Box::new(|this: &mut Self| {
                        this.events.send_blocking(TrayEvent::Show(Instant::now())).ok();
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(ksni::MenuItem::Separator);
        }
        items.extend([
            StandardItem {
                label: t!("tray.show").into(),
                activate: Box::new(|this: &mut Self| {
                    this.events.send_blocking(TrayEvent::Show(Instant::now())).ok();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: t!("tray.settings").into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|this: &mut Self| {
                    this.events.send_blocking(TrayEvent::Settings).ok();
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: t!("tray.quit").into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    this.events.send_blocking(TrayEvent::Quit).ok();
                }),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

/// Keeps the tray icon alive and lets it pick up a new language.
#[derive(Clone)]
pub struct TrayHandle(ksni::blocking::Handle<AnythingTray>);

impl TrayHandle {
    /// Rebuilds the menu, e.g. after a language change.
    pub fn refresh(&self) {
        self.0.update(|_| {});
    }

    /// Offers `version` in the menu, or nothing.
    pub fn set_update(&self, version: Option<String>) {
        self.0.update(|tray| tray.update = version);
    }
}

/// Starts the tray icon.
pub fn spawn(events: async_channel::Sender<TrayEvent>) -> Option<TrayHandle> {
    match (AnythingTray { events, update: None }).spawn() {
        Ok(handle) => Some(TrayHandle(handle)),
        Err(err) => {
            eprintln!("anything: no tray icon available: {err}");
            None
        }
    }
}
