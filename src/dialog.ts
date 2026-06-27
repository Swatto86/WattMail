// In-app modal dialogs — drop-in replacements for the browser's native
// window.alert / window.confirm / window.prompt. The native ones render as OS
// dialogs whose title is the webview origin ("tauri…"), which both looks
// unpolished and leaks the shell name. These render inside the app instead,
// reusing the existing `.overlay` / `.settings-panel` styling, so every prompt
// matches the rest of the UI and is themed.
//
// All three are async: callers `await` the result. Only one dialog shows at a
// time; opening a second resolves the first as cancelled first.

type DialogKind = "alert" | "confirm" | "prompt";

interface DialogOptions {
  title?: string;
  okLabel?: string;
  cancelLabel?: string;
  // Style the confirm/OK button as a destructive action.
  danger?: boolean;
  // prompt-only:
  defaultValue?: string;
  placeholder?: string;
}

// Lazily-built singleton DOM (created on first use, then reused).
let overlay: HTMLDivElement | null = null;
let titleEl: HTMLDivElement;
let messageEl: HTMLDivElement;
let inputEl: HTMLInputElement;
let okBtn: HTMLButtonElement;
let cancelBtn: HTMLButtonElement;

// Resolver + key handler for the dialog currently on screen, if any.
let activeResolve: ((value: string | boolean | null) => void) | null = null;
let activeKeyHandler: ((e: KeyboardEvent) => void) | null = null;

function build(): void {
  overlay = document.createElement("div");
  overlay.className = "overlay hidden";
  overlay.id = "dialog-overlay";
  overlay.innerHTML = `
    <div class="settings-panel dialog-panel" role="dialog" aria-modal="true">
      <div class="settings-title" data-role="title"></div>
      <div class="dialog-message" data-role="message"></div>
      <input class="input input-bordered input-sm dialog-input" data-role="input" autocomplete="off" />
      <div class="settings-actions" style="gap: 8px">
        <button class="btn btn-sm" data-role="cancel"></button>
        <button class="btn btn-sm btn-primary" data-role="ok"></button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  titleEl = overlay.querySelector('[data-role="title"]')!;
  messageEl = overlay.querySelector('[data-role="message"]')!;
  inputEl = overlay.querySelector('[data-role="input"]')!;
  okBtn = overlay.querySelector('[data-role="ok"]')!;
  cancelBtn = overlay.querySelector('[data-role="cancel"]')!;

  okBtn.addEventListener("click", () => settle(currentResult()));
  cancelBtn.addEventListener("click", () => settle(cancelResult()));
  // Backdrop click cancels (matches the other overlays).
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) settle(cancelResult());
  });
  inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      settle(currentResult());
    }
  });
}

let currentKind: DialogKind = "alert";

// The value a successful (OK) close resolves to, per dialog kind.
function currentResult(): string | boolean | null {
  if (currentKind === "prompt") return inputEl.value;
  if (currentKind === "confirm") return true;
  return null; // alert
}
// The value a cancelled close (Cancel / Esc / backdrop) resolves to.
function cancelResult(): string | boolean | null {
  if (currentKind === "prompt") return null;
  if (currentKind === "confirm") return false;
  return null; // alert
}

function settle(value: string | boolean | null): void {
  if (!activeResolve || !overlay) return;
  const resolve = activeResolve;
  activeResolve = null;
  overlay.classList.add("hidden");
  if (activeKeyHandler) {
    document.removeEventListener("keydown", activeKeyHandler, true);
    activeKeyHandler = null;
  }
  resolve(value);
}

function open(kind: DialogKind, message: string, opts: DialogOptions): Promise<string | boolean | null> {
  if (!overlay) build();
  // A second dialog opening while one is up cancels the first.
  if (activeResolve) settle(cancelResult());

  currentKind = kind;
  titleEl.textContent = opts.title ?? defaultTitle(kind);
  titleEl.classList.toggle("hidden", !titleEl.textContent);
  messageEl.textContent = message;
  messageEl.classList.toggle("hidden", !message);

  // Prompt input only for prompts.
  inputEl.classList.toggle("hidden", kind !== "prompt");
  inputEl.value = kind === "prompt" ? (opts.defaultValue ?? "") : "";
  inputEl.placeholder = opts.placeholder ?? "";

  // Cancel button hidden for a bare alert.
  cancelBtn.classList.toggle("hidden", kind === "alert");
  cancelBtn.textContent = opts.cancelLabel ?? "Cancel";
  okBtn.textContent = opts.okLabel ?? "OK";
  okBtn.classList.toggle("btn-error", opts.danger === true);
  okBtn.classList.toggle("btn-primary", opts.danger !== true);

  overlay!.classList.remove("hidden");

  // The dialog owns the keyboard while open: Escape cancels, Enter confirms.
  // Captured + stopped so it never leaks to the app's global shortcut/Escape
  // handlers behind it.
  activeKeyHandler = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      settle(cancelResult());
    } else if (e.key === "Enter" && document.activeElement !== inputEl) {
      e.preventDefault();
      e.stopPropagation();
      settle(currentResult());
    }
  };
  document.addEventListener("keydown", activeKeyHandler, true);

  // Focus the natural control.
  if (kind === "prompt") {
    inputEl.focus();
    inputEl.select();
  } else {
    okBtn.focus();
  }

  return new Promise((resolve) => {
    activeResolve = resolve;
  });
}

function defaultTitle(kind: DialogKind): string {
  return kind === "confirm" ? "Confirm" : kind === "prompt" ? "WattMail" : "WattMail";
}

/** True while any in-app dialog is on screen (used to suspend app shortcuts). */
export function isDialogOpen(): boolean {
  return overlay !== null && !overlay.classList.contains("hidden");
}

/** In-app replacement for window.alert. Resolves when dismissed. */
export async function showAlert(message: string, opts: DialogOptions = {}): Promise<void> {
  await open("alert", message, opts);
}

/** In-app replacement for window.confirm. Resolves true on OK, false otherwise. */
export async function showConfirm(message: string, opts: DialogOptions = {}): Promise<boolean> {
  return (await open("confirm", message, opts)) === true;
}

/**
 * In-app replacement for window.prompt. Resolves the entered string on OK, or
 * null if cancelled — matching window.prompt's contract.
 */
export async function showPrompt(message: string, opts: DialogOptions = {}): Promise<string | null> {
  const r = await open("prompt", message, opts);
  return typeof r === "string" ? r : null;
}
