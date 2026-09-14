//! A key name, expressed the way CDP needs it to deliver a real keystroke.
//!
//! `Press` used to build a `KeyboardEvent` in JavaScript and dispatch it. Such
//! an event carries `isTrusted: false` and, more to the point, has no default
//! action: pressing Enter that way does not submit the form, and Tab does not
//! move focus — which is what pressing Enter and Tab are for. The character goes
//! nowhere either, since nothing routed it into the input pipeline.
//!
//! These are the fields a real one needs: `key` and `code` for the page to read,
//! `windowsVirtualKeyCode` for the browser's own handling to recognise, and
//! `text` for keys that produce a character.

/// A key as CDP wants it.
#[derive(Debug, Clone, PartialEq)]
pub struct KeySpec {
    /// What the page sees as `event.key` — `"Enter"`, `"a"`.
    pub key: String,
    /// The physical key as `event.code` — `"Enter"`, `"KeyA"`. Some pages read
    /// this instead, and it is what a layout-independent shortcut matches on.
    pub code: String,
    /// `windowsVirtualKeyCode`, which is how the browser recognises a key well
    /// enough to act on it.
    pub vk: i64,
    /// The character this key produces, for keys that produce one. Absent means
    /// the key is handled rather than typed.
    pub text: Option<String>,
}

impl KeySpec {
    /// Whether the browser should insert a character.
    pub fn is_printable(&self) -> bool {
        self.text.is_some()
    }
}

/// A named key: the words that name it, and the values the browser expects.
struct NamedKey {
    names: &'static [&'static str],
    key: &'static str,
    code: &'static str,
    vk: i64,
    /// The character the key produces, where it produces one. Enter does, and
    /// that is not decoration: a real Enter arrives with its carriage return,
    /// and Chrome submits the form on the back of it.
    text: Option<&'static str>,
}

/// Only the keys a caller has reason to name: navigation, editing, and the two
/// that carry default behaviour everywhere.
const NAMED: &[NamedKey] = &[
    NamedKey {
        names: &["enter", "return"],
        key: "Enter",
        code: "Enter",
        vk: 13,
        text: Some("\r"),
    },
    NamedKey {
        names: &["tab"],
        key: "Tab",
        code: "Tab",
        vk: 9,
        text: None,
    },
    NamedKey {
        names: &["escape", "esc"],
        key: "Escape",
        code: "Escape",
        vk: 27,
        text: None,
    },
    NamedKey {
        names: &["backspace"],
        key: "Backspace",
        code: "Backspace",
        vk: 8,
        text: None,
    },
    NamedKey {
        names: &["delete", "del"],
        key: "Delete",
        code: "Delete",
        vk: 46,
        text: None,
    },
    NamedKey {
        names: &["arrowup", "up"],
        key: "ArrowUp",
        code: "ArrowUp",
        vk: 38,
        text: None,
    },
    NamedKey {
        names: &["arrowdown", "down"],
        key: "ArrowDown",
        code: "ArrowDown",
        vk: 40,
        text: None,
    },
    NamedKey {
        names: &["arrowleft", "left"],
        key: "ArrowLeft",
        code: "ArrowLeft",
        vk: 37,
        text: None,
    },
    NamedKey {
        names: &["arrowright", "right"],
        key: "ArrowRight",
        code: "ArrowRight",
        vk: 39,
        text: None,
    },
    NamedKey {
        names: &["home"],
        key: "Home",
        code: "Home",
        vk: 36,
        text: None,
    },
    NamedKey {
        names: &["end"],
        key: "End",
        code: "End",
        vk: 35,
        text: None,
    },
    NamedKey {
        names: &["pageup", "pgup"],
        key: "PageUp",
        code: "PageUp",
        vk: 33,
        text: None,
    },
    NamedKey {
        names: &["pagedown", "pgdn"],
        key: "PageDown",
        code: "PageDown",
        vk: 34,
        text: None,
    },
];

/// Keys that are only meaningful held down while another key is pressed.
///
/// This action sends one key at a time, so a modifier on its own does nothing a
/// page can act on. Refusing says so, rather than sending a keystroke that
/// silently has no effect.
const MODIFIERS: &[&str] = &[
    "shift", "control", "ctrl", "alt", "meta", "cmd", "command", "win",
];

