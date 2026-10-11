//! Window and view state that survives a restart, kept apart from the
//! settings in `$XDG_STATE_HOME/anything/state.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    /// Position of the window content on screen, in pixels.
    pub x: i32,
    pub y: i32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub window: Option<Geometry>,
    pub settings_window: Option<Geometry>,
    /// A release the user chose to skip.
    pub skipped_version: Option<String>,
    /// "list" or "grid".
    pub view: String,
    /// "name", "path", "size" or "modified".
    pub sort: String,
    pub descending: bool,
    pub column_widths: Vec<f32>,
    /// Tile size in the grid, see `grid::ZOOM_LEVELS`.
    pub grid_zoom: Option<usize>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("anything").join("state.toml"))
}

impl UiState {
    pub fn load() -> Self {
        path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let result = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
            let tmp = path.with_extension("toml.tmp");
            std::fs::write(&tmp, toml::to_string_pretty(self).expect("state serializes"))?;
            std::fs::rename(&tmp, &path)
        });
        if let Err(err) = result {
            eprintln!("anything: could not save {}: {err}", path.display());
        }
    }
}
