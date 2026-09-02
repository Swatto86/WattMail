//! Handing a URL or file to the desktop's own handler, with an environment the
//! handler can actually survive.
//!
//! Inside an AppImage, `AppRun` exports `LD_LIBRARY_PATH` (and a pile of GTK /
//! GIO / Qt lookup paths) pointing at the bundled Ubuntu libraries, and every
//! process the app spawns inherits them. The browser then loads *our* libraries
//! instead of its own and dies before it draws a window — on Arch, Chromium
//! exits with `undefined symbol: BrotliDecoderAttachDictionary`. To the user
//! that is indistinguishable from a dead link: nothing happens.
//!
//! So anything we hand to the desktop is spawned with those variables stripped.
//! The app's own process keeps them — WebKit's network and web processes are
//! children too, and they *do* need the bundled libraries — which is why this
//! is done per-spawn and never by clearing our own environment at startup.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables `AppRun` injects that point at bundled libraries or module caches.
/// A desktop handler must see none of them.
const APPIMAGE_INJECTED: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "PERLLIB",
    "PYTHONPATH",
    "PYTHONHOME",
    "QT_PLUGIN_PATH",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
    "GTK_DATA_PREFIX",
    "GTK_EXE_PREFIX",
    "GTK_PATH",
    "GTK_IM_MODULE_FILE",
    "GTK_THEME",
    "GDK_BACKEND",
    "GDK_PIXBUF_MODULE_FILE",
    "GSETTINGS_SCHEMA_DIR",
    "GIO_EXTRA_MODULES",
    "APPDIR",
    "APPIMAGE",
    "OWD",
    "ARGV0",
];

/// Path-list variables that legitimately exist outside an AppImage but pick up
/// `$APPDIR` entries inside one. Those entries are dropped, the rest survive.
const PATH_LISTS: &[&str] = &["PATH", "XDG_DATA_DIRS", "XDG_CONFIG_DIRS"];

/// What to change about a child's environment. Empty when we are not running
/// from an AppImage, so the desktop's own environment is passed through intact.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnvPlan {
    pub remove: BTreeSet<String>,
    pub set: Vec<(String, OsString)>,
}

impl EnvPlan {
    fn apply(&self, cmd: &mut Command) {
        for key in &self.remove {
            cmd.env_remove(key);
        }
        for (key, value) in &self.set {
            cmd.env(key, value);
        }
    }
}

/// Drop `$APPDIR`-rooted entries from a `:`-separated path list. Returns `None`
/// when nothing was rooted there, so an untouched list is left alone rather
/// than rewritten (and a list that is *entirely* ours collapses to empty, which
/// is correct: the system defaults then apply).
fn strip_appdir_entries(value: &OsStr, appdir: &Path) -> Option<OsString> {
    let text = value.to_string_lossy();
    let kept: Vec<&str> = text
        .split(':')
        .filter(|entry| !entry.is_empty() && !Path::new(entry).starts_with(appdir))
        .collect();
    if kept.len() == text.split(':').filter(|e| !e.is_empty()).count() {
        return None;
    }
    Some(OsString::from(kept.join(":")))
}

/// Build the environment changes for a child spawned from inside `appdir`.
/// Pure in its inputs so it can be tested without touching the process
/// environment (which every other test in the binary shares).
pub fn plan_for(appdir: Option<&Path>, env: &[(OsString, OsString)]) -> EnvPlan {
    let Some(appdir) = appdir else {
        return EnvPlan::default();
    };
    let mut plan = EnvPlan::default();
    for name in APPIMAGE_INJECTED {
        plan.remove.insert((*name).to_string());
    }
    for (key, value) in env {
        let Some(key) = key.to_str() else { continue };
        if !PATH_LISTS.contains(&key) {
            continue;
        }
        if let Some(cleaned) = strip_appdir_entries(value, appdir) {
            plan.set.push((key.to_string(), cleaned));
        }
    }
    plan
}

/// `$APPDIR` when the running process is an extracted AppImage, else `None`.
/// Both variables are set by `AppRun`; requiring both avoids mistaking a stray
/// `APPDIR` in a developer's shell for a real bundle.
fn appimage_root() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDIR")?;
    std::env::var_os("APPIMAGE")?;
    Some(PathBuf::from(dir))
}

fn desktop_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    let snapshot: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    plan_for(appimage_root().as_deref(), &snapshot).apply(&mut cmd);
    cmd
}

