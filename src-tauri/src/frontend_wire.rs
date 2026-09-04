//! Source-wiring contracts for frontend behaviours that have no TS test harness.

#[cfg(test)]
mod tests {
    /// Launch must download and install a found update, not only show the banner.
    /// The 2026-09-04 ask: check + install automatically when the app starts.
    #[test]
    fn launch_update_check_auto_installs() {
        let src = include_str!("../../src/main.ts");
        let start = src
            .find("async function checkForUpdates()")
            .expect("scan must see checkForUpdates before asserting install");
        // Bound the function body so a later unrelated downloadAndInstall can't
        // satisfy the assertion (About / Install button still call it too).
        let rest = &src[start..];
        let end = rest[1..]
            .find("\nasync function ")
            .map(|i| i + 1)
            .unwrap_or(rest.len().min(1200));
        let body = &rest[..end];
        assert!(
            body.contains("installUpdate") || body.contains("downloadAndInstall"),
            "checkForUpdates must install on launch, not only reveal the banner"
        );
        // Known-present control: boot still kicks off the silent launch check.
        assert!(
            src.contains("void checkForUpdates()"),
            "boot must still call checkForUpdates"
        );
    }
}
