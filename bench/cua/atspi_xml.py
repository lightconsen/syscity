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

#: Roles whose text is behind `queryText` rather than `queryValue`.
#:
#: WebKitGTK's `entry` — what every `input` and `textarea` in a pywebview task
#: window exposes — raises `NotImplementedError` from `queryValue()` while
#: `queryText()` returns the real content. Asking only `queryValue` therefore
#: published no value for any webview text field, and the typing verifier had
#: nothing to compare: `typed_value_hint` needs an editable field whose value
#: moved, so it returned None and verification fell through to the pixel diff,
#: where 43 characters landing in one field is 0.0% of the screen.
#:
#: Deliberately not every role: AT-SPI's `text` role (static document text) is
#: mapped to `text_field` by the reader, so giving it a value would turn every
#: paragraph into an editable field and make the field counts disagree between
#: the before and after trees. Only controls the user types into.
#:
#: `password text` is excluded on purpose. It raises from `queryValue` the same
#: way, but `queryText` there answers with the plaintext — publishing it would
#: put a credential into the turn log and the trace. A masked field already
#: falls back to the pixel diff, which is the right answer for it.
TEXT_VALUE_ROLES = {"entry", "text field"}

#: Roles a closed `<select>` puts its options under, where every option reports
#: the combo box's own box. See [`real_option_boxes`].
POPUP_ROLES = {"combo box"}

#: Roles that hold the real, on-screen option rows once a popup is open.
DEMUX_CONTAINER_ROLES = {"window", "tree table"}
DEMUX_ITEM_ROLES = {"table cell", "menu item", "list item"}

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


def _bounds(accessible, pyatspi=None, states=None):
    try:
        extents = accessible.queryComponent().getExtents(0)  # ATSPI_COORD_TYPE_SCREEN
    except Exception:
        return None
    x, y, width, height = extents.x, extents.y, extents.width, extents.height
    if width <= 0 or height <= 0:
        return None
    if x < -10000 or y < -10000:
        return None
    # `visible` means the widget is laid out; `showing` means it is actually
    # inside its scroll viewport. A button pushed past the bottom of a short
    # window keeps its layout box and keeps reporting `visible`, but it is not
    # on screen — and handing that box to the agent sends the click into
    # whatever is behind the window. `click-button`'s 256x256 window holds six
    # buttons and grows to eight: the target lands on a fourth row below the
    # fold, the tree still says "button at y=620", and a click there hits the
    # bare desktop. Dropping the box is the honest answer — the element exists,
    # but this read knows no point on screen for it.
    if states is not None and "visible" in states and "showing" not in states:
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


def _value(accessible, role=None):
    """The value attribute: `queryValue`, falling back to `queryText`.

    The fallback is for WebKitGTK entries, where `queryValue` is not
    implemented at all — see [`TEXT_VALUE_ROLES`]. A node that answers neither
    publishes no value, which is the honest answer: "not readable" is not "".
    """
    try:
        value = _text(accessible.queryValue().currentValue)
    except Exception:
        value = None
    if value is None and role in TEXT_VALUE_ROLES:
        try:
            text = accessible.queryText()
            value = _text(text.getText(0, text.characterCount))
        except Exception:
            return None
    return value


def _walk(accessible, depth=0, counter=None):
    """Yield `(role, name, box)` for `accessible` and every descendant."""
    if counter is None:
        counter = [0]
    if depth > MAX_DEPTH or counter[0] >= MAX_NODES:
        return
    counter[0] += 1
    try:
        role = accessible.getRoleName() or "unknown"
        name = _text(accessible.name)
        box = _bounds(accessible)
    except Exception:
        return
    yield role, name, box
    try:
        count = accessible.childCount
    except Exception:
        count = 0
    for index in range(count):
        try:
            child = accessible.getChildAtIndex(index)
        except Exception:
            continue
        if child is None:
            continue
        yield from _walk(child, depth + 1, counter)


