//! New versions: a check against GitHub's latest release 30 seconds after
//! the start and every 6 hours after that, and installing one, depending on
//! how Anything is installed: from the Debian package or under `/usr/local`,
//! `anythingd --update` does it as root (downloading and checking the
//! release itself, see `anything_update`); an AppImage replaces its own file.
//! Then Anything restarts.

use std::{path::Path, time::Duration};

use anything_update::{Release, layout::Install};
use gpui::{App, AppContext as _, Global};

use crate::{
    service::{self, Action, Outcome},
    state::UiState,
    tray::TrayEvent,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
pub struct Updates {
    pub latest: Option<Release>,
    /// The last check got no answer.
    pub failed: bool,
    /// A version the user does not want to be offered.
    pub skipped: Option<String>,
    pub busy: bool,
    pub outcome: Option<Outcome>,
    events: Option<async_channel::Sender<TrayEvent>>,
}

impl Global for Updates {}

/// Lets the tray menu offer an update.
pub struct TrayUpdate(pub Box<dyn Fn(Option<String>)>);

impl Global for TrayUpdate {}

pub fn init(skipped: Option<String>, events: async_channel::Sender<TrayEvent>, cx: &mut App) {
    cx.set_global(Updates { skipped, events: Some(events), ..Default::default() });
    // ANYTHING_UPDATE_DELAY=<seconds> shortens the first wait for testing.
    let first = std::env::var("ANYTHING_UPDATE_DELAY").ok().and_then(|s| s.parse().ok()).map_or(FIRST_CHECK, Duration::from_secs);
    cx.spawn(async move |cx| {
        let mut wait = first;
        loop {
            cx.background_executor().timer(wait).await;
            wait = EVERY;
            cx.update(|cx| {
                if crate::settings::get(cx).check_updates {
                    check(cx);
                }
            });
        }
    })
    .detach();
}

/// Asks GitHub now, in the background.
pub fn check(cx: &mut App) {
    // Development aid: ANYTHING_UPDATE_FAKE=<version> pretends GitHub
    // offers that version, without asking it.
    let fake = std::env::var("ANYTHING_UPDATE_FAKE").ok();
    let task = cx.background_spawn(async move {
        match fake {
            Some(version) => Some(Release {
                page: format!("https://github.com/{}/releases", anything_update::REPOSITORY),
                version,
                assets: Vec::new(),
            }),
            None => anything_update::latest(),
        }
    });
    cx.spawn(async move |cx| {
        let latest = task.await;
        cx.update(|cx| {
            let updates = cx.global_mut::<Updates>();
            updates.failed = latest.is_none();
            if latest.is_some() {
                updates.latest = latest;
            }
            tell_tray(cx);
        });
    })
    .detach();
}

fn tell_tray(cx: &App) {
    let version = available(cx).map(|r| r.version);
    if let Some(tray) = cx.try_global::<TrayUpdate>() {
        (tray.0)(version);
    }
}

/// A newer release than this one, skipped or not.
pub fn newer(cx: &App) -> Option<Release> {
    let latest = cx.try_global::<Updates>()?.latest.clone()?;
    anything_update::is_newer(&latest.version, VERSION).then_some(latest)
}

/// A newer release the user has not skipped.
pub fn available(cx: &App) -> Option<Release> {
    let skipped = cx.try_global::<Updates>().and_then(|u| u.skipped.clone());
    newer(cx).filter(|r| skipped.as_deref() != Some(r.version.as_str()))
}

pub fn skipped(cx: &App) -> Option<String> {
    cx.try_global::<Updates>().and_then(|u| u.skipped.clone())
}

/// Whether this Anything can install an update: from the package, under
/// `/usr/local` or as an AppImage. A build run from its own folder only
/// points to the release page.
pub fn can_update() -> bool {
    Install::current() != Install::Build
}

/// Whether this is a build run from its own folder, which can be installed
/// for all users.
pub fn is_build() -> bool {
    Install::current() == Install::Build
}

/// Replaces an AppImage with the latest release's, if that is newer and
/// signed. The file belongs to the user, so no password is needed.
fn update_appimage(image: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let release = anything_update::latest().ok_or("no release information from GitHub")?;
    if !anything_update::is_newer(&release.version, VERSION) {
        return Err(format!("{} is not newer than {VERSION}", release.version));
    }
    let data = anything_update::fetch_verified(&release, &anything_update::appimage_name(&release.version))?;
    let new = image.with_extension("new");
    std::fs::write(&new, data).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&new, image).map_err(|e| e.to_string())
}

/// Never offers this version again; a later one is offered as usual.
pub fn skip(cx: &mut App) {
    let Some(release) = newer(cx) else { return };
    cx.global_mut::<Updates>().skipped = Some(release.version.clone());
    let mut state = UiState::load();
    state.skipped_version = Some(release.version);
    state.save();
    tell_tray(cx);
}

/// Installs the latest release (`Action::Update`) or this build for all
/// users (`Action::InstallApp`), then restarts from the installed copy.
pub fn install(action: Action, cx: &mut App) {
    if cx.global::<Updates>().busy {
        return;
    }
    let updates = cx.global_mut::<Updates>();
    updates.busy = true;
    updates.outcome = None;
    let task = cx.background_spawn(async move {
        match (action, Install::current()) {
            (Action::Update, Install::AppImage(image)) => match update_appimage(&image) {
                Ok(()) => Outcome::Done,
                Err(error) => Outcome::Failed(error),
            },
            _ => service::run(action, &service::probe()),
        }
    });
    cx.spawn(async move |cx| {
        let outcome = task.await;
        cx.update(|cx| {
            let updates = cx.global_mut::<Updates>();
            updates.busy = false;
            if outcome == Outcome::Done {
                if let Some(events) = &updates.events {
                    events.send_blocking(TrayEvent::Restart).ok();
                }
            } else if outcome != Outcome::Cancelled {
                updates.outcome = Some(outcome);
            }
        });
    })
    .detach();
}
