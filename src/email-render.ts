// Shared email-body rendering helpers used by both the reading pane (main.ts)
// and the pop-out message window (message.ts). The sanitization itself happens
// in Rust; these are the frontend presentation concerns: theme colour probing,
// dark-mode contrast adaptation, and the iframe document wrapper.

export type RGB = { r: number; g: number; b: number }; // 0..255

const NAMED_COLORS: Record<string, string> = {
  black: "#000000", white: "#ffffff", red: "#ff0000", green: "#008000",
  blue: "#0000ff", gray: "#808080", grey: "#808080", silver: "#c0c0c0",
  navy: "#000080", maroon: "#800000", purple: "#800080", teal: "#008080",
  olive: "#808000", lime: "#00ff00", aqua: "#00ffff", cyan: "#00ffff",
  fuchsia: "#ff00ff", magenta: "#ff00ff", yellow: "#ffff00", orange: "#ffa500",
  transparent: "", inherit: "", currentcolor: "", initial: "", unset: "",
};

const RGB_RE = /^rgba?\(\s*([\d.]+)[ ,]+([\d.]+)[ ,]+([\d.]+)(?:[ ,/]+([\d.]+%?))?\s*\)$/;

export function parseColor(raw: string): RGB | null {
  if (!raw) return null;
  let v = raw.trim().toLowerCase();
  if (v in NAMED_COLORS) {
    const m = NAMED_COLORS[v];
    if (!m) return null;
    v = m;
  }
  if (v[0] === "#") {
    if (v.length === 4) v = "#" + v[1] + v[1] + v[2] + v[2] + v[3] + v[3];
    if (v.length === 7) {
      const n = parseInt(v.slice(1), 16);
      if (Number.isNaN(n)) return null;
      return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
    }
    return null;
  }
  const m = v.match(RGB_RE);
  if (m) {
    const a = m[4] == null ? 1 : m[4].endsWith("%") ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
    if (a === 0) return null; // fully transparent == no colour
    return { r: +m[1], g: +m[2], b: +m[3] };
  }
  return null; // hsl()/oklch()/keywords: skip safely
}

