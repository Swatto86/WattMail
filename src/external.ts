// Opening links outside the app.
//
// This does NOT use `@tauri-apps/plugin-opener` directly: inside an AppImage
// the browser must be spawned with the bundled library paths stripped, or it
// dies on load and the link silently does nothing. The Rust side owns that,
// and also re-checks the scheme — the hrefs here come from email HTML.
import { invoke } from "@tauri-apps/api/core";

export function openExternalUrl(url: string | null | undefined): void {
  if (!url) return;
  void invoke("open_external", { url }).catch((e) => {
    console.error("failed to open link externally", e);
  });
}
