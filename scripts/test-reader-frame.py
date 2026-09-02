#!/usr/bin/env python3
"""Reader-frame click regression test, run against the real Linux webview.

The reading pane renders email into a sandboxed srcdoc iframe and attaches its
link handlers from the parent document. WebKitGTK silently drops those
listeners when the frame has no `allow-scripts`, which made every link in the
reading pane unclickable on Linux while every unit test stayed green — the
behaviour only exists in the engine, so this check runs there.

It reads the sandbox value the app actually ships and the app's own CSP, then
asserts both halves of the contract in one document:

  * a click inside the frame reaches the parent handler and is preventable, and
  * a hostile email body still cannot run script, inline handlers, or
    `javascript:` hrefs.

Skips loudly when WebKitGTK's Python bindings or a display are unavailable
(CI containers); it is a required check on a Linux desktop.
"""

import json
import os
import re
import sys
from pathlib import Path

# Must be set before GTK/WebKit are imported: an offscreen WebKitGTK view
# aborts the process ("GDK is not able to create a GL context") on backends
# without OpenGL, which would fail the gate for a reason unrelated to the test.
for _var, _val in (
    ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
    ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
    ("LIBGL_ALWAYS_SOFTWARE", "1"),
    ("GDK_BACKEND", "x11"),
):
    os.environ.setdefault(_var, _val)

ROOT = Path(__file__).resolve().parent.parent


def read_sandbox() -> str:
    src = (ROOT / "src" / "email-render.ts").read_text()
    m = re.search(r'export const EMAIL_FRAME_SANDBOX\s*=\s*"([^"]+)"', src)
    if not m:
        sys.exit("could not read EMAIL_FRAME_SANDBOX from src/email-render.ts")
    value = m.group(1)
    # The scan must not be able to pass by matching too little: a value that has
    # lost same-origin access would make the whole test vacuous.
    if "allow-same-origin" not in value:
        sys.exit(f"EMAIL_FRAME_SANDBOX looks wrong: {value!r}")
    return value


# Every frame whose links are handled by a listener the parent attaches. Each
# names EMAIL_FRAME_SANDBOX exactly once, so a frame swapped back to a literal
# sandbox string drops the only mention and fails here. (main.ts also has a
# non-interactive colour-probe frame; it attaches no listeners and is exempt.)
INTERACTIVE_FRAMES = ("src/main.ts", "src/message.ts", "src/calendar.ts")


def check_frames_use_the_constant() -> None:
    for rel in INTERACTIVE_FRAMES:
        text = (ROOT / rel).read_text()
        # Assert the file is the one we think it is before concluding anything
        # from its absence of a literal: a moved frame would otherwise pass.
        if "sandbox" not in text:
            sys.exit(f"{rel} has no sandboxed frame any more — update this test")
        code = [
            line
            for line in text.splitlines()
            if "EMAIL_FRAME_SANDBOX" in line and not line.lstrip().startswith(("//", "*", "/*"))
        ]
        if not any("sandbox" in line.lower() or "import" in line for line in code):
            sys.exit(f"{rel} does not use EMAIL_FRAME_SANDBOX for its reader frame")


def read_csp() -> str:
    conf = json.loads((ROOT / "src-tauri" / "tauri.conf.json").read_text())
    csp = conf["app"]["security"]["csp"]
    if not csp or "default-src" not in csp:
        sys.exit(f"tauri.conf.json csp looks wrong: {csp!r}")
    return csp


# A hostile body: every one of these must stay inert inside the frame.
HOSTILE = (
    "<!doctype html><html><head></head><body>"
    "<scr" + "ipt>window.__evil=1;try{parent.__pwned=1;"
    'window.frameElement.removeAttribute("sandbox");}catch(e){}</scr' + "ipt>"
    '<p id="p" onclick="window.__evil2=1">hi</p>'
    '<a id="a" href="https://example.com/target">Go</a> '
    '<a id="js" href="javascript:window.__evil3=1">JS</a>'
    "</body></html>"
)

