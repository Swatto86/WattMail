//! Cross-platform per-user data directory for WattMail.
//!
//! Resolves the OS-conventional local-data location and appends `WattMail`:
//! - Windows: `%LOCALAPPDATA%\WattMail`
//! - macOS:   `~/Library/Application Support/WattMail`
//! - Linux:   `$XDG_DATA_HOME/WattMail` (or `~/.local/share/WattMail`)
//!
//! On Windows `dirs::data_local_dir()` returns `%LOCALAPPDATA%`, so the path is
//! identical to the previous hand-rolled one — existing caches and settings are
//! found in place, no migration needed. The temp-dir fallback only fires if the
//! platform can't resolve a home/data dir at all (effectively never on desktop).

use std::path::PathBuf;

/// WattMail's per-user data directory, created on demand by callers.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("WattMail")
}