/// The space key, which is a character rather than a named key but is reached
/// by naming it.
fn space() -> KeySpec {
    KeySpec {
        key: " ".to_string(),
        code: "Space".to_string(),
        vk: 32,
        text: Some(" ".to_string()),
    }
}

/// Map a caller's word for a key onto the fields CDP needs.
pub fn key_spec(name: &str) -> Result<KeySpec, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        // Whitespace on its own is the space key; nothing at all is not a key.
        if name.is_empty() {
            return Err("a key name is required".to_string());
        }
        return Ok(space());
    }
    let lowered = trimmed.to_lowercase();

    if MODIFIERS.contains(&lowered.as_str()) {
        return Err(format!(
            "`{trimmed}` is a modifier: it only does something while another key is held, and this \
             action sends one key at a time. Send the key it should modify, or use the page's own \
             controls."
        ));
    }

    if lowered == "space" {
        return Ok(space());
    }

    if let Some(named) = NAMED
        .iter()
        .find(|named| named.names.contains(&lowered.as_str()))
    {
        return Ok(KeySpec {
            key: named.key.to_string(),
            code: named.code.to_string(),
            vk: named.vk,
            text: named.text.map(str::to_string),
        });
    }

    // A single character is typed as itself. Its virtual key code only exists
    // for the characters a US layout can reach, and text is what actually
    // delivers it either way.
    let mut chars = trimmed.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let ascii = c.is_ascii_alphanumeric();
        let vk = if ascii {
            i64::from(c.to_ascii_uppercase() as u32)
        } else {
            0
        };
        let code = if c.is_ascii_alphabetic() {
            format!("Key{}", c.to_ascii_uppercase())
        } else if c.is_ascii_digit() {
            format!("Digit{c}")
        } else {
            String::new()
        };
        return Ok(KeySpec {
            key: c.to_string(),
            code,
            vk,
            text: Some(c.to_string()),
        });
    }

    Err(format!(
        "unknown key `{trimmed}` — name a key such as Enter, Tab, Escape, Backspace, Delete, \
         ArrowUp/Down/Left/Right, Home, End, PageUp/PageDown, Space, or a single character to type"
    ))
}

/// What a key step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStepKind {
    /// Press, letting a character through if the key has one.
    Down,
    /// Press without a character: for modifiers, whose whole meaning is being
    /// held while another key goes down.
    DownRaw,
    Up,
}

/// One key event in a sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyStep {
    pub kind: KeyStepKind,
    pub spec: KeySpec,
    /// CDP's modifier bitmask as held during this event — Alt 1, Control 2,
    /// Meta 4, Shift 8.
    pub modifiers: i64,
}

/// CDP's modifier bits, as the wire defines them.
pub const ALT: i64 = 1;
pub const CONTROL: i64 = 2;
pub const META: i64 = 4;
pub const SHIFT: i64 = 8;

/// Cmd+Shift.
///
/// A named constant because `META | SHIFT` inside a match arm would be an
/// *or-pattern* — matching either bit alone, not both — which silently matched
/// nothing here until a test caught it. In a const expression `|` is the
/// bitwise or that was meant.
const META_SHIFT: i64 = META | SHIFT;