TEST_JS = """
window.__RESULT = null;
(function () {
  var f = document.getElementById('f');
  f.addEventListener('load', function () {
    setTimeout(function () {
      var r = {}, doc = f.contentDocument, win = f.contentWindow;
      r.contentDocumentAccessible = !!doc;
      if (!doc) { window.__RESULT = JSON.stringify(r); return; }
      r.emailScriptRan = !!win.__evil;
      r.parentReachedFromFrame = !!window.__pwned;
      r.sandboxIntact = f.getAttribute('sandbox') === __SANDBOX__;
      window.__fired = 0;
      doc.addEventListener('click', function (e) { window.__fired++; e.preventDefault(); }, true);
      var before = win.location.href;
      doc.getElementById('a').dispatchEvent(new win.MouseEvent('click', {bubbles: true, cancelable: true}));
      r.parentListenerFired = window.__fired > 0;
      doc.getElementById('p').dispatchEvent(new win.MouseEvent('click', {bubbles: true, cancelable: true}));
      r.inlineHandlerRan = !!win.__evil2;
      doc.getElementById('js').dispatchEvent(new win.MouseEvent('click', {bubbles: true, cancelable: true}));
      r.javascriptHrefRan = !!win.__evil3;
      setTimeout(function () {
        var after = '';
        try { after = win.location.href; } catch (e) { after = 'unreadable'; }
        r.frameNavigated = after !== before;
        window.__RESULT = JSON.stringify(r);
      }, 400);
    }, 80);
  }, {once: true});
  f.srcdoc = __BODY__;
})();
"""

EXPECTED = {
    "contentDocumentAccessible": True,
    "parentListenerFired": True,   # the regression this test exists for
    "frameNavigated": False,
    "sandboxIntact": True,
    "emailScriptRan": False,
    "parentReachedFromFrame": False,
    "inlineHandlerRan": False,
    "javascriptHrefRan": False,
}


def main() -> int:
    sandbox = read_sandbox()
    check_frames_use_the_constant()
    csp = read_csp()

    try:
        import gi

        gi.require_version("Gtk", "3.0")
        gi.require_version("WebKit2", "4.1")
        from gi.repository import GLib, Gtk, WebKit2
    except (ImportError, ValueError) as exc:
        print(f"SKIP reader-frame engine test: WebKitGTK bindings unavailable ({exc})")
        return 0
    if not (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")):
        print("SKIP reader-frame engine test: no display")
        return 0

    page = (
        "<!doctype html><html><head>"
        f'<meta http-equiv="Content-Security-Policy" content="{csp}">'
        f'</head><body><iframe id="f" sandbox="{sandbox}"></iframe></body></html>'
    )
    script = TEST_JS.replace("__BODY__", json.dumps(HOSTILE)).replace(
        "__SANDBOX__", json.dumps(sandbox)
    )

    if not Gtk.init_check()[0]:
        print("SKIP reader-frame engine test: GTK could not open a display")
        return 0
    window = Gtk.OffscreenWindow()
    view = WebKit2.WebView()
    window.add(view)
    window.show_all()
    outcome: dict = {}
    started = {"yes": False}

    def poll():
        def done(_o, res):
            try:
                text = view.run_javascript_finish(res).get_js_value().to_string()
            except Exception:
                return
            if text and text != "null":
                outcome.update(json.loads(text))
                Gtk.main_quit()

        view.run_javascript("window.__RESULT", None, done)
        return True

    def on_load(v, event):
        if event != WebKit2.LoadEvent.FINISHED or started["yes"]:
            return
        started["yes"] = True

        def done(_o, res):
            try:
                v.run_javascript_finish(res)
            except Exception as exc:
                print(f"probe script failed: {exc}", file=sys.stderr)
                Gtk.main_quit()
                return
            GLib.timeout_add(200, poll)

        v.run_javascript(script, None, done)

    view.connect("load-changed", on_load)
    view.load_html(page, "http://localhost/")
    GLib.timeout_add_seconds(30, Gtk.main_quit)
    Gtk.main()

    if not outcome:
        print("reader-frame engine test produced no result (webview timed out)", file=sys.stderr)
        return 1

    failures = [
        f"  {key}: expected {want}, got {outcome.get(key)!r}"
        for key, want in EXPECTED.items()
        if outcome.get(key) is not want
    ]
    if failures:
        print(f"reader-frame engine test FAILED with sandbox={sandbox!r}", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"reader-frame engine test OK (sandbox={sandbox!r})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