def real_option_boxes(nodes):
    """Real on-screen boxes for `<select>` options, keyed by label.

    A closed `<select>`'s options are all reported at the combo box's own box.
    Clicking "Banana" there just closes the menu, and the model has no way to
    tell two options apart — which is exactly how `select-dropdown` was lost.

    When the popup is *open* the real rows are live in a top-level `window` →
    `tree table` → `table cell` subtree, but the stale copies under the combo
    box are still in the tree and still come first, so a reader that takes what
    it is given never sees the real box.

    This maps label -> correct box for exactly the options whose reported box is
    demonstrably the collapsed fiction, and leaves every other node alone:

    * a label is remapped only when every copy of it under the combo box shares
      the combo box's own box;
    * a popup that is *closed* is a `menu` of size 0×0, so it yields no rows and
      the tree comes out exactly as before;
    * a label that appears more than once in a popup is skipped, not guessed at.

    `nodes` is the output of [`_walk`] over one application.
    """
    nodes = list(nodes)

    popups = []        # [[(label, box), ...], ...] one per popup container
    boxes = {}         # label -> real box, only when unambiguous
    combo_boxes = {}   # combo box label -> its own box
    option_boxes = {}  # combo box label -> {option label: [reported box, ...]}

    # A popup container is never nested inside the combo box it belongs to, so
    # once one starts, the rows that follow are the popup's, not the combo
    # box's. `in_popup` says which kind of row the next one is; `owner` says
    # whose options they are.
    owner = None
    in_popup = False
    for role, name, box in nodes:
        if role in DEMUX_CONTAINER_ROLES:
            in_popup = True
            popups.append([])
        elif role in POPUP_ROLES:
            in_popup = False
            owner = name
            if name and box:
                combo_boxes[name] = box
        elif owner and name and role in DEMUX_ITEM_ROLES:
            if in_popup:
                popups[-1].append((name, box))
            else:
                option_boxes.setdefault(owner, {}).setdefault(name, []).append(box)

    # A popup labels rows unambiguously only if each label appears once in it.
    for rows in popups:
        seen = {}
        for label, box in rows:
            seen.setdefault(label, []).append(box)
        for label, found in seen.items():
            if len(found) == 1 and found[0] is not None and label not in boxes:
                boxes[label] = found[0]

    out = {}
    for owner, options in option_boxes.items():
        own = combo_boxes.get(owner)
        for label, reported in options.items():
            if label not in boxes:
                continue
            if any(box is not None and box != own for box in reported):
                continue  # the page reports a distinct box; believe it
            out[label] = boxes[label]
    return out


def build(accessible, pyatspi, depth, counter, parent, real_boxes=None):
    if depth > MAX_DEPTH or counter[0] >= MAX_NODES:
        return
    counter[0] += 1
    try:
        node = ET.SubElement(parent, "node")
        role = accessible.getRoleName() or "unknown"
        name = _text(accessible.name)
        value = _value(accessible, role)
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
        bounds = _bounds(accessible, pyatspi, states.split() if states else None)
        # An option row inside a `<select>` popup reports the combo box's own
        # box until the popup opens; while it is open the real row is a
        # different element entirely. Substituting the real one is the only way
        # a click can land on the option the caller asked for.
        if real_boxes and role in DEMUX_ITEM_ROLES and name in real_boxes:
            bounds = real_boxes[name]
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
        build(child, pyatspi, depth + 1, counter, node, real_boxes)


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
    # Read once from the whole desktop: an open `<select>` puts its real rows in
    # a top-level `window`, not under the application that owns the combo box.
    real_boxes = _desktop_option_boxes(desktop, pyatspi)
    for index in range(desktop.childCount):
        if counter[0] >= MAX_NODES:
            break
        try:
            app = desktop.getChildAtIndex(index)
        except Exception:
            continue
        if app is None or not _is_showing(app, pyatspi):
            continue
        build(app, pyatspi, 1, counter, root, real_boxes)
    sys.stdout.write(ET.tostring(root, encoding="unicode"))


def _desktop_option_boxes(desktop, pyatspi):
    """`real_option_boxes` over every application on the desktop."""
    found = {}
    try:
        count = desktop.childCount
    except Exception:
        return found
    for index in range(count):
        try:
            app = desktop.getChildAtIndex(index)
        except Exception:
            continue
        if app is None:
            continue
        try:
            found.update(real_option_boxes(_walk(app)))
        except Exception:
            continue
    return found


if __name__ == "__main__":
    main()
