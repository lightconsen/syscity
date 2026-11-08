"""Attach a pyautogui/pyperclip surface to the guest's real X desktop.

syscity plans actions as small pyautogui snippets (see `click_code`,
`type_code`, `drag_code` and the rest in src/computer/remote/osworld.rs) and
runs each one inside the benchmark VM. That VM image ships pyautogui; the Cua
Bench bench-web image does not. Rather than rewrite the snippets, this module
implements the handful of pyautogui and pyperclip names they use on top of
xdotool, xclip and ImageMagick, which the image does have.

Every call goes to the same X display the picture comes from, so a click lands
where the screenshot showed the button.
"""

import os
import shutil
import subprocess
import time

#: How long a copied selection stays available, in seconds.
CLIPBOARD_LINGER = 60

#: How long `copy` waits for the selection to actually serve its text.
CLIPBOARD_SETTLE = 3.0

#: The fewest motion events a drag may produce. Below this a browser does not
#: recognise the gesture as a drag at all and simply never fires `drop`.
DRAG_MIN_STEPS = 24

_display = os.environ.get("DISPLAY") or ":0"
os.environ.setdefault("DISPLAY", _display)


def _run(argv, input_text=None):
    return subprocess.run(
        argv,
        input=input_text.encode() if input_text is not None else None,
        capture_output=True,
    )


def _xdotool(*args):
    result = _run(["xdotool", *[str(a) for a in args]])
    # `xdotool` exits 0 even when it ignores a name it does not know — it warns
    # on stderr and carries on. A dropped keypress that looks like a success is
    # exactly the kind of silence this shim must not add to, so stderr from the
    # key paths is treated as failure: `No such key name` means the action did
    # not happen.
    complaint = result.stderr.decode(errors="replace").strip()
    if result.returncode != 0 or "No such key name" in complaint:
        raise RuntimeError(
            "xdotool {} failed: {}".format(
                " ".join(str(a) for a in args),
                complaint or "exit {}".format(result.returncode),
            )
        )
    return result.stdout.decode(errors="replace")


# ── Mouse ───────────────────────────────────────────────────────────────────


def position():
    out = _xdotool("getmouselocation", "--shell")
    values = dict(
        line.split("=", 1) for line in out.splitlines() if "=" in line
    )
    return int(values.get("X", 0)), int(values.get("Y", 0))


def size():
    out = _xdotool("getdisplaygeometry").split()
    return int(out[0]), int(out[1])


def moveTo(x, y, duration=0.0):
    if duration and duration > 0:
        start_x, start_y = position()
        # A drag is recognised by the *sequence* of motion it produces, not by
        # the endpoints: Chromium starts its native drag once a press has been
        # followed by enough movement events, and drops it without ever firing
        # `drop` if the pointer jumps. The step count therefore has a floor well
        # above what 60 Hz would give, and the per-step pause is capped — each
        # `xdotool` invocation already costs several milliseconds, so the
        # wall-clock duration of a drag is the step count, not `duration`.
        steps = max(DRAG_MIN_STEPS, int(duration * 400))
        pause = min(0.02, duration / steps)
        for step in range(1, steps + 1):
            fraction = step / steps
            _xdotool(
                "mousemove",
                round(start_x + (x - start_x) * fraction),
                round(start_y + (y - start_y) * fraction),
            )
            time.sleep(pause)
        return
    _xdotool("mousemove", x, y)


def _button_number(button):
    return {"left": 1, "middle": 2, "right": 3}.get(str(button).lower(), 1)


def click(x=None, y=None, button="left", clicks=1):
    if x is not None and y is not None:
        moveTo(x, y)
    _xdotool("click", "--repeat", max(1, int(clicks)), _button_number(button))


def doubleClick(x=None, y=None, button="left"):
    click(x, y, button=button, clicks=2)


def tripleClick(x=None, y=None, button="left"):
    click(x, y, button=button, clicks=3)


def rightClick(x=None, y=None):
    click(x, y, button="right")


def middleClick(x=None, y=None):
    click(x, y, button="middle")


def mouseDown(x=None, y=None, button="left"):
    if x is not None and y is not None:
        moveTo(x, y)
    _xdotool("mousedown", _button_number(button))


def mouseUp(x=None, y=None, button="left"):
    if x is not None and y is not None:
        moveTo(x, y)
    _xdotool("mouseup", _button_number(button))


def dragTo(x, y, duration=0.5, button="left"):
    start_x, start_y = position()
    mouseDown(start_x, start_y, button=button)
    moveTo(x, y, duration=duration)
    mouseUp(x, y, button=button)


