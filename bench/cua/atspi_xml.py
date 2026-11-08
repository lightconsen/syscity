"""Render the guest's AT-SPI desktop as the XML the OSWorld VM service returns.

Run inside the benchmark guest. Emits one element per AT-SPI accessible with
`role`, `name`, `value`, `states` and `x`/`y`/`width`/`height` attributes, which
is the shape `parse_at_spi` in src/computer/remote/osworld.rs reads.
"""

import sys
import xml.etree.ElementTree as ET

MAX_NODES = 4000
MAX_DEPTH = 60

SKIP_ROLES = {"redundant object"}

# State name reported in the `states` attribute -> AT-SPI constant.
STATE_NAMES = (
    ("showing", "STATE_SHOWING"),
    ("visible", "STATE_VISIBLE"),
    ("focused", "STATE_FOCUSED"),
    ("active", "STATE_ACTIVE"),
    ("focusable", "STATE_FOCUSABLE"),
    ("editable", "STATE_EDITABLE"),
    ("checked", "STATE_CHECKED"),
    ("selected", "STATE_SELECTED"),
    ("expanded", "STATE_EXPANDED"),
)


def _text(value):
    if value is None:
        return None
    try:
        text = str(value).strip()
    except Exception:
        return None
    return text or None


def _bounds(accessible):
    try:
        extents = accessible.queryComponent().getExtents(0)  # ATSPI_COORD_TYPE_SCREEN
    except Exception:
        return None
    x, y, width, height = extents.x, extents.y, extents.width, extents.height
    if width <= 0 or height <= 0:
        return None
    # A window manager that has not mapped a scrollbar yet reports
    # INT32_MIN for its origin; such a box would resolve clicks off-screen.
    if x < -10000 or y < -10000:
        return None
    return x, y, width, height


def _states(accessible, pyatspi):
    """The `states` attribute: positive flags, plus `disabled` when absent.

    An absent state list is not evidence of anything, so a node whose state set
    cannot be read gets no attribute at all and the session keeps `Tri::Unknown`.
    """
    try:
        state_set = accessible.getState()
    except Exception:
        return ""

    names = []
    enabled_flag = getattr(pyatspi, "STATE_ENABLED", None)
    if enabled_flag is not None:
        try:
            if not state_set.contains(enabled_flag):
                names.append("disabled")
        except Exception:
            pass
    for label, constant in STATE_NAMES:
        flag = getattr(pyatspi, constant, None)
        if flag is None:
            continue
        try:
            if state_set.contains(flag):
                names.append(label)
        except Exception:
            pass
    return " ".join(names)


def _value(accessible):
    try:
        value = _text(accessible.queryValue().currentValue)
    except Exception:
        return None
    return value


def build(accessible, pyatspi, depth, counter, parent):
    if depth > MAX_DEPTH or counter[0] >= MAX_NODES:
        return
    counter[0] += 1
    try:
        node = ET.SubElement(parent, "node")
        role = accessible.getRoleName() or "unknown"
        name = _text(accessible.name)
        value = _value(accessible)
        if value is not None and value == name:
            value = None
        node.set("role", role)
        if name:
            node.set("name", name)
        if value:
            node.set("value", value)
        states = _states(accessible, pyatspi)
        if states:
            node.set("states", states)
        bounds = _bounds(accessible)
        if bounds is not None:
            x, y, width, height = bounds
            node.set("x", str(x))
            node.set("y", str(y))
            node.set("width", str(width))
            node.set("height", str(height))
    except Exception:
        return

    try:
        count = accessible.childCount
    except Exception:
        count = 0
    for index in range(count):
        if counter[0] >= MAX_NODES:
            return
        try:
            child = accessible.getChildAtIndex(index)
        except Exception:
            continue
        if child is None:
            continue
        try:
            if child.getRoleName() in SKIP_ROLES:
                continue
        except Exception:
            pass
        build(child, pyatspi, depth + 1, counter, node)


def _is_showing(accessible, pyatspi):
    """Whether an application node is worth walking.

    An application's own state set is unreliable: the bench-web window's
    `child.py` application reports no states at all while its frame and every
    widget below are `showing`. So the test is on the frame, not the
    application, and an application that cannot be read is walked rather than
    dropped.
    """
    flag = getattr(pyatspi, "STATE_DEFUNCT", None)
    try:
        count = accessible.childCount
    except Exception:
        return False
    for index in range(count):
        try:
            frame = accessible.getChildAtIndex(index)
        except Exception:
            continue
        if frame is None:
            continue
        if flag is not None:
            try:
                if frame.getState().contains(flag):
                    continue
            except Exception:
                pass
        return True
    return False


def main():
    import pyatspi

    desktop = pyatspi.Registry.getDesktop(0)
    root = ET.Element("desktop")
    counter = [0]
    for index in range(desktop.childCount):
        if counter[0] >= MAX_NODES:
            break
        try:
            app = desktop.getChildAtIndex(index)
        except Exception:
            continue
        if app is None or not _is_showing(app, pyatspi):
            continue
        build(app, pyatspi, 1, counter, root)
    sys.stdout.write(ET.tostring(root, encoding="unicode"))


if __name__ == "__main__":
    main()
