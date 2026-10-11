//! User settings, stored as TOML in `$XDG_CONFIG_HOME/anything/settings.toml`.
//!
//! The settings live in a GPUI global; [`update`] changes, saves and applies
//! them in one go.

use std::path::{Path, PathBuf};

use gpui::{App, Global, Window, WindowAppearance};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode, ThemeRegistry};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [(Appearance, &'static str); 3] =
        [(Appearance::System, "system"), (Appearance::Light, "light"), (Appearance::Dark, "dark")];

    pub fn key(self) -> &'static str {
        Self::ALL.iter().find(|a| a.0 == self).map_or("system", |a| a.1)
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL.iter().find(|a| a.1 == key).map_or(Appearance::System, |a| a.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "de")]
    German,
    #[serde(rename = "es")]
    Spanish,
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "pt-BR")]
    Portuguese,
    #[serde(rename = "ru")]
    Russian,
    #[serde(rename = "zh-CN")]
    Chinese,
}

impl Language {
    pub const ALL: [(Language, &'static str); 8] = [
        (Language::System, "system"),
        (Language::English, "en"),
        (Language::German, "de"),
        (Language::Spanish, "es"),
        (Language::French, "fr"),
        (Language::Portuguese, "pt-BR"),
        (Language::Russian, "ru"),
        (Language::Chinese, "zh-CN"),
    ];

    pub fn key(self) -> &'static str {
        Self::ALL.iter().find(|l| l.0 == self).map_or("system", |l| l.1)
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL.iter().find(|l| l.1 == key).map_or(Language::System, |l| l.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub language: Language,
    /// Start with the desktop session, waiting in the tray.
    pub autostart: bool,
    /// Where the index is stored.
    pub database_dir: PathBuf,
    /// Whether the grid shows previews of files.
    pub previews: bool,
    /// Where thumbnails are looked up and stored.
    pub thumbnail_dir: PathBuf,
    /// Folders the scan covers, see `anything_core::Scope`.
    pub scan_include: Vec<String>,
    /// Folders and names the scan skips, see `anything_core::Scope`.
    pub scan_exclude: Vec<String>,
    /// Leave out the folders programs keep their caches in.
    pub skip_caches: bool,
    /// Leave out `node_modules` folders.
    pub skip_node_modules: bool,
    /// Look for new releases on GitHub.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            language: Language::default(),
            autostart: false,
            database_dir: default_database_dir(),
            previews: true,
            thumbnail_dir: default_thumbnail_dir(),
            scan_include: default_scan_include(),
            scan_exclude: Vec::new(),
            skip_caches: true,
            skip_node_modules: true,
            check_updates: true,
        }
    }
}

impl Settings {
    /// Everything the scan leaves out: the own list and what the switches add.
    pub fn all_excludes(&self) -> Vec<String> {
        anything_core::excludes(&self.scan_exclude, self.skip_caches, self.skip_node_modules)
    }
}

/// `$XDG_DATA_HOME/anything`, usually `~/.local/share/anything`.
pub fn default_database_dir() -> PathBuf {
    data_home().map_or_else(|| repo_dir().join("dev").join("db"), |data| data.join("anything"))
}

/// `$XDG_CACHE_HOME/anything/thumbnails`, apart from other programs'
/// thumbnails.
pub fn default_thumbnail_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map_or_else(|| repo_dir().join("dev").join("thumbnails"), |cache| cache.join("anything").join("thumbnails"))
}

pub fn default_scan_include() -> Vec<String> {
    vec!["/".to_owned()]
}

