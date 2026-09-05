//! "Start at login" guard-rails.
//!
//! `tauri-plugin-autostart` writes the *running executable's* path into the
//! login entry (`~/.config/autostart/WattMail.desktop` on Linux, the `Run`
//! registry key on Windows). Enabling it while a debug build ran from
//! Downloads therefore pinned login to that throwaway binary. The toggle now
//! refuses unless this process is the installed app, and start-up repairs a
//! Linux entry that points somewhere other than the installed AppImage.

use std::path::{Component, Path, PathBuf};

use tauri::{AppHandle, Runtime};
use tauri_plugin_autostart::ManagerExt;

/// The path the login entry would launch, or why one must not be written now.
pub fn launch_target() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this executable: {e}"))?;
    let appimage = if cfg!(target_os = "linux") {
        std::env::var_os("APPIMAGE").map(PathBuf::from)
    } else {
        None
    };
    classify(
        &exe,
        appimage.as_deref(),
        dirs::download_dir().as_deref(),
        &std::env::temp_dir(),
    )
}

/// Pure decision: `exe` is the running binary, `appimage` the `$APPIMAGE` the
/// AppImage runtime sets (Linux), `downloads` and `temp` the user's folders.
fn classify(
    exe: &Path,
    appimage: Option<&Path>,
    downloads: Option<&Path>,
    temp: &Path,
) -> Result<PathBuf, String> {
    let target = match appimage {
        Some(appimage) => appimage.to_path_buf(),
        None if cfg!(target_os = "linux") => {
            return Err(format!(
                "this instance is a bare binary ({}), not the installed AppImage. \
                 Start at login would point at it. Launch the installed WattMail.AppImage \
                 and enable it there.",
                exe.display()
            ));
        }
        None => exe.to_path_buf(),
    };
    if downloads.is_some_and(|d| target.starts_with(d)) {
        return Err(format!(
            "this instance runs from your Downloads folder ({}), not an installed \
             location. Install it first, then enable Start at login.",
            target.display()
        ));
    }
    if target.starts_with(temp) {
        return Err(format!(
            "this instance runs from a temporary folder ({}). Install it first, then \
             enable Start at login.",
            target.display()
        ));
    }
    if is_cargo_build(&target) {
        return Err(format!(
            "this instance is a development build ({}). Start at login only works from \
             the installed app.",
            target.display()
        ));
    }
    Ok(target)
}

/// A binary under a cargo `target/{debug,release,…}` tree (`npm run tauri dev`).
fn is_cargo_build(path: &Path) -> bool {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && matches!(w[1], "debug" | "release" | "debugging"))
}

/// Whether the login entry currently exists.
pub fn is_enabled<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Write or remove the login entry. Writing is refused unless this process is
/// the installed app, so a throwaway binary can never become the login target.
pub fn set_enabled<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    if enabled {
        launch_target()?;
        launcher.enable().map_err(|e| e.to_string())
    } else {
        launcher.disable().map_err(|e| e.to_string())
    }
}

/// Start-up repair (Linux): if a login entry exists but runs something other
/// than this installed AppImage, rewrite it. Running from a non-installed
/// location never rewrites — that is exactly how the entry went wrong.
pub fn repair_login_entry<R: Runtime>(app: &AppHandle<R>) {
    let Some(entry) = login_entry_path() else {
        return;
    };
    let Ok(contents) = std::fs::read_to_string(&entry) else {
        return;
    };
    let Ok(target) = launch_target() else {
        return;
    };
    if exec_path(&contents).is_some_and(|current| current == target) {
        return;
    }
    match app.autolaunch().enable() {
        Ok(()) => eprintln!(
            "WattMail: rewrote the start-at-login entry to {}",
            target.display()
        ),
        Err(e) => eprintln!("WattMail: could not repair the start-at-login entry: {e}"),
    }
}

/// Where `auto-launch` writes the desktop entry on Linux; `None` elsewhere.
fn login_entry_path() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("autostart")
            .join("WattMail.desktop"),
    )
}

/// The executable named by a desktop entry's `Exec=` line (its first token —
/// `auto-launch` writes the path unquoted, followed by the `--hidden` flag).
fn exec_path(desktop_entry: &str) -> Option<PathBuf> {
    desktop_entry
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))
        .and_then(|exec| exec.split_whitespace().next())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DL: &str = "/home/swatto/Downloads";
    const TMP: &str = "/tmp";

    fn classify_linux(exe: &str, appimage: Option<&str>) -> Result<PathBuf, String> {
        classify(
            Path::new(exe),
            appimage.map(Path::new),
            Some(Path::new(DL)),
            Path::new(TMP),
        )
    }

    #[test]
    fn the_installed_appimage_is_the_launch_target() {
        // Inside an AppImage the exe is the ephemeral mount; $APPIMAGE is the file.
        let installed = "/home/swatto/.local/lib/wattmail/WattMail.AppImage";
        let target = classify_linux(
            "/tmp/.mount_WattMaXYZ/usr/bin/wattmail-desktop",
            Some(installed),
        );
        assert_eq!(target.unwrap(), PathBuf::from(installed));
    }

    #[test]
    fn a_debug_binary_in_downloads_is_refused() {
        // The exact shape of the broken entry: a bare binary run from Downloads.
        let err = classify_linux("/home/swatto/Downloads/wattmail-desktop-0.14.8", None);
        let msg = err.unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "bare binary"
        } else {
            "Downloads"
        };
        assert!(msg.contains(expected), "{msg}");
        // Even an AppImage is refused while it still sits in Downloads.
        let err = classify_linux(
            "/tmp/.mount_x/usr/bin/wattmail-desktop",
            Some("/home/swatto/Downloads/WattMail_0.15.0_amd64.AppImage"),
        );
        assert!(err.unwrap_err().contains("Downloads"));
    }

    #[test]
    fn cargo_and_temp_builds_are_refused() {
        let err = classify_linux(
            "/home/swatto/Projects/WattMail/src-tauri/target/debug/wattmail-desktop",
            Some("/home/swatto/Projects/WattMail/src-tauri/target/debug/wattmail-desktop"),
        );
        assert!(err.unwrap_err().contains("development build"));
        let err = classify_linux("/tmp/x/wattmail-desktop", Some("/tmp/x/WattMail.AppImage"));
        assert!(err.unwrap_err().contains("temporary folder"));
        assert!(is_cargo_build(Path::new(
            "C:/src/WattMail/src-tauri/target/release/wattmail-desktop.exe"
        )));
        assert!(!is_cargo_build(Path::new(
            "/home/swatto/target/WattMail.AppImage"
        )));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn an_installed_exe_is_its_own_launch_target() {
        let exe = Path::new("C:/Users/swatto/AppData/Local/WattMail/WattMail.exe");
        let target = classify(
            exe,
            None,
            Some(Path::new("C:/Users/swatto/Downloads")),
            Path::new("C:/Temp"),
        );
        assert_eq!(target.unwrap(), exe);
    }

    #[test]
    fn exec_path_reads_the_binary_from_the_desktop_entry() {
        let entry = "[Desktop Entry]\nType=Application\nVersion=1.0\nName=WattMail\n\
                     Comment=WattMailstartup script\n\
                     Exec=/home/swatto/Downloads/wattmail-desktop-0.14.8 --hidden\n\
                     StartupNotify=false\nTerminal=false";
        assert_eq!(
            exec_path(entry),
            Some(PathBuf::from(
                "/home/swatto/Downloads/wattmail-desktop-0.14.8"
            ))
        );
        assert_eq!(exec_path("[Desktop Entry]\nName=WattMail"), None);
    }
}
