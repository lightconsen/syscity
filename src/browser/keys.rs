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
}