/// The editing command a combination stands for, when it stands for one.
///
/// macOS runs its editing shortcuts in the browser process, not from the DOM
/// event: a synthetic Cmd+A arrives at the page with `metaKey` true and the
/// right `key`, and selects nothing, because nothing in the page ever handled
/// it. CDP carries the command alongside the event for exactly this reason
/// (`Input.dispatchKeyEvent.commands`), and without it a combination is
/// delivered and ignored — which is how this was found.
///
/// The names are WebKit's editing commands, the same vocabulary
/// `document.execCommand` uses; the bindings are macOS's. An unmatched
/// combination has no command, which is the common case and not a failure:
/// shift-click and arrow keys are handled by the page itself.
pub fn editing_command(modifiers: i64, code: &str) -> Option<&'static str> {
    let command = match (modifiers, code) {
        (META, "KeyA") => "SelectAll",
        (META, "KeyC") => "Copy",
        (META, "KeyX") => "Cut",
        (META, "KeyV") => "Paste",
        (META, "KeyZ") => "Undo",
        (META_SHIFT, "KeyZ") => "Redo",
        // The macOS binding for Control+A is not select-all: it moves the
        // caret, which is why sending it and seeing no selection is correct
        // behaviour rather than a bug.
        (CONTROL, "KeyA") => "MoveToBeginningOfParagraph",
        (META, "ArrowUp") => "MoveToBeginningOfDocument",
        (META, "ArrowDown") => "MoveToEndOfDocument",
        (META, "ArrowLeft") => "MoveToLeftEndOfLine",
        (META, "ArrowRight") => "MoveToRightEndOfLine",
        (META_SHIFT, "ArrowUp") => "MoveToBeginningOfDocumentAndModifySelection",
        (META_SHIFT, "ArrowDown") => "MoveToEndOfDocumentAndModifySelection",
        (META_SHIFT, "ArrowLeft") => "MoveToLeftEndOfLineAndModifySelection",
        (META_SHIFT, "ArrowRight") => "MoveToRightEndOfLineAndModifySelection",
        _ => return None,
    };
    Some(command)
}

/// A modifier: the bit it holds down, and the key event that holds it.
///
/// These do not go through `key_spec`, which refuses a modifier on its own —
/// correct for `Press`, which sends one key, and wrong here, where holding one
/// while another goes down is the entire point.
fn modifier_spec(name: &str) -> Option<(i64, KeySpec)> {
    let (bit, key, code, vk) = match name {
        "alt" | "option" => (ALT, "Alt", "AltLeft", 18),
        "control" | "ctrl" => (CONTROL, "Control", "ControlLeft", 17),
        "meta" | "cmd" | "command" | "win" | "super" => (META, "Meta", "MetaLeft", 91),
        "shift" => (SHIFT, "Shift", "ShiftLeft", 16),
        _ => return None,
    };
    Some((
        bit,
        KeySpec {
            key: key.to_string(),
            code: code.to_string(),
            vk,
            text: None,
        },
    ))
}

/// The steps that press and release one key.
pub fn press_steps(name: &str) -> Result<Vec<KeyStep>, String> {
    let spec = key_spec(name)?;
    let kind = if spec.is_printable() {
        KeyStepKind::Down
    } else {
        KeyStepKind::DownRaw
    };
    Ok(vec![
        KeyStep {
            kind,
            spec: spec.clone(),
            modifiers: 0,
        },
        KeyStep {
            kind: KeyStepKind::Up,
            spec,
            modifiers: 0,
        },
    ])
}