pub fn repo_dir() -> PathBuf {
    // crates/anything-gui -> repository root
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

fn config_path() -> Option<PathBuf> {
    Some(config_home()?.join("anything").join("settings.toml"))
}

/// Writes or removes `autostart/anything.desktop` (freedesktop autostart).
fn sync_autostart(enabled: bool) {
    let Some(config) = config_home() else { return };
    let path = config.join("autostart").join("anything.desktop");
    let icon = data_home().map(|d| d.join("anything").join("anything.svg"));
    // An AppImage runs from a different place every time; start the file.
    let exe = std::env::var_os("APPIMAGE").map(std::path::PathBuf::from).map_or_else(std::env::current_exe, Ok);
    let result = exe.and_then(|exe| {
        let icon = match (&icon, enabled) {
            (Some(icon), true) => {
                std::fs::create_dir_all(icon.parent().unwrap())?;
                std::fs::write(icon, crate::icon::SVG)?;
                Some(icon.as_path())
            }
            _ => None,
        };
        write_autostart(&path, enabled.then_some(exe.as_path()), icon)
    });
    if let Err(err) = result {
        eprintln!("anything: could not update {}: {err}", path.display());
    }
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

fn write_autostart(path: &Path, exe: Option<&Path>, icon: Option<&Path>) -> std::io::Result<()> {
    match exe {
        Some(exe) => {
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName=Anything\nComment=Instant file name search\n\
                 Exec=\"{}\"\nIcon={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe.display(),
                icon.map_or("system-search".into(), |icon| icon.display().to_string()),
            );
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, entry)
        }
        None => match std::fs::remove_file(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

impl Settings {
    /// Loads the settings; missing or unreadable files give the defaults.
    pub fn load() -> Self {
        let Some(path) = config_path() else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                eprintln!("anything: ignoring {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) {
        let Some(path) = config_path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let text = toml::to_string_pretty(self).expect("settings serialize");
                // Write to a temporary file first so a crash never leaves half a file.
                let tmp = path.with_extension("toml.tmp");
                std::fs::write(&tmp, text)?;
                std::fs::rename(&tmp, &path)
            });
        if let Err(err) = result {
            eprintln!("anything: could not save {}: {err}", path.display());
        }
    }
}

pub struct AppSettings(pub Settings);

impl Global for AppSettings {}

pub fn init(cx: &mut App) {
    let settings = Settings::load();
    if settings.autostart {
        // Keeps the entry pointing at the program that runs now.
        sync_autostart(true);
    }
    cx.set_global(AppSettings(settings));
    install_themes(cx);
}

/// Replaces the default themes: the near-black dark one with a dimmer slate
/// one meant for daytime use, the plain white light one with a soft
/// cool-gray one.
fn install_themes(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    registry.load_themes_from_str(include_str!("../assets/theme-light.json")).expect("bundled light theme");
    registry.load_themes_from_str(include_str!("../assets/theme-dark.json")).expect("bundled dark theme");
    let light = registry.themes()["Anything Light"].clone();
    let dark = registry.themes()["Anything Dark"].clone();
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    // Reloads the current mode with the new theme.
    Theme::change(cx.theme().mode, None, cx);
}

pub fn get(cx: &App) -> &Settings {
    &cx.global::<AppSettings>().0
}

/// Changes the settings, saves them if they changed, and applies them to all
/// windows.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let mut settings = get(cx).clone();
    change(&mut settings);
    if &settings == get(cx) {
        return;
    }
    settings.save();
    if settings.autostart != get(cx).autostart {
        sync_autostart(settings.autostart);
    }
    cx.set_global(AppSettings(settings));
    for window in cx.windows() {
        window.update(cx, |_, window, cx| apply_theme(window, cx)).ok();
    }
    crate::i18n::apply(cx);
    cx.refresh_windows();
}

/// Applies the chosen appearance; "System" follows the desktop.
pub fn apply_theme(window: &mut Window, cx: &mut App) {
    let mode = match get(cx).appearance {
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
        Appearance::System => match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
        },
    };
    if cx.theme().mode != mode {
        Theme::change(mode, Some(window), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_entry() {
        let dir = std::env::temp_dir().join(format!("anything-test-{}", std::process::id()));
        let path = dir.join("autostart").join("anything.desktop");
        write_autostart(&path, Some(Path::new("/opt/any thing/anything")), Some(Path::new("/i/a.svg")))
            .unwrap();
        let entry = std::fs::read_to_string(&path).unwrap();
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=\"/opt/any thing/anything\"\n"));
        assert!(entry.contains("Icon=/i/a.svg\n"));
        write_autostart(&path, None, None).unwrap();
        assert!(!path.exists());
        // Removing twice is fine.
        write_autostart(&path, None, None).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn round_trip_and_defaults() {
        let settings = Settings {
            appearance: Appearance::Dark,
            thumbnail_dir: "/tmp/t".into(),
            scan_exclude: vec!["/home/*".into()],
            ..Settings::default()
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        // Unknown or missing keys fall back to defaults.
        let partial: Settings =
            toml::from_str("appearance = \"light\"\nlanguage = \"de\"\nfuture = 1").unwrap();
        assert_eq!(partial.language, Language::German);
        assert_eq!(partial.appearance, Appearance::Light);
        assert_eq!(partial.thumbnail_dir, default_thumbnail_dir());
        assert_eq!(partial.scan_include, ["/"]);
        assert!(default_thumbnail_dir().ends_with("anything/thumbnails"));
        assert!(partial.previews);
        assert!(repo_dir().join("Cargo.toml").exists());
    }
}
