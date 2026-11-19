//! Text to keystrokes, for `type_text`: each character goes through the window's key handling
//! as if typed, so key bindings and the focused input see what a keyboard would send.

use gpui_kit::{Keystroke, Modifiers};

/// One keystroke per character. A newline is Enter and a tab is Tab (no text); anything else
/// types itself.
pub fn keystrokes(text: &str) -> Vec<Keystroke> {
    text.chars()
        .map(|c| match c {
            '\n' => named("enter"),
            '\t' => named("tab"),
            ' ' => Keystroke {
                modifiers: Modifiers::default(),
                key: "space".to_owned(),
                key_char: Some(" ".to_owned()),
            },
            c => Keystroke {
                modifiers: Modifiers {
                    shift: shifted(c),
                    ..Modifiers::default()
                },
                key: c.to_lowercase().collect(),
                key_char: Some(c.to_string()),
            },
        })
        .collect()
}

/// Whether typing `c` on a US keyboard takes Shift: capitals and the shifted symbols.
pub fn shifted(c: char) -> bool {
    c.is_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(c)
}

/// A key pressed by name (`press_key`): a single shifted character gets Shift, as a keyboard
/// would send it.
pub fn with_shift(mut keystroke: Keystroke) -> Keystroke {
    let mut chars = keystroke.key.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && shifted(c)
    {
        keystroke.modifiers.shift = true;
        if keystroke.key_char.is_none() && !keystroke.modifiers.platform {
            keystroke.key_char = Some(c.to_string());
        }
    }
    keystroke
}

fn named(key: &str) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::default(),
        key: key.to_owned(),
        key_char: None,
    }
}

#[cfg(test)]
mod tests {
    use super::keystrokes;

    #[test]
    fn characters_become_keystrokes() {
        let keys = keystrokes("aB \n");
        let keys: Vec<(&str, Option<&str>, bool)> = keys
            .iter()
            .map(|k| (k.key.as_str(), k.key_char.as_deref(), k.modifiers.shift))
            .collect();
        assert_eq!(
            keys,
            [
                ("a", Some("a"), false),
                ("b", Some("B"), true),
                ("space", Some(" "), false),
                ("enter", None, false),
            ]
        );
        assert!(keystrokes("").is_empty());
        assert!(keystrokes("?")[0].modifiers.shift);
        assert!(!keystrokes("/")[0].modifiers.shift);
    }

    #[test]
    fn named_shifted_symbols_get_shift() {
        use gpui_kit::Keystroke;
        let key = |k: &str| {
            super::with_shift(Keystroke {
                modifiers: Default::default(),
                key: k.into(),
                key_char: None,
            })
        };
        assert!(key("?").modifiers.shift);
        assert_eq!(key("?").key_char.as_deref(), Some("?"));
        assert!(!key("a").modifiers.shift);
        assert!(!key("escape").modifiers.shift);
    }
}