/// The steps that hold modifiers, press one key, and let go.
///
/// The order matters and so do the masks: a modifier that is not yet held when
/// the main key goes down is not a combination, and the key-up carries the
/// modifiers that were still held. That is what the browser reads, and what
/// makes Control+A select rather than type an "a".
pub fn hotkey_steps(keys: &[String]) -> Result<Vec<KeyStep>, String> {
    if keys.is_empty() {
        return Err("a hotkey needs at least one key".to_string());
    }

    let mut held: Vec<(i64, KeySpec)> = Vec::new();
    let mut main: Option<KeySpec> = None;
    for key in keys {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err("a hotkey has an empty key name in it".to_string());
        }
        if let Some((bit, spec)) = modifier_spec(&trimmed.to_lowercase()) {
            held.push((bit, spec));
            continue;
        }
        if main.is_some() {
            return Err(format!(
                "`{trimmed}` is a second key to press: a hotkey modifies one key, so send the \
                 others one at a time"
            ));
        }
        main = Some(key_spec(trimmed)?);
    }

    let Some(spec) = main else {
        return Err("a combination needs a key to modify (e.g. [\"ctrl\", \"a\"])".to_string());
    };

    let mut steps = Vec::with_capacity(held.len() * 2 + 2);
    let mut mask = 0i64;
    for (bit, modifier) in &held {
        mask |= bit;
        steps.push(KeyStep {
            kind: KeyStepKind::DownRaw,
            spec: modifier.clone(),
            modifiers: mask,
        });
    }

    let down = if spec.is_printable() {
        KeyStepKind::Down
    } else {
        KeyStepKind::DownRaw
    };
    steps.push(KeyStep {
        kind: down,
        spec: spec.clone(),
        modifiers: mask,
    });
    steps.push(KeyStep {
        kind: KeyStepKind::Up,
        spec,
        modifiers: mask,
    });

    for (bit, modifier) in held.iter().rev() {
        mask &= !bit;
        steps.push(KeyStep {
            kind: KeyStepKind::Up,
            spec: modifier.clone(),
            modifiers: mask,
        });
    }

    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_tab_carry_the_codes_the_browser_acts_on() {
        // 13 and 9 are not decoration: they are how the browser knows to submit
        // the form and to move focus.
        let enter = key_spec("Enter").unwrap();
        assert_eq!((enter.key.as_str(), enter.code.as_str(), enter.vk), ("Enter", "Enter", 13));

        // And the carriage return is not decoration either. Without it Chrome
        // delivers the key but does not submit the form — which is what the
        // integration test caught when this was sent as a bare rawKeyDown.
        assert_eq!(enter.text.as_deref(), Some("\r"), "Enter must carry its CR");

        let tab = key_spec("tab").unwrap();
        assert_eq!(tab.vk, 9);
        assert_eq!(tab.code, "Tab");
        assert_eq!(tab.text, None, "Tab is handled, never typed");
    }

    #[test]
    fn the_names_are_case_insensitive_and_have_aliases() {
        assert_eq!(key_spec("ENTER").unwrap().key, "Enter");
        assert_eq!(key_spec("Esc").unwrap().key, "Escape");
        assert_eq!(key_spec(" arrowdown ").unwrap().vk, 40);
        assert_eq!(key_spec("Down").unwrap().key, "ArrowDown");
        assert_eq!(key_spec("pgdn").unwrap().key, "PageDown");
    }

    #[test]
    fn space_is_a_character_not_a_named_key() {
        let space = key_spec("space").unwrap();
        assert_eq!(space.text.as_deref(), Some(" "));
        assert_eq!(space.vk, 32);
        assert!(space.is_printable());

        assert_eq!(key_spec(" ").unwrap().key, " ");
    }

    #[test]
    fn a_single_character_is_typed_as_itself() {
        let a = key_spec("a").unwrap();
        assert_eq!(a.text.as_deref(), Some("a"));
        assert_eq!(a.code, "KeyA");
        assert_eq!(a.vk, i64::from(b'A' as u32));
        // Case is the character's own, not folded into the key code.
        assert_eq!(key_spec("A").unwrap().text.as_deref(), Some("A"));

        let one = key_spec("1").unwrap();
        assert_eq!(one.code, "Digit1");
        assert_eq!(one.vk, i64::from(b'1' as u32));
    }

    #[test]
    fn a_character_a_us_layout_cannot_reach_still_carries_its_text() {
        // There is no virtual key code for it, and text is what delivers it.
        let e_acute = key_spec("é").unwrap();
        assert_eq!(e_acute.text.as_deref(), Some("é"));
        assert_eq!(e_acute.vk, 0);
        assert_eq!(e_acute.code, "");
    }

    #[test]
    fn a_modifier_is_refused_rather_than_sent_to_no_effect() {
        let refused = key_spec("Shift").unwrap_err();
        assert!(refused.contains("modifier"), "{refused}");
        assert!(refused.contains("one key at a time"), "{refused}");

        assert!(key_spec("ctrl").is_err());
        assert!(key_spec("Cmd").is_err());
    }

    #[test]
    fn a_word_that_is_not_a_key_is_refused_with_examples() {
        let refused = key_spec("Frobnicate").unwrap_err();
        assert!(refused.contains("Frobnicate"), "{refused}");
        assert!(refused.contains("Enter"), "{refused}");
        assert!(key_spec("").is_err());
    }

    #[test]
    fn a_hotkey_holds_its_modifier_while_the_key_goes_down() {
        let steps = hotkey_steps(&["ctrl".to_string(), "a".to_string()]).unwrap();
        let shape: Vec<(KeyStepKind, String, i64)> = steps
            .iter()
            .map(|s| (s.kind, s.spec.key.clone(), s.modifiers))
            .collect();
        assert_eq!(
            shape,
            vec![
                (KeyStepKind::DownRaw, "Control".to_string(), 2),
                (KeyStepKind::Down, "a".to_string(), 2),
                (KeyStepKind::Up, "a".to_string(), 2),
                (KeyStepKind::Up, "Control".to_string(), 0),
            ]
        );
        // The character is carried on the main key, not the modifier.
        assert_eq!(steps[1].spec.text.as_deref(), Some("a"));
        assert_eq!(steps[0].spec.text, None, "a modifier types nothing");
    }

    #[test]
    fn a_hotkey_with_a_key_that_types_nothing_stays_raw() {
        let steps = hotkey_steps(&["shift".to_string(), "Enter".to_string()]).unwrap();
        assert_eq!(steps[0].kind, KeyStepKind::DownRaw);
        assert_eq!(steps[1].kind, KeyStepKind::Down, "Enter carries its CR");
        assert_eq!(steps[1].modifiers, 8);
    }

    #[test]
    fn several_modifiers_accumulate_and_release_in_reverse() {
        let steps = hotkey_steps(&[
            "ctrl".to_string(),
            "shift".to_string(),
            "Escape".to_string(),
        ])
        .unwrap();
        let mask: Vec<i64> = steps.iter().map(|s| s.modifiers).collect();
        // Ctrl down (2), Shift joins it (10), the main key and its release
        // carry both, then Shift goes first (2 — Ctrl is still held) and Ctrl
        // last (0).
        assert_eq!(mask, vec![2, 10, 10, 10, 2, 0]);
        assert_eq!(steps.iter().filter(|s| s.kind == KeyStepKind::Up).count(), 3);
    }

    #[test]
    fn a_hotkey_needs_a_key_to_modify() {
        let refused = hotkey_steps(&["ctrl".to_string()]).unwrap_err();
        assert!(refused.contains("needs a key to modify"), "{refused}");
        assert!(hotkey_steps(&[]).unwrap_err().contains("at least one"));
    }

    #[test]
    fn a_hotkey_takes_one_key_at_a_time() {
        // Two main keys is two combinations, and sending them as one would
        // press the second while the first is still down.
        let refused =
            hotkey_steps(&["ctrl".to_string(), "a".to_string(), "b".to_string()]).unwrap_err();
        assert!(refused.contains("second key"), "{refused}");
        assert!(refused.contains("one at a time"), "{refused}");
    }

    #[test]
    fn an_unknown_key_in_a_hotkey_is_refused() {
        assert!(hotkey_steps(&["ctrl".to_string(), "Frobnicate".to_string()]).is_err());
        assert!(hotkey_steps(&["".to_string()]).is_err());
    }

    #[test]
    fn a_single_press_is_a_down_and_an_up() {
        let steps = press_steps("Tab").unwrap();
        assert_eq!(steps[0].kind, KeyStepKind::DownRaw);
        assert_eq!(steps[1].kind, KeyStepKind::Up);
        assert!(press_steps("Frobnicate").is_err());
    }

    #[test]
    fn a_mac_editing_shortcut_carries_its_command() {
        // Cmd+A selects all; the event alone does not make that happen.
        assert_eq!(editing_command(META, "KeyA"), Some("SelectAll"));
        assert_eq!(editing_command(META, "KeyC"), Some("Copy"));
        assert_eq!(editing_command(META, "KeyV"), Some("Paste"));
        assert_eq!(editing_command(META, "KeyZ"), Some("Undo"));
        assert_eq!(editing_command(META_SHIFT, "KeyZ"), Some("Redo"));
    }

    #[test]
    fn control_a_on_mac_is_not_select_all() {
        // It moves the caret, and saying so is better than leaving a caller to
        // conclude the combination was delivered wrongly.
        assert_eq!(editing_command(CONTROL, "KeyA"), Some("MoveToBeginningOfParagraph"));
        assert_ne!(editing_command(CONTROL, "KeyA"), Some("SelectAll"));
    }

    #[test]
    fn an_unbound_combination_has_no_command() {
        assert_eq!(editing_command(META, "KeyB"), None);
        assert_eq!(editing_command(0, "KeyA"), None, "a bare a types");
        assert_eq!(
            editing_command(META | CONTROL, "KeyA"),
            None,
            "extra modifiers make it a different shortcut"
        );
    }
}
