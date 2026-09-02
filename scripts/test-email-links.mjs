#!/usr/bin/env node
// Node tests for reading-pane button-link hit testing and the srcdoc shim.
// Uses linkedom (no browser) so it can run in CI/headless VMs without Chrome.
import { spawnSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dir = mkdtempSync(join(tmpdir(), "wattmail-email-links-"));
const bundle = join(dir, "email-render.js");

const esbuild = spawnSync(
  "npx",
  ["--yes", "esbuild", join(root, "src/email-render.ts"), "--bundle", "--format=esm", `--outfile=${bundle}`],
  { cwd: root, encoding: "utf8" },
);
if (esbuild.status !== 0) {
  console.error(esbuild.stderr || esbuild.stdout);
  process.exit(esbuild.status ?? 1);
}

const linkedomInstall = spawnSync("npm", ["install", "--no-save", "--prefix", dir, "linkedom@0.18.12"], {
  encoding: "utf8",
});
if (linkedomInstall.status !== 0) {
  console.error(linkedomInstall.stderr || linkedomInstall.stdout);
  process.exit(linkedomInstall.status ?? 1);
}

const require = createRequire(join(dir, "package.json"));
const { parseHTML } = require("linkedom");
const { window, document } = parseHTML("<!doctype html><html><body></body></html>");
globalThis.window = window;
globalThis.document = document;
globalThis.Element = window.Element;
globalThis.HTMLElement = window.HTMLElement;
globalThis.Node = window.Node;
globalThis.Event = window.Event;
if (!globalThis.crypto) {
  globalThis.crypto = await import("node:crypto");
}

const mod = await import(pathToFileURL(bundle).href);
const { wrapEmailHtml, hrefFromEmailEvent, safeExternalHref, EMAIL_FRAME_SANDBOX } = mod;

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: !!cond, detail: cond ? undefined : String(detail ?? "") });
}

function clickTarget(html, selector) {
  document.body.innerHTML = html;
  const el = document.querySelector(selector);
  if (!el) throw new Error("missing " + selector);
  return hrefFromEmailEvent({ target: el });
}

const padBtn =
  '<table><tr><td id="chrome" bgcolor="#f97316" style="padding:20px 40px">' +
  '<a href="https://example.com/confirm" style="color:#fff;text-decoration:none">Confirm Email</a>' +
  "</td></tr></table>";
check(
  "padded td chrome recovers href",
  clickTarget(padBtn, "#chrome") === "https://example.com/confirm",
  clickTarget(padBtn, "#chrome"),
);

const divBtn =
  '<div id="chrome" style="background-color:#191919;padding:12px 24px;border-radius:8px;display:inline-block">' +
  '<a href="https://claude.ai/magic-link/abc" style="color:#fff">Sign in</a></div>';
check(
  "padded div chrome recovers href",
  clickTarget(divBtn, "#chrome") === "https://claude.ai/magic-link/abc",
  clickTarget(divBtn, "#chrome"),
);

const emptyA =
  '<table><tr><td id="chrome" bgcolor="#0a66c2" style="padding:16px">' +
  '<a href="https://example.com/x">&nbsp;</a>Continue</td></tr></table>';
check(
  "nbsp-empty anchor in cell recovers href",
  clickTarget(emptyA, "#chrome") === "https://example.com/x",
  clickTarget(emptyA, "#chrome"),
);

const split =
  '<p><a href="https://example.com/split"></a></p>' +
  '<table><tr><td id="chrome" bgcolor="#111" style="padding:12px;color:#fff">Confirm Email</td></tr></table>';
check(
  "collapsed empty <a> before table recovers href",
  clickTarget(split, "#chrome") === "https://example.com/split",
  clickTarget(split, "#chrome"),
);

const mixed =
  '<table><tr><td id="prose">Please confirm your account.' +
  '<table><tr><td bgcolor="#f97316" style="padding:12px"><a href="https://example.com/btn">Confirm</a></td></tr></table>' +
  "</td></tr></table>";
check(
  "prose in the same outer cell does not steal the button href",
  clickTarget(mixed, "#prose") === null,
  clickTarget(mixed, "#prose"),
);

check(
  "plain text link still works",
  clickTarget('<p>See <a id="t" href="https://example.com/plain">here</a>.</p>', "#t") ===
    "https://example.com/plain",
);

check("javascript href is rejected", safeExternalHref("javascript:alert(1)") === null);
check("https href is accepted", safeExternalHref("https://example.com/x") === "https://example.com/x");
check("sandbox includes allow-scripts for WebKit", EMAIL_FRAME_SANDBOX.includes("allow-scripts"));

const wrapped = wrapEmailHtml("<p>hi</p>" + padBtn, {
  adapt: false,
  bg: { r: 255, g: 255, b: 255 },
  fg: { r: 0, g: 0, b: 0 },
});
check("wrapper injects CSP meta", wrapped.includes('http-equiv="Content-Security-Policy"'));
check("wrapper injects click shim", wrapped.includes('postMessage({t:"wattmail-frame"'));
check("wrapper CSP allows only nonce scripts", /script-src 'nonce-[0-9a-f]{32}'/.test(wrapped));
check("wrapper CSP forbids default loads", wrapped.includes("default-src 'none'"));
const nonce = wrapped.match(/script-src 'nonce-([0-9a-f]+)'/)?.[1];
check("shim script carries matching nonce", !!(nonce && wrapped.includes(`<script nonce="${nonce}">`)));

const failed = results.filter((r) => !r.ok);
if (failed.length) {
  console.error("FAIL");
  for (const f of failed) console.error(" -", f.name, f.detail ?? "");
  process.exit(1);
}
console.log(`email-link tests: PASS (${results.length})`);
