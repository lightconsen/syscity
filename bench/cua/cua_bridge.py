"""The OSWorld VM desktop service, served from inside a Cua Bench sandbox.

`syscity bench run` speaks one protocol (src/computer/remote/osworld.rs):
`GET /screenshot`, `GET /accessibility`, `POST /screen_size`, `POST /execute`.
The Cua Bench session speaks another: a `cua_sandbox.Sandbox` whose shell,
mouse and screen live behind the SDK. This file is the missing half — it runs
*in the guest*, on the guest's own X display, and answers those four endpoints.
The host then tunnels port 8765 and hands the URL to syscity, so syscity drives
the sandbox exactly as it drives an OSWorld VM, with no adapter of its own.

Two details make it an honest bridge rather than a mock:

* `/screenshot` is a capture of the display the session also captures, so a
  frame syscity reasons about is a frame the evaluator will see.
* `/execute` really runs the snippet, with `pyautogui` and `pyperclip` bound to
  this module's implementations over xdotool and xclip. syscity's planning is
  unchanged; only the library underneath differs from an OSWorld VM.

Kept dependency-free on purpose: the guest's Python has no pip-installed web
framework, and pulling one in would be a network dependency in the middle of a
benchmark run.
"""

import base64
import io
import json
import os
import subprocess
import sys
import threading
import traceback
import types
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 8765
DISPLAY = os.environ.get("DISPLAY") or ":0"
os.environ.setdefault("DISPLAY", DISPLAY)

HERE = os.path.dirname(os.path.abspath(__file__))


# ── The guest's display ─────────────────────────────────────────────────────


def screen_size():
    out = subprocess.run(
        ["xdotool", "getdisplaygeometry"], capture_output=True, text=True
    )
    if out.returncode == 0:
        width, height = out.stdout.split()[:2]
        return int(width), int(height)
    # Fall back to asking X directly if xdotool is missing.
    out = subprocess.run(
        ["xdpyinfo"], capture_output=True, text=True
    )
    for line in out.stdout.splitlines():
        if "dimensions:" in line:
            width, height = line.split()[1].split("x")[:2]
            return int(width), int(height)
    raise RuntimeError("could not read the display size")


def capture_png():
    """PNG bytes of the whole display.

    ImageMagick's `import` writes to whatever path it is given; `png:-` puts it
    on stdout, which keeps the frame out of the guest's disk.
    """
    out = subprocess.run(
        ["import", "-window", "root", "-display", DISPLAY, "png:-"],
        capture_output=True,
    )
    if out.returncode != 0 or not out.stdout.startswith(b"\x89PNG"):
        raise RuntimeError(
            "screenshot failed: {}".format(
                out.stderr.decode(errors="replace").strip()[:500]
            )
        )
    return out.stdout


def accessibility_xml():
    script = os.path.join(HERE, "atspi_xml.py")
    out = subprocess.run(
        [sys.executable, script], capture_output=True, text=True, timeout=60
    )
    if out.returncode != 0:
        raise RuntimeError(
            "accessibility tree failed: {}".format(out.stderr.strip()[:1000])
        )
    return out.stdout


# ── The pyautogui / pyperclip surface ──────────────────────────────────────

_exec_lock = threading.Lock()


def install_shims():
    """Bind `import pyautogui` and `import pyperclip` to the shim modules.

    Registered in `sys.modules` rather than written to disk so a snippet runs
    against the same code as the bridge and nothing has to be installed.
    """
    shim_dir = HERE
    if shim_dir not in sys.path:
        sys.path.insert(0, shim_dir)
    import pyautogui_shim

    pyautogui_module = types.ModuleType("pyautogui")
    pyperclip_module = types.ModuleType("pyperclip")
    for name in dir(pyautogui_shim):
        if name.startswith("_") and name != "__all__":
            continue
        setattr(pyautogui_module, name, getattr(pyautogui_shim, name))
    pyautogui_module.FAILSAFE = False
    pyautogui_module.PAUSE = 0.0
    pyperclip_module.copy = pyautogui_shim.copy
    pyperclip_module.paste = pyautogui_shim.paste
    sys.modules["pyautogui"] = pyautogui_module
    sys.modules["pyperclip"] = pyperclip_module


def execute(code):
    """Run one snippet and return its stdout.

    Every snippet gets its own globals, so a name one snippet leaves behind
    cannot quietly change what the next one does.
    """
    namespace = {"__name__": "__cua_exec__", "__builtins__": __builtins__}
    buffer = io.StringIO()
    stdout, sys.stdout = sys.stdout, buffer
    try:
        with _exec_lock:
            exec(compile(code, "<syscity-action>", "exec"), namespace)
    finally:
        sys.stdout = stdout
    return buffer.getvalue()


# ── HTTP ────────────────────────────────────────────────────────────────────


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("bridge: " + (fmt % args) + "\n")
        sys.stderr.flush()

    def _read_body(self):
        """Read the whole request body, whatever the endpoint does with it.

        A body left unread stays in the socket, and the next request on the
        same keep-alive connection starts parsing at the wrong offset — which
        arrives as `501 Unsupported method ('{}GET')`. So every POST drains it,
        including the ones that ignore it.
        """
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not raw:
            return {}
        try:
            return json.loads(raw)
        except ValueError:
            return {}

    def _send(self, status, body, content_type):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self, status, payload):
        self._send(status, json.dumps(payload), "application/json")

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        try:
            if path == "/screenshot":
                self._send(200, capture_png(), "image/png")
            elif path == "/accessibility":
                self._send(200, accessibility_xml(), "text/plain; charset=utf-8")
            elif path == "/health":
                self._send_json(200, {"ok": True})
            else:
                self._send_json(404, {"error": "no such endpoint: %s" % path})
        except Exception as error:  # noqa: BLE001 - the envelope is the message
            self.log_message("GET %s failed: %s", path, error)
            self._send_json(500, {"error": str(error)})

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        body = self._read_body()
        try:
            if path == "/screen_size":
                width, height = screen_size()
                self._send_json(200, {"output": {"width": width, "height": height}})
            elif path == "/execute":
                code = body.get("command") or ""
                if not isinstance(code, str) or not code.strip():
                    self._send_json(400, {"error": "no command given"})
                    return
                output = execute(code)
                self._send_json(200, {"output": output})
            elif path == "/health":
                self._send_json(200, {"ok": True})
            else:
                self._send_json(404, {"error": "no such endpoint: %s" % path})
        except Exception as error:  # noqa: BLE001
            detail = traceback.format_exc(limit=3)
            self.log_message("POST %s failed: %s", path, error)
            self._send_json(500, {"error": str(error), "traceback": detail})


def main():
    install_shims()
    server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    server.daemon_threads = True
    sys.stderr.write("bridge listening on 0.0.0.0:%d (DISPLAY=%s)\n" % (PORT, DISPLAY))
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
