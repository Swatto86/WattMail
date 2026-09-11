//! User settings, persisted to `settings.json` in WattMail's per-user data dir
//! (see [`crate::paths::data_dir`]).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Per-folder colour and pin. Keyed as `{accountId}:{folderId}` so switching
/// mailboxes never shares one account's colours/pins with another.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct FolderPref {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

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
    /// Sidebar colour / pin for each folder. Empty map omitted from disk.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub folder_prefs: HashMap<String, FolderPref>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            notifications_enabled: true,
            signature: String::new(),
            folder_prefs: HashMap::new(),
        }
    }
}

/// Accept only `#rrggbb` so a stored value can be dropped into CSS safely.
pub fn normalize_folder_color(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        Some(trimmed.to_ascii_lowercase())
    } else {
        None
    }
}

impl Settings {
    /// Insert or clear one folder pref. A row with no colour and not pinned is
    /// removed so `settings.json` does not accumulate empty entries.
    pub fn upsert_folder_pref(&mut self, key: String, color: Option<String>, pinned: bool) {
        let color = color.as_deref().and_then(normalize_folder_color);
        if color.is_none() && !pinned {
            self.folder_prefs.remove(&key);
            return;
        }
        self.folder_prefs.insert(key, FolderPref { color, pinned });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_json_loads_without_folder_prefs() {
        let s: Settings = serde_json::from_str(
            r#"{"closeToTray":false,"notificationsEnabled":true,"signature":"Hi"}"#,
        )
        .unwrap();
        assert!(!s.close_to_tray);
        assert_eq!(s.signature, "Hi");
        assert!(s.folder_prefs.is_empty());
    }

    #[test]
    fn folder_prefs_round_trip() {
        let mut s = Settings::default();
        s.upsert_folder_pref("acc:inbox".into(), Some("#3B82F6".into()), true);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("folderPrefs"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        let pref = &back.folder_prefs["acc:inbox"];
        assert_eq!(pref.color.as_deref(), Some("#3b82f6"));
        assert!(pref.pinned);
    }

    #[test]
    fn upsert_clears_empty_pref() {
        let mut s = Settings::default();
        s.upsert_folder_pref("x".into(), Some("#ef4444".into()), false);
        assert_eq!(s.folder_prefs.len(), 1);
        s.upsert_folder_pref("x".into(), None, false);
        assert!(s.folder_prefs.is_empty());
    }

    #[test]
    fn rejects_non_hex_color() {
        let mut s = Settings::default();
        s.upsert_folder_pref("x".into(), Some("red".into()), true);
        assert_eq!(s.folder_prefs["x"].color, None);
        assert!(s.folder_prefs["x"].pinned);
        s.upsert_folder_pref("y".into(), Some("javascript:alert(1)".into()), false);
        assert!(!s.folder_prefs.contains_key("y"));
    }
}