def scroll(clicks, x=None, y=None):
    if x is not None and y is not None:
        moveTo(x, y)
    # PyAutoGUI counts wheel clicks; X button 4 is up and 5 is down.
    button = 4 if clicks > 0 else 5
    _xdotool("click", "--repeat", max(1, abs(int(clicks))), button)


def hscroll(clicks, x=None, y=None):
    if x is not None and y is not None:
        moveTo(x, y)
    button = 6 if clicks > 0 else 7
    _xdotool("click", "--repeat", max(1, abs(int(clicks))), button)


# ── Keyboard ────────────────────────────────────────────────────────────────

# xdotool key names differ from pyautogui's, and it is case-sensitive in a way
# pyautogui is not: `Page_Up` and `F1` are real, `page_up` and `f1` are not.
#
# Every name a snippet may use is listed here with the X keysym it becomes, so
# that an unmapped name can be refused instead of forwarded. xdotool answers an
# unknown name with a stderr warning and exit 0 — a dropped keypress dressed as
# a success — and that silence cost whole tasks: `page_up` in a date-picker run
# did nothing at all while syscity believed the key had been pressed.
#
# X has no keysym for the Windows menu keys, the Japanese input toggles or the
# numeric-pad operators, so those pyautogui names are absent and therefore
# refused. A refused key raises, and the snippet can react; a dropped one cannot.
_KEY_ALIASES = {
    # Editing and navigation.
    "enter": "Return",
    "return": "Return",
    "kp_enter": "KP_Enter",
    "esc": "Escape",
    "escape": "Escape",
    "del": "Delete",
    "delete": "Delete",
    "backspace": "BackSpace",
    "tab": "Tab",
    "space": "space",
    "insert": "Insert",
    "ins": "Insert",
    "capslock": "Caps_Lock",
    "numlock": "Num_Lock",
    "scrolllock": "Scroll_Lock",
    "printscreen": "Print",
    "print": "Print",
    # All three spellings pyautogui ships and callers use in practice.
    "prtsc": "Print",
    "prtscr": "Print",
    "prntscrn": "Print",
    "pause": "Pause",
    "menu": "Menu",
    "apps": "Menu",
    "home": "Home",
    "end": "End",
    "up": "Up",
    "down": "Down",
    "left": "Left",
    "right": "Right",
    "pageup": "Prior",
    "page_up": "Prior",
    "pagedown": "Next",
    "page_down": "Next",
    "pgup": "Prior",
    "pgdn": "Next",
    # Modifiers. X has one keysym per modifier, so pyautogui's left/right split
    # collapses; `cmd` means whatever the platform's shortcut modifier is, and
    # on the guest that is `ctrl`.
    "ctrl": "ctrl",
    "control": "ctrl",
    "ctrlleft": "ctrl",
    "ctrlright": "ctrl",
    "alt": "alt",
    "option": "alt",
    "altleft": "alt",
    "altright": "alt",
    "shift": "shift",
    "shiftleft": "shift",
    "shiftright": "shift",
    "win": "super",
    "super": "super",
    "winleft": "super",
    "winright": "super",
    "cmd": "ctrl",
    "command": "ctrl",
    "meta": "ctrl",
    # Keys with no X equivalent of their own.
    "help": "F1",
    "fn": "XF86WakeUp",
    # Media and browser keys.
    "volumemute": "XF86AudioMute",
    "volumedown": "XF86AudioLowerVolume",
    "volumeup": "XF86AudioRaiseVolume",
    "playpause": "XF86AudioPlay",
    "nexttrack": "XF86AudioNext",
    "prevtrack": "XF86AudioPrev",
    "stop": "XF86AudioStop",
    "launchmail": "XF86Mail",
    "launchapp1": "XF86Launch1",
    "launchhome": "XF86HomePage",
    "browserback": "XF86Back",
    "browserforward": "XF86Forward",
    "browserrefresh": "XF86Refresh",
    "browsersearch": "XF86Search",
    "sleep": "XF86Sleep",
}

#: pyautogui spells the function keys in lower case; X wants `F1`.
for _number in range(1, 13):
    _KEY_ALIASES["f{}".format(_number)] = "F{}".format(_number)
del _number

#: pyautogui also calls Page Up/Down the "prior"/"next" keys.
_KEY_ALIASES["prior"] = "Prior"
_KEY_ALIASES["next"] = "Next"

#: The keysyms the map produces. A caller may pass one of these directly — the
#: snippets syscity emits mostly do — so they have to round-trip unchanged rather
#: than be refused for not being pyautogui's own spelling.
_KEY_SYMS = set(_KEY_ALIASES.values())


