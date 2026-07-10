//! User settings, persisted to `settings.json` in WattMail's per-user data dir
//! (see [`crate::paths::data_dir`]).

use std::io;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Show a native OS notification when new unread mail arrives.
    pub notifications_enabled: bool,
    /// Plain-text signature appended to new messages, replies, and forwards.
    /// Empty = no signature. Converted to HTML (escaped, line breaks) at insert.
    // ponytail: plain text + one global signature; per-account rich-HTML
    // signatures if multi-account use ever demands them.
    pub signature: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            notifications_enabled: true,
            signature: String::new(),
        }
    }
}

/// Tauri-managed settings, shared with the window-close handler.
pub struct SettingsState(pub RwLock<Settings>);

fn settings_path() -> PathBuf {
    crate::paths::data_dir().join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read(settings_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> io::Result<()> {
    let path = settings_path();
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