/// Reject anything that is not a plain http(s) URL. The caller is email HTML,
/// so this is a trust boundary: no `file:`, no `javascript:`, and nothing that
/// a handler could read as an option instead of a URL.
pub fn checked_http_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.starts_with('-') {
        return Err("refusing a URL that looks like a command-line option".into());
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("only http(s) links open externally".into());
    }
    if url.contains(['\n', '\r', '\0']) {
        return Err("refusing a URL containing control characters".into());
    }
    Ok(url)
}

fn spawn_detached(mut cmd: Command) -> Result<(), String> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Open an http(s) URL in the user's browser.
pub fn open_url(url: &str) -> Result<(), String> {
    let url = checked_http_url(url)?;
    if cfg!(target_os = "linux") && appimage_root().is_some() {
        let mut cmd = desktop_command("xdg-open");
        cmd.arg(url);
        return spawn_detached(cmd);
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Open a local file with the desktop's default handler.
pub fn open_path(path: &Path) -> Result<(), String> {
    if cfg!(target_os = "linux") && appimage_root().is_some() {
        let mut cmd = desktop_command("xdg-open");
        cmd.arg(path);
        return spawn_detached(cmd);
    }
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    open_url(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn outside_an_appimage_the_environment_is_left_alone() {
        let plan = plan_for(None, &env(&[("LD_LIBRARY_PATH", "/opt/lib")]));
        assert_eq!(plan, EnvPlan::default());
    }

    #[test]
    fn the_bundled_library_path_is_removed() {
        // The exact failure: Chromium inherited this and died with
        // "undefined symbol: BrotliDecoderAttachDictionary".
        let plan = plan_for(
            Some(Path::new("/tmp/.mount_x")),
            &env(&[("LD_LIBRARY_PATH", "/tmp/.mount_x/usr/lib")]),
        );
        assert!(plan.remove.contains("LD_LIBRARY_PATH"));
        assert!(plan.remove.contains("GIO_EXTRA_MODULES"));
        assert!(plan.remove.contains("GDK_BACKEND"));
    }

    #[test]
    fn only_appdir_entries_are_stripped_from_path_lists() {
        let plan = plan_for(
            Some(Path::new("/tmp/.mount_x")),
            &env(&[("PATH", "/tmp/.mount_x/usr/bin:/usr/bin:/bin")]),
        );
        let path = plan
            .set
            .iter()
            .find(|(k, _)| k == "PATH")
            .expect("PATH should be rewritten");
        assert_eq!(path.1, OsString::from("/usr/bin:/bin"));
    }

    #[test]
    fn a_path_list_with_nothing_of_ours_is_not_rewritten() {
        let plan = plan_for(
            Some(Path::new("/tmp/.mount_x")),
            &env(&[("PATH", "/usr/bin:/bin")]),
        );
        assert!(plan.set.iter().all(|(k, _)| k != "PATH"));
    }

    #[test]
    fn non_http_schemes_are_refused() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "--version",
            "ftp://example.com",
        ] {
            assert!(checked_http_url(bad).is_err(), "should refuse {bad}");
        }
        assert!(checked_http_url("https://example.com/a?b=c").is_ok());
        assert!(checked_http_url("HTTP://Example.com").is_ok());
    }

    /// The wiring, not just the plan: spawn a real child through the same
    /// builder the browser goes through and read the environment it actually
    /// received. A plan that is correct but never applied would still pass the
    /// tests above.
    #[test]
    fn a_spawned_child_sees_the_cleaned_environment() {
        let appdir = Path::new("/tmp/.mount_test");
        let poisoned = env(&[
            ("LD_LIBRARY_PATH", "/tmp/.mount_test/usr/lib"),
            ("PATH", "/tmp/.mount_test/usr/bin:/usr/bin:/bin"),
        ]);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf '%s|%s' \"$LD_LIBRARY_PATH\" \"$PATH\"");
        // Seed the child with the poisoned values first, exactly as AppRun does.
        for (k, v) in &poisoned {
            cmd.env(k, v);
        }
        plan_for(Some(appdir), &poisoned).apply(&mut cmd);
        let out = cmd.output().expect("sh should run");
        let seen = String::from_utf8_lossy(&out.stdout);
        let (ld, path) = seen.split_once('|').expect("child should print both");
        assert_eq!(ld, "", "child still saw the bundled library path");
        assert_eq!(path, "/usr/bin:/bin", "child still saw the bundled bin dir");
    }
}
