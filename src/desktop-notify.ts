import { invoke } from "@tauri-apps/api/core";

/** OS toast via the Rust command that runs notify-rust off the Tokio worker. */
export function showDesktopNotification(title: string, body: string): void {
  void invoke("show_desktop_notification", { title, body }).catch(() => {});
}

/**
 * The notification plugin injects `window.Notification` → `plugin:notification|notify`,
 * which runs notify-rust on a Tokio worker and aborts on Linux. Route the Web
 * Notification constructor through the same off-thread command.
 */
if (typeof window !== "undefined") {
  const Shim = function Notification(
    this: unknown,
    title: string,
    options?: { body?: string },
  ): void {
    showDesktopNotification(title, options?.body ?? "");
  };
  (Shim as unknown as { permission: string }).permission = "granted";
  (
    Shim as unknown as { requestPermission: () => Promise<string> }
  ).requestPermission = () => Promise.resolve("granted");
  window.Notification = Shim as unknown as typeof Notification;
}
