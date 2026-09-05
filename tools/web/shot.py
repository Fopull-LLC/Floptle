#!/usr/bin/env python3
"""Run the browser probe — or a web export — and bring back what it said and drew.

    tools/web/shot.py                 # the bring-up probe, headless Brave/Chrome
    tools/web/shot.py --display       # a real window on this display
    tools/web/shot.py --browser firefox
    tools/web/shot.py --game DIR      # a folder File ⏵ Export Game… → Web made
    tools/web/shot.py --game DIR --frames 120 --display

Serves target/web/ (or the export folder) on a local port, opens it, and
collects the page's transcript plus a PNG of a frame. For the probe that is
every `RUNG n …` line and the canvas once rung 4 has run; for a game it is the
engine's own log and the frame `--frames` names, photographed the way the
desktop player's `--shot` photographs one. The transcript goes to stdout and
the PNG beside the build (probe.png, or shot.png in the export folder). Exit 0
only when everything passed and a picture arrived.

This is the web half of "verify anything visual by rendering a PNG and looking
at it": the browser is the renderer here, and the PNG is what to look at.

Headless Chromium runs the whole ladder — the VM, every shader through its
compiler, the raster pass, frames — but aborts the readback that makes the
picture ("a valid external Instance reference no longer exists" from
`mapAsync`, however the map is timed). So headless answers "does it run" and
exits 1 for the missing picture; `--display` runs the browser as a window,
which completes the readback, and opens and closes on its own.
"""
import argparse
import base64
import http.server
import json
import os
import shutil
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def target_dir():
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    return Path(json.loads(meta)["target_directory"])


class State:
    lines = []
    png = None
    done = threading.Event()


def handler_for(web):
    class H(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=str(web), **k)

        def log_message(self, *a):
            pass

        def do_POST(self):
            n = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(n).decode("utf-8", "replace")
            if self.path == "/report":
                State.lines.append(body)
                print(body, flush=True)
                if body == "SHOT SENT" or body.startswith(("RUNG", "FATAL", "PANIC")) and "OK" not in body:
                    State.done.set()
            elif self.path == "/shot" and body.startswith("data:image/png;base64,"):
                State.png = base64.b64decode(body.split(",", 1)[1])
            self.send_response(204)
            self.end_headers()

    return H


def browser_cmd(name, url, headless):
    if name in ("brave", "chrome", "chromium"):
        exe = shutil.which(name) or shutil.which("brave") or shutil.which("google-chrome") or shutil.which("chromium")
        if not exe:
            sys.exit("no Chromium-family browser found")
        cmd = [exe, "--no-first-run", "--no-default-browser-check", "--enable-unsafe-webgpu",
               "--user-data-dir=" + str(Path(os.environ.get("TMPDIR", "/tmp")) / "floptle-web-probe-profile")]
        if headless:
            cmd += ["--headless=new", "--no-sandbox", "--use-angle=vulkan", "--enable-features=Vulkan"]
        else:
            cmd += ["--new-window", "--window-size=700,600", "--app=" + url]
            # An X11 window under Xwayland, not a native Wayland one. A Wayland
            # compositor only sends frame callbacks to a surface it is showing,
            # and a window opened from a terminal session can land where it is
            # not — then the page never gets an animation frame and the engine
            # never draws (measured 2026-09-05: the same build ran 5000 frames
            # headless and zero in a Wayland window). Xwayland always ticks.
            if sys.platform.startswith("linux") and os.environ.get("WAYLAND_DISPLAY"):
                cmd += ["--ozone-platform=x11"]
            return cmd
        return cmd + [url]
    if name == "firefox":
        exe = shutil.which("firefox")
        if not exe:
            sys.exit("firefox not found")
        prof = Path(os.environ.get("TMPDIR", "/tmp")) / "floptle-web-probe-ff"
        prof.mkdir(parents=True, exist_ok=True)
        (prof / "user.js").write_text(
            'user_pref("dom.webgpu.enabled", true);\n'
            'user_pref("gfx.webgpu.ignore-blocklist", true);\n'
            'user_pref("browser.shell.checkDefaultBrowser", false);\n'
        )
        cmd = [exe, "--profile", str(prof), "--no-remote"]
        if headless:
            cmd.append("--headless")
        return cmd + [url]
    sys.exit(f"unknown browser {name}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--browser", default="brave")
    ap.add_argument("--display", action="store_true", help="a real window rather than headless")
    ap.add_argument("--timeout", type=float, default=40.0)
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--game", metavar="DIR", help="a web export folder to play instead of the probe")
    ap.add_argument("--console", metavar="FILE", help="write the browser's own console/stderr here (Chromium family)")
    ap.add_argument("--frames", type=int, default=60, help="--game: which frame to photograph")
    a = ap.parse_args()

    web = Path(a.game).resolve() if a.game else target_dir() / "web"
    if not (web / "pkg" / "floptle_web_bg.wasm").is_file():
        if a.game:
            sys.exit(f"{web} is not a web export (no pkg/floptle_web_bg.wasm)")
        sys.exit(f"{web} has no build — run tools/web/build.sh first")
    if a.game and not (web / "game.flpk").is_file():
        sys.exit(f"{web} has no game.flpk — export the project for the web first")

    socketserver.TCPServer.allow_reuse_address = True
    srv = socketserver.TCPServer(("127.0.0.1", a.port), handler_for(web))
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{port}/index.html?shot={a.frames}" if a.game else f"http://127.0.0.1:{port}/probe.html"

    cmd = browser_cmd(a.browser, url, not a.display)
    # Extra browser flags, for the machine this runs on: a compositor that
    # keeps a new window behind the terminal starves it of animation frames,
    # and `--kiosk --window-position=0,0` puts it in front.
    cmd += os.environ.get("FLOPTLE_SHOT_BROWSER_ARGS", "").split()
    err = subprocess.DEVNULL
    if a.console:
        cmd += ["--enable-logging=stderr", "--v=0"]
        err = open(a.console, "w")
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=err)
    try:
        State.done.wait(a.timeout)
    finally:
        proc.terminate()
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
        srv.shutdown()

    if State.png:
        out = web / ("shot.png" if a.game else "probe.png")
        out.write_bytes(State.png)
        print(f"wrote {out} ({len(State.png)} bytes)")
    if a.game:
        bad = [l for l in State.lines if l.startswith(("FATAL", "PANIC"))]
        if not State.done.is_set():
            print(f"timed out after {a.timeout:.0f}s; last line: "
                  f"{State.lines[-1] if State.lines else '(nothing reported)'}")
            return 3
        if bad:
            print("the game stopped: " + bad[0])
            return 1
        if State.png:
            print(f"the game ran to frame {a.frames} and the frame came back")
            return 0
        print("no picture")
        return 1
    rungs = [l for l in State.lines if l.startswith("RUNG ")]
    ok = [l for l in rungs if " OK" in l]
    if not State.done.is_set():
        print(f"timed out after {a.timeout:.0f}s with {len(ok)} rung(s) passed; last line: "
              f"{State.lines[-1] if State.lines else '(nothing reported)'}")
        return 3
    if len(ok) == 4 and State.png:
        print("all four rungs passed and the canvas came back")
        return 0
    print(f"{len(ok)} of 4 rungs passed" + ("" if State.png else "; no picture"))
    return 1


if __name__ == "__main__":
    sys.exit(main())