def _key_name(key):
    """The X keysym for one pyautogui key, or a character as itself.

    A named key that is in neither `_KEY_ALIASES` nor `_KEY_SYMS` raises rather
    than being forwarded, because xdotool would drop it and still exit 0.
    """
    text = str(key)
    if len(text) == 1:
        return text
    lowered = text.lower()
    if lowered in _KEY_ALIASES:
        return _KEY_ALIASES[lowered]
    if text in _KEY_SYMS:
        return text
    raise RuntimeError(
        "unrecognised key {!r}: it is neither a single character nor a name "
        "this shim can map to an X keysym".format(text)
    )


def press(keys, presses=1, interval=0.0):
    if isinstance(keys, str):
        keys = [keys]
    for _ in range(max(1, int(presses))):
        for key in keys:
            _xdotool("key", _key_name(key))
        if interval:
            time.sleep(interval)


def hotkey(*keys):
    if len(keys) == 1 and isinstance(keys[0], (list, tuple)):
        keys = keys[0]
    _xdotool("key", "+".join(_key_name(key) for key in keys))


def keyDown(key):
    _xdotool("keydown", _key_name(key))


def keyUp(key):
    _xdotool("keyup", _key_name(key))


def write(text, interval=0.0):
    """Type text as keystrokes.

    `xdotool type` takes the string directly, and with `--clearmodifiers` it
    does not inherit a modifier still held from a previous hotkey.
    """
    if interval and interval > 0:
        for character in text:
            _xdotool("type", "--clearmodifiers", "--delay", str(int(interval * 1000)), character)
        return
    _xdotool("type", "--clearmodifiers", "--delay", "12", text)


def typewrite(text, interval=0.0):
    write(text, interval=interval)


# ── Clipboard (pyperclip) ───────────────────────────────────────────────────

PASTE = "ctrl+v"


def _require_xclip():
    if shutil.which("xclip") is None:
        raise RuntimeError("xclip is not installed in this guest")


def copy(text):
    """Own the X clipboard as `text`, and say so only once it is true.

    An X selection lives in whichever process owns it, so a copy leaves `xclip`
    running in its own session: the text goes in on stdin, and `CLIPBOARD_LINGER`
    bounds how long it stays. Without the timeout a snippet that copies and
    never pastes would leave a process behind for the life of the sandbox.

    Three details here are each the difference between a working paste and a
    silent one, and all three were found by pasting into a real guest:

    * The text arrives on **stdin**, not through a temp file. A temp file path
      was passed here once and unlinked immediately after `Popen` returned — but
      `Popen` returns before the child has exec'd, so `xclip` would sometimes
      open a path that no longer existed, exit with `No such file or directory`,
      and leave nothing owning the selection.
    * `-loops 0`, not `-loops 1`. A GTK/Chromium paste first asks the owner for
      `TARGETS` and only then for the data; with a one-shot server that first
      request exhausts the loop, so the actual paste reads nothing. `-loops 1`
      pastes into `xterm` and fails into every browser.
    * The copy is **verified** by reading the selection back, because a `copy`
      that reports success while owning nothing defeats the usual snippet
      shape — `try: pyperclip.copy(...); hotkey('ctrl','v') except: write(...)`
      — by never reaching the fallback. Raising here is what lets the snippet
      type the text instead of losing it.
    """
    _require_xclip()
    text = str(text)

    process = subprocess.Popen(
        [
            "timeout",
            str(CLIPBOARD_LINGER),
            "xclip",
            "-selection",
            "clipboard",
            "-loops",
            "0",
            "-i",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        process.stdin.write(text.encode())
        process.stdin.close()
    except OSError as error:
        raise RuntimeError("could not hand the text to xclip: {}".format(error))

    if not text:
        return
    # The owner takes a moment to claim the selection; pasting before it has is
    # the other half of a paste that lands empty. Waiting for the read-back
    # proves the text is actually reachable, not merely that xclip was started.
    deadline = time.time() + CLIPBOARD_SETTLE
    while True:
        result = _run(["xclip", "-selection", "clipboard", "-o"])
        if result.returncode == 0 and result.stdout == text.encode():
            return
        if time.time() >= deadline:
            raise RuntimeError(
                "the clipboard is not serving the copied text after "
                "{:.1f}s".format(CLIPBOARD_SETTLE)
            )
        time.sleep(0.05)


def paste():
    _require_xclip()
    result = _run(["xclip", "-selection", "clipboard", "-o"])
    if result.returncode != 0:
        return ""
    return result.stdout.decode(errors="replace")
