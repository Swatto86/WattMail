import { invoke } from "@tauri-apps/api/core";

/** OS toast via the Rust command that runs notify-rust off the Tokio worker. */
export function showDesktopNotification(title: string, body: string): void {
  void invoke("show_desktop_notification", { title, body }).catch(() => {});
}