export function relLuminance(c: RGB): number {
  const lin = (x: number) => {
    x /= 255;
    return x <= 0.04045 ? x / 12.92 : Math.pow((x + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
}

export function contrast(a: RGB, b: RGB): number {
  const la = relLuminance(a), lb = relLuminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

function rgbToHsl(c: RGB): { h: number; s: number; l: number } {
  const r = c.r / 255, g = c.g / 255, b = c.b / 255;
  const mx = Math.max(r, g, b), mn = Math.min(r, g, b);
  let h = 0, s = 0;
  const l = (mx + mn) / 2;
  if (mx !== mn) {
    const d = mx - mn;
    s = l > 0.5 ? d / (2 - mx - mn) : d / (mx + mn);
    h = mx === r ? (g - b) / d + (g < b ? 6 : 0) : mx === g ? (b - r) / d + 2 : (r - g) / d + 4;
    h /= 6;
  }
  return { h, s, l };
}

function hslToRgb(h: number, s: number, l: number): RGB {
  const f = (n: number) => {
    const k = (n + h * 12) % 12;
    const a = s * Math.min(l, 1 - l);
    return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
  };
  return { r: Math.round(f(0) * 255), g: Math.round(f(8) * 255), b: Math.round(f(4) * 255) };
}

export function rgbToCss(c: RGB): string {
  return `rgb(${c.r}, ${c.g}, ${c.b})`;
}

export function isNearNeutral(c: RGB): boolean {
  return rgbToHsl(c).s < 0.15; // chroma proxy: plain black/grey
}

// Contrast vs a dark background is monotonic in HSL lightness, so binary-search
// the *minimum* lightness (same hue/saturation) that clears the target — keeps
// the author's hue while making the colour readable.
export function liftToContrast(fg: RGB, bg: RGB, target: number): RGB {
  const { h, s, l } = rgbToHsl(fg);
  let lo = l, hi = 1;
  let best = hslToRgb(h, s, 1);
  for (let i = 0; i < 12; i++) {
    const mid = (lo + hi) / 2;
    const cand = hslToRgb(h, s, mid);
    if (contrast(cand, bg) >= target) {
      best = cand;
      hi = mid;
    } else {
      lo = mid;
    }
  }
  return best;
}

// Resolve the active DaisyUI theme tokens to concrete rgb in the PARENT (the
// iframe has no DaisyUI tokens). A throwaway probe span lets the browser resolve
// oklch(var(--bc))/oklch(var(--b1)) for us.
export function readThemeColors(): { bg: RGB; fg: RGB } {
  const p = document.createElement("span");
  p.style.cssText =
    "color:oklch(var(--bc));background:oklch(var(--b1));position:absolute;left:-9999px";
  document.body.appendChild(p);
  const cs = getComputedStyle(p);
  const fg = parseColor(cs.color) ?? { r: 230, g: 230, b: 230 };
  const bg = parseColor(cs.backgroundColor) ?? { r: 24, g: 24, b: 27 };
  p.remove();
  return { bg, fg };
}

const BODY_CONTRAST = 4.5; // WCAG AA, normal text
const ACCENT_CONTRAST = 3.0; // WCAG AA, large text / UI: leave a brand colour that already clears this
const NODE_CAP = 4000;
const SKIP_TAGS = new Set(["IMG", "PICTURE", "SVG", "CANVAS", "VIDEO", "OBJECT", "EMBED", "IFRAME"]);

// Repair author text colours inside a same-origin frame. Run on the frame
// 'load' event, only for (plain email AND dark theme).
export function adaptPlainEmail(frame: HTMLIFrameElement, theme: { bg: RGB; fg: RGB }): void {
  const doc = frame.contentDocument;
  if (!doc) return;

  // First explicit, non-transparent ancestor background; else the dark card.
  const localBg = (el: Element): RGB => {
    let n: Element | null = el;
    while (n && n !== doc.body) {
      const h = n as HTMLElement;
      // Also honour the legacy `bgcolor` attribute: the browser paints the cell
      // from it, but it never lands in `style.backgroundColor`, so without this a
      // white bgcolor cell reads as the dark theme surface and its dark text gets
      // relit to light — invisible on the cell's real white background.
      const c =
        parseColor(h.style?.backgroundColor) ??
        parseColor(h.style?.background ?? "") ??
        parseColor(h.getAttribute?.("bgcolor") ?? "");
      if (c) return c;
      n = n.parentElement;
    }
    return theme.bg;
  };

  const styled = doc.querySelectorAll<HTMLElement>("[style], font[color]");
  const limit = Math.min(styled.length, NODE_CAP);
  for (let i = 0; i < limit; i++) {
    const el = styled[i];
    if (SKIP_TAGS.has(el.tagName)) continue;

    // Light local cell (e.g. a highlight): keep the author's dark text intact.
    if (relLuminance(localBg(el)) > 0.5) continue;

    const raw = el.style.color || (el.tagName === "FONT" ? el.getAttribute("color") ?? "" : "");
    const authored = parseColor(raw);
    if (!authored) continue;

    const isLink = el.closest("a") != null;
    const gate = isLink ? ACCENT_CONTRAST : BODY_CONTRAST;
    if (contrast(authored, theme.bg) >= gate) continue; // already readable + intentional: keep

    if (!isLink && isNearNeutral(authored)) {
      // Plain black/grey body text: inherit the theme foreground.
      el.style.removeProperty("color");
      if (el.tagName === "FONT") el.removeAttribute("color");
    } else {
      // Chromatic accent or link: preserve hue, lift lightness until it clears.
      el.style.color = rgbToCss(liftToContrast(authored, theme.bg, gate));
      if (el.tagName === "FONT") el.removeAttribute("color");
    }
  }
  // Dark local author backgrounds are left to blend into the dark card.
}

export type WrapOpts = { adapt: boolean; bg: RGB; fg: RGB };

// Wrap a sanitized body for the reading-pane iframe. Plain text in light mode
// (and every designed email) renders on the light "paper" card — the
// conservative default that matches the author's light-background assumption.
// Plain mail in dark mode renders on the theme surface (adaptPlainEmail then
// repairs author text colours).
// WebKitGTK (Linux Tauri) treats a parent-attached listener on a sandboxed
// srcdoc document as iframe script and drops it unless `allow-scripts` is set
// (WebKit bug 218086). Email JS still cannot run: ammonia strips `<script>`,
// and wrapEmailHtml's CSP allows only a nonce shim that postMessages clicks
// to the parent. `allow-same-origin` stays so the parent can hit-test and
// adapt colours; `allow-modals` is for print().
export const EMAIL_FRAME_SANDBOX = "allow-same-origin allow-modals allow-scripts";

const FRAME_MSG = "wattmail-frame";

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  let out = "";
  for (let i = 0; i < bytes.length; i++) out += bytes[i].toString(16).padStart(2, "0");
  return out;
}

function cspAndClickShim(): string {
  const nonce = randomNonce();
  const csp =
    `default-src 'none'; img-src data:; style-src 'unsafe-inline'; ` +
    `script-src 'nonce-${nonce}'; base-uri 'none'; form-action 'none'; object-src 'none'`;
  // Capture-phase preventDefault so a javascript: href cannot run even with
  // allow-scripts. Coordinates are iframe-viewport; the parent hit-tests.
  const shim =
    `<script nonce="${nonce}">(function(){` +
    `function s(k,e){try{e.preventDefault()}catch(_){}` +
    `parent.postMessage({t:"${FRAME_MSG}",kind:k,x:e.clientX,y:e.clientY},"*")}` +
    `document.addEventListener("click",function(e){s("click",e)},true);` +
    `document.addEventListener("contextmenu",function(e){s("contextmenu",e)},true);` +
    `document.addEventListener("keydown",function(e){` +
    `if(e.key==="Escape")parent.postMessage({t:"${FRAME_MSG}",kind:"escape"},"*")});` +
    `})();</script>`;
  return `<meta http-equiv="Content-Security-Policy" content="${csp}" />${shim}`;
}

export function wrapEmailHtml(inner: string, opts: WrapOpts): string {
  const bg = opts.adapt ? rgbToCss(opts.bg) : "#ffffff";
  const fg = opts.adapt ? rgbToCss(opts.fg) : "#1a1a1a";
  const link = opts.adapt ? rgbToCss(liftToContrast({ r: 37, g: 99, b: 235 }, opts.bg, ACCENT_CONTRAST)) : "#2563eb";
  const quote = opts.adapt
    ? `border-left: 3px solid ${rgbToCss(opts.fg)}; opacity: 0.85;`
    : "border-left: 3px solid #cbd5e1; color: #475569;";
  return `<!doctype html><html><head><meta charset="utf-8" />
<meta name="referrer" content="no-referrer" />
${cspAndClickShim()}
<style>
  html, body { margin: 0; }
  body {
    padding: 16px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 14px; line-height: 1.55; color: ${fg}; background: ${bg};
    word-wrap: break-word; overflow-wrap: anywhere;
  }
  a { color: ${link}; cursor: pointer; }
  td:has(> a[href]), th:has(> a[href]) { cursor: pointer; }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  pre { white-space: pre-wrap; }
  blockquote { margin: 0 0 0 12px; padding-left: 12px; ${quote} }
</style></head><body>${inner}</body></html>`;
}

/** Head extras (CSP + click shim) for other sandboxed srcdoc frames (calendar). */
export function sandboxedSrcdocHead(extraCss: string): string {
  return `<meta charset="utf-8" /><meta name="referrer" content="no-referrer" />${cspAndClickShim()}<style>${extraCss}</style>`;
}

// ---- Reading-pane link hits ----
// Email "buttons" are often a padded <td> whose <a> only wraps the label, or a
// <p><a href><table>…</table></a></p> that HTML5 splits into an empty <a> plus
// an unlinked visual. The click/contextmenu handlers must recover the href from
// that chrome, not only from event.target.closest("a").
//
// Do not use `instanceof Element` here: the target lives in the iframe's realm,
// so a parent-window instanceof check is always false.

const ELEMENT_NODE = 1;

function eventElement(target: EventTarget | null): Element | null {
  if (!target || typeof (target as Node).nodeType !== "number") return null;
  const node = target as Node;
  if (node.nodeType === ELEMENT_NODE) return node as Element;
  return node.parentElement;
}

function visibleText(el: Element): string {
  return (el.textContent ?? "").replace(/\u00a0/g, " ").replace(/\s+/g, " ").trim();
}

function isVisiblyEmptyAnchor(a: Element): boolean {
  if (visibleText(a)) return false;
  for (let i = 0; i < a.children.length; i++) {
    const child = a.children[i];
    if (child.tagName === "IMG" || child.tagName === "SVG") return false;
  }
  return true;
}

function hrefAttr(el: Element | null | undefined): string | null {
  if (!el) return null;
  const href = (el.getAttribute("href") ?? "").trim();
  if (!href) return null;
  const lower = href.toLowerCase();
  if (lower.startsWith("javascript:") || lower.startsWith("data:") || lower.startsWith("vbscript:")) {
    return null;
  }
  return href;
}

function hrefFromCollapsedPrecedingAnchor(block: Element): string | null {
  let sib: Element | null = block.previousElementSibling;
  for (let hops = 0; sib && hops < 3; hops++) {
    if (sib.tagName === "A") {
      const href = hrefAttr(sib);
      if (href && isVisiblyEmptyAnchor(sib)) return href;
      break;
    }
    const anchors = sib.querySelectorAll("a[href]");
    const emptyWrapper =
      !visibleText(sib) && anchors.length === 1 && isVisiblyEmptyAnchor(anchors[0]);
    if (emptyWrapper) {
      const href = hrefAttr(anchors[0]);
      if (href) return href;
    }
    if (visibleText(sib)) break;
    sib = sib.previousElementSibling;
  }
  return null;
}

function isButtonLike(el: Element): boolean {
  const tag = el.tagName;
  if (tag === "TD" || tag === "TH") return true;
  const html = el as HTMLElement;
  if (html.getAttribute?.("bgcolor")) return true;
  const style = `${html.getAttribute?.("style") ?? ""};${html.style?.cssText ?? ""}`.toLowerCase();
  const hasBg = style.includes("background");
  const hasPad = style.includes("padding");
  const hasRadius = style.includes("border-radius");
  const inlineBlock = style.includes("display:inline-block") || style.includes("display:block");
  return (hasBg && (hasPad || hasRadius || inlineBlock)) || (hasPad && hasRadius);
}

function uniqueHrefAnchor(el: Element): Element | null {
  const links = el.querySelectorAll("a[href]");
  let found: Element | null = null;
  for (let i = 0; i < links.length; i++) {
    if (!hrefAttr(links[i])) continue;
    if (found) return null;
    found = links[i];
  }
  return found;
}

/** Raw href of the email link the user clicked or right-clicked, if any. */
export function hrefFromEmailEvent(ev: Event): string | null {
  const el = eventElement(ev.target);
  if (!el) return null;

  if (typeof ev.composedPath === "function") {
    const path = ev.composedPath();
    for (let i = 0; i < path.length; i++) {
      const node = path[i] as Node;
      if (!node || node.nodeType !== ELEMENT_NODE) continue;
      const item = node as Element;
      if (item.tagName !== "A" && item.tagName !== "AREA") continue;
      const href = hrefAttr(item);
      if (href && (item.tagName === "AREA" || !isVisiblyEmptyAnchor(item))) return href;
    }
  }

  const direct = el.closest("a");
  const fromDirect = hrefAttr(direct);
  if (fromDirect && direct && !isVisiblyEmptyAnchor(direct)) return fromDirect;

  if (el.tagName === "AREA") {
    const href = hrefAttr(el);
    if (href) return href;
  }

  let node: Element | null = el;
  const body = el.ownerDocument?.body ?? null;
  while (node && node !== body) {
    const unique = uniqueHrefAnchor(node);
    if (unique) {
      const href = hrefAttr(unique);
      if (href) {
        const nodeText = visibleText(node);
        const linkText = visibleText(unique);
        // Standalone button: the container's text IS the label (padding/chrome
        // around a single link). Extra prose in the same cell must not count.
        if (linkText && nodeText === linkText) return href;
        // Outlook padding-on-td with an empty (or &nbsp;) <a> plus sibling label.
        if (isButtonLike(node) && isVisiblyEmptyAnchor(unique) && nodeText) return href;
      }
    }
    const collapsed = hrefFromCollapsedPrecedingAnchor(node);
    if (collapsed) return collapsed;
    if (node.querySelectorAll("a[href]").length > 1) break;
    node = node.parentElement;
  }

  return fromDirect;
}

/** Hit-test an iframe-viewport point onto the same recovery path as a click. */
export function hrefFromEmailPoint(doc: Document, x: number, y: number): string | null {
  const hit = doc.elementFromPoint(x, y);
  if (!hit) return null;
  return hrefFromEmailEvent({ target: hit } as unknown as Event);
}

/** http(s) URL the reading pane may open in the system browser. */
export function safeExternalHref(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const href = raw.trim();
  if (/^https?:\/\//i.test(href)) return href;
  if (href.startsWith("//") && href.length > 2) return `https:${href}`;
  return null;
}

/** Cursor hint on button chrome whose <a> only wraps the label. */
export function enhanceEmailButtons(doc: Document): void {
  const cells = doc.querySelectorAll("td, th, div, p, span, center");
  for (let i = 0; i < cells.length; i++) {
    const cell = cells[i];
    const unique = uniqueHrefAnchor(cell);
    if (!unique || !hrefAttr(unique)) continue;
    const nodeText = visibleText(cell);
    const linkText = visibleText(unique);
    const empty = isVisiblyEmptyAnchor(unique);
    if ((linkText && nodeText === linkText) || (empty && isButtonLike(cell) && nodeText)) {
      (cell as HTMLElement).style.cursor = "pointer";
    }
  }
}

export type EmailFrameClick = {
  href: string | null;
  clientX: number;
  clientY: number;
  selected: string;
};

export type EmailFrameHandlers = {
  onClick: (ev: EmailFrameClick) => void;
  onContextMenu?: (ev: EmailFrameClick) => void;
  onEscape?: () => void;
};

type FrameMsg =
  | { kind: "click" | "contextmenu"; x: number; y: number }
  | { kind: "escape" };

function parseEmailFrameMessage(e: MessageEvent, frame: HTMLIFrameElement): FrameMsg | null {
  if (e.source !== frame.contentWindow) return null;
  const d = e.data as { t?: unknown; kind?: unknown; x?: unknown; y?: unknown } | null;
  if (!d || d.t !== FRAME_MSG) return null;
  if (d.kind === "escape") return { kind: "escape" };
  if ((d.kind === "click" || d.kind === "contextmenu") && typeof d.x === "number" && typeof d.y === "number") {
    return { kind: d.kind, x: d.x, y: d.y };
  }
  return null;
}

/** Bind the srcdoc click shim to parent handlers. One binding per iframe. */
const bridgeAborts = new WeakMap<HTMLIFrameElement, AbortController>();

export function wireEmailFrame(frame: HTMLIFrameElement, handlers: EmailFrameHandlers): void {
  bridgeAborts.get(frame)?.abort();
  const ac = new AbortController();
  bridgeAborts.set(frame, ac);
  const { signal } = ac;
  const doc = frame.contentDocument;
  if (doc) {
    try {
      enhanceEmailButtons(doc);
    } catch {
      /* WebKit can throw if the document is mid-replace */
    }
  }
  window.addEventListener(
    "message",
    (e: MessageEvent) => {
      const msg = parseEmailFrameMessage(e, frame);
      if (!msg) return;
      if (msg.kind === "escape") {
        handlers.onEscape?.();
        return;
      }
      const live = frame.contentDocument;
      const href = live ? hrefFromEmailPoint(live, msg.x, msg.y) : null;
      const selected = frame.contentWindow?.getSelection()?.toString() ?? "";
      const payload: EmailFrameClick = { href, clientX: msg.x, clientY: msg.y, selected };
      if (msg.kind === "click") handlers.onClick(payload);
      else handlers.onContextMenu?.(payload);
    },
    { signal },
  );
}
