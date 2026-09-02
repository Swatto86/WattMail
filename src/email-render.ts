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
export function wrapEmailHtml(inner: string, opts: WrapOpts): string {
  const bg = opts.adapt ? rgbToCss(opts.bg) : "#ffffff";
  const fg = opts.adapt ? rgbToCss(opts.fg) : "#1a1a1a";
  const link = opts.adapt ? rgbToCss(liftToContrast({ r: 37, g: 99, b: 235 }, opts.bg, ACCENT_CONTRAST)) : "#2563eb";
  const quote = opts.adapt
    ? `border-left: 3px solid ${rgbToCss(opts.fg)}; opacity: 0.85;`
    : "border-left: 3px solid #cbd5e1; color: #475569;";
  return `<!doctype html><html><head><meta charset="utf-8" />
<meta name="referrer" content="no-referrer" />
<style>
  html, body { margin: 0; }
  body {
    padding: 16px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 14px; line-height: 1.55; color: ${fg}; background: ${bg};
    word-wrap: break-word; overflow-wrap: anywhere;
  }
  a { color: ${link}; cursor: pointer; }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  pre { white-space: pre-wrap; }
  blockquote { margin: 0 0 0 12px; padding-left: 12px; ${quote} }
</style></head><body>${inner}</body></html>`;
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

function isVisiblyEmptyAnchor(a: Element): boolean {
  if ((a.textContent ?? "").trim()) return false;
  for (let i = 0; i < a.children.length; i++) {
    const child = a.children[i];
    if ((child.textContent ?? "").trim()) return false;
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
    const emptyWrapper = !(sib.textContent ?? "").trim() && anchors.length === 1 && isVisiblyEmptyAnchor(anchors[0]);
    if (emptyWrapper) {
      const href = hrefAttr(anchors[0]);
      if (href) return href;
    }
    if ((sib.textContent ?? "").trim()) break;
    sib = sib.previousElementSibling;
  }
  return null;
}

/** Raw href of the email link the user clicked or right-clicked, if any. */
export function hrefFromEmailEvent(ev: Event): string | null {
  const el = eventElement(ev.target);
  if (!el) return null;

  const direct = el.closest("a");
  const fromDirect = hrefAttr(direct);
  if (fromDirect && direct && !isVisiblyEmptyAnchor(direct)) return fromDirect;

  if (el.tagName === "AREA") {
    const href = hrefAttr(el);
    if (href) return href;
  }

  const cell = el.closest("td, th");
  if (cell) {
    const links = cell.querySelectorAll("a[href]");
    const real = Array.from(links).filter((a) => !isVisiblyEmptyAnchor(a));
    const pick = real.length === 1 ? real[0] : links.length === 1 ? links[0] : null;
    const href = hrefAttr(pick);
    if (href) return href;
  }

  let block: Element | null = el.closest("table") ?? el.closest("div");
  const body = el.ownerDocument?.body ?? null;
  while (block && block !== body) {
    const href = hrefFromCollapsedPrecedingAnchor(block);
    if (href) return href;
    const parent = block.parentElement;
    if (!parent || parent === body) break;
    block = parent.closest("table") ?? parent.closest("div");
  }

  return fromDirect;
}

/** http(s) URL the reading pane may open in the system browser. */
export function safeExternalHref(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const href = raw.trim();
  if (/^https?:\/\//i.test(href)) return href;
  if (href.startsWith("//") && href.length > 2) return `https:${href}`;
  return null;
}

/** Cursor hint on table-cell buttons whose <a> only wraps the label. */
export function enhanceEmailButtons(doc: Document): void {
  const cells = doc.querySelectorAll("td, th");
  for (let i = 0; i < cells.length; i++) {
    const cell = cells[i];
    const links = cell.querySelectorAll("a[href]");
    const real = Array.from(links).filter((a) => !isVisiblyEmptyAnchor(a) && hrefAttr(a));
    if (real.length === 1) (cell as HTMLElement).style.cursor = "pointer";
  }
}
