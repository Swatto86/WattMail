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

    /// Date-range control defaults to last 7 days and can widen; list coverage
    /// expands until the window reaches the cutoff.
    #[test]
    fn mail_list_date_range_defaults_to_seven_days() {
        let main = include_str!("../../src/main.ts");
        let helpers = include_str!("../../src/date-range.ts");
        assert!(
            main.contains("id=\"range\""),
            "toolbar must expose the date-range select"
        );
        assert!(
            main.contains("Last 7 days"),
            "default option label must be Last 7 days"
        );
        assert!(
            main.contains("loadMailboxRange") && main.contains("list_messages_since"),
            "finite ranges must load mailbox-wide via list_messages_since"
        );
        assert!(
            main.contains("rangeDays > 0 && filterMode === \"unread\" ? \"all\""),
            "finite date ranges must include both read and unread mail"
        );
        assert!(
            helpers.contains("parseRangeDays") && helpers.contains(": 7"),
            "missing/invalid stored range must fall back to 7 days"
        );
        assert!(
            helpers.contains("rangeSinceIso"),
            "cutoff must be an ISO timestamp for Graph $filter"
        );
    }
}
