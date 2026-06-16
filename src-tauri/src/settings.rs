//! User settings, persisted to `%LOCALAPPDATA%\WattMail\settings.json`.
//!
//! Windows-only path for now; a cross-platform config-dir abstraction comes with
//! the macOS/Linux work. On a platform without `LOCALAPPDATA`, settings fall back
//! to defaults and saves are a no-op error surfaced to the caller.

use std::io;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
        }
    }
}

/// Tauri-managed settings, shared with the window-close handler.
pub struct SettingsState(pub RwLock<Settings>);

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("WattMail").join("settings.json"))
}

pub fn load() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path().ok_or_else(|| io::Error::other("LOCALAPPDATA not set"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    // Write to a temp file then atomically rename, so a crash mid-write can't
    // truncate settings.json into an unparseable file that reverts every setting.
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}
