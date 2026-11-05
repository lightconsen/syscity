// The page-side half of `android_web`.
//
// Evaluated into the agent's WebView with `evaluateJavascript`, then asked
// questions. It is deliberately small: the Rust side owns the protocol, the
// timeouts and the wording, and this only touches the DOM.
//
// Two rules shape it:
//
// * **Every read is a new numbering.** `snapshot()` rebuilds the element list
//   and hands each one an index, kept in `els`; `click(i)` and `type(i, …)`
//   address that list. After navigation the page is a different document and
//   the numbers mean nothing — the caller re-reads, exactly as it does after an
//   accessibility `ref` goes stale.
// * **`click` presses the element, not a point.** A WebView has no
//   `input tap` that lands on the right piece of a canvas-drawn page, and the
//   element already knows what it is. `scrollIntoView` first so a control below
//   the fold still gets the press a person would have given it after scrolling.
//
// `VERSION` is what makes re-injection cheap and safe: the source is prepended
// before every evaluation, so a build whose shim changed replaces the page's
// older copy, and a page that already has this one skips the work.

(() => {
  const VERSION = 1;
  if (window.__syscity && window.__syscity.version === VERSION) return true;

  const els = [];

  const isField = (el) => ['input', 'textarea', 'select'].includes(el.tagName.toLowerCase());

  const isButton = (el) => {
    const tag = el.tagName.toLowerCase();
    if (tag === 'button') return true;
    if (tag !== 'input') return false;
    return ['submit', 'button', 'reset'].includes((el.getAttribute('type') || 'text').toLowerCase());
  };

  // A control's label.
  //
  // Never a text field's `value`: that is what the person typed there before
  // this read, and a password sitting in a transcript is the one leak a browser
  // bridge of all things must not cause. A submit button is the exception worth
  // stating — `<input type=submit value="Sign in">` *carries* its label in
  // `value`, and most login forms give the button nothing else.
  const text = (el) => {
    const raw =
      el.getAttribute('aria-label') ||
      el.getAttribute('placeholder') ||
      el.getAttribute('title') ||
      // A field's `name` is what the page calls it (`username`, `password`),
      // and it is the only label many login forms give a control at all.
      el.getAttribute('name') ||
      (isButton(el) ? el.value : '') ||
      (isField(el) ? '' : el.innerText || el.textContent || '') ||
      '';
    return String(raw).replace(/\s+/g, ' ').trim().slice(0, 160);
  };

  const role = (el) => {
    const explicit = el.getAttribute('role');
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    if (tag === 'a') return 'link';
    if (tag === 'button') return 'button';
    if (tag === 'input') return isButton(el) ? 'button' : 'input';
    if (tag === 'textarea') return 'input';
    if (tag === 'select') return 'select';
    return el.isContentEditable ? 'input' : tag;
  };

  // A stable handle the Rust side can quote back: an id when the page has one,
  // else a short `tag[name=…]` or a positional selector. It is *not* how
  // click/type address elements — the index is — so a change here cannot break
  // an action.
  const selector = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const tag = el.tagName.toLowerCase();
    const name = el.getAttribute('name');
    if (name) return tag + '[name="' + name.replace(/"/g, '\\"') + '"]';
    const parent = el.parentNode;
    const siblings = Array.from(parent ? parent.children : []);
    const same = siblings.filter((s) => s.tagName === el.tagName);
    return tag + ':nth-of-type(' + (same.indexOf(el) + 1) + ')';
  };

  // What a person can press or type into. `[onclick]` covers the divs that
  // many older pages use as buttons; `contenteditable` is a rich-text box.
  const INTERACTIVE =
    'a[href],button,input,textarea,select,summary,' +
    '[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],' +
    '[onclick],[contenteditable=""],[contenteditable=true]';

  window.__syscity = {
    version: VERSION,

    snapshot() {
      els.length = 0;
      for (const el of document.querySelectorAll(INTERACTIVE)) {
        const rect = el.getBoundingClientRect();
        // Hidden, collapsed or not laid out: a person could not press it, so
        // neither can this, and an index that names it would point at nothing.
        if (rect.width <= 0 || rect.height <= 0) continue;
        if (el.disabled) continue;
        // An unlabelled control is a leak of the page's markup, not something
        // to act on — unless it is a field, where a login form's password box
        // often carries only `type=password` and a name.
        if (!text(el) && !isField(el)) continue;
        els.push(el);
      }

      const elements = els.map((el, index) => {
        const rect = el.getBoundingClientRect();
        return {
          i: index + 1,
          role: role(el),
          tag: el.tagName.toLowerCase(),
          type: (el.getAttribute('type') || '').toLowerCase(),
          name: text(el),
          selector: selector(el),
          x: Math.round(rect.x + rect.width / 2),
          y: Math.round(rect.y + rect.height / 2),
        };
      });

      const body = document.body ? document.body.innerText || '' : '';
      return {
        url: location.href,
        title: document.title || '',
        text: body
          .replace(/[ \t]+\n/g, '\n')
          .replace(/\n{3,}/g, '\n\n')
          .trim()
          .slice(0, 20000),
        elements,
      };
    },

    click(i) {
      const el = els[i - 1];
      if (!el) return { ok: false, reason: 'gone' };
      el.scrollIntoView({ block: 'center', inline: 'center' });
      if (typeof el.focus === 'function') el.focus();
      el.click();
      return { ok: true, name: text(el) };
    },

    type(i, value) {
      const el = els[i - 1];
      if (!el) return { ok: false, reason: 'gone' };
      el.scrollIntoView({ block: 'center', inline: 'center' });
      el.focus();
      // The native setter, then input/change: React and Vue listen for the
      // events and re-read the property, and a plain `el.value = …` leaves
      // their state untouched — the field shows the text and the framework
      // submits an empty string.
      const proto = el instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : el instanceof HTMLSelectElement
          ? HTMLSelectElement.prototype
          : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value');
      if (setter && setter.set) setter.set.call(el, value);
      else el.value = value;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return { ok: true, value: el.value };
    },
  };

  return true;
})()
