//! What a key press sends to the program in a shell tab (#14): the bytes an xterm-compatible
//! terminal sends, given the modes the program set ([`TermMode`]).
//!
//! Only keys that do not type text are encoded here: Return, Tab, Esc, Backspace, the arrows and
//! the other editing keys, function keys, Ctrl combinations, and Alt as Meta. Plain text (letters,
//! Shift-letters, and characters composed with Option on macOS or with an input method) is left
//! to the platform's text input, which hands it to the view as typed text.
//!
//! - **Cursor keys** send `ESC O A` in application cursor mode (`DECCKM`, which full-screen
//!   programs such as `vi`, `less` and `top` turn on) and `ESC [ A` otherwise. With modifiers
//!   they send xterm's `ESC [ 1 ; m A`, where `m` is 1 plus 1 for Shift, 2 for Alt and 4 for Ctrl.
//! - **Ctrl** with a letter sends its control character (Ctrl-C is 0x03), and Ctrl with `@`,
//!   `[`, `\`, `]`, `^`, `_`, `?` (or the digits 2 to 8 standing for them, as xterm does) the
//!   control characters past the letters.
//! - **Alt as Meta** prefixes ESC (Alt-B sends `ESC b`). It is on where Alt does not compose text
//!   ([`ALT_IS_META`]): off on macOS, where Option types characters such as `@` on many
//!   keyboards, so Option-letters go to the text input and type what the keyboard composes.
//! - **Cmd** (the platform key) never reaches the program: it is the app's.

use flint_core::terminal::TermMode;
use gpui_kit::Keystroke;

/// Whether Alt sends ESC before a key (Meta). Off on macOS, where Option composes characters.
pub const ALT_IS_META: bool = cfg!(not(target_os = "macos"));

/// The bytes to send for `keystroke`, or `None` when it types text (the text input handles it)
/// or is not the program's (Cmd shortcuts, unknown keys).
pub fn to_bytes(keystroke: &Keystroke, mode: TermMode, alt_is_meta: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // xterm's modifier parameter: 1 plus Shift 1, Alt 2, Ctrl 4.
    let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let meta = m.alt && alt_is_meta;
    let esc = |rest: &[u8]| -> Vec<u8> {
        let mut out = vec![0x1b];
        out.extend_from_slice(rest);
        out
    };
    // A cursor-style key: `ESC O x` / `ESC [ x` plain, `ESC [ 1 ; m x` with modifiers.
    let cursor = |letter: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{}", letter as char).into_bytes()
        } else if app_cursor {
            esc(&[b'O', letter])
        } else {
            esc(&[b'[', letter])
        }
    };
    // A key sent as `ESC [ n ~`, with `; m` before the tilde when modified.
    let tilde = |n: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{n};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    // F1 to F4: `ESC O P` plain, `ESC [ 1 ; m P` modified.
    let ss3 = |letter: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{}", letter as char).into_bytes()
        } else {
            esc(&[b'O', letter])
        }
    };
    let with_meta = |bytes: &[u8]| -> Vec<u8> { if meta { esc(bytes) } else { bytes.to_vec() } };
    let bytes = match keystroke.key.as_str() {
        "enter" => with_meta(b"\r"),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => with_meta(b"\t"),
        "escape" => with_meta(b"\x1b"),
        // Ctrl-Backspace erases a word in most shells as ^H.
        "backspace" if m.control => with_meta(b"\x08"),
        "backspace" => with_meta(b"\x7f"),
        "space" if m.control => with_meta(b"\0"),
        "space" if meta => esc(b" "),
        "up" => cursor(b'A'),
        "down" => cursor(b'B'),
        "right" => cursor(b'C'),
        "left" => cursor(b'D'),
        "home" => cursor(b'H'),
        "end" => cursor(b'F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => ss3(b'P'),
        "f2" => ss3(b'Q'),
        "f3" => ss3(b'R'),
        "f4" => ss3(b'S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        key => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            if m.control {
                with_meta(&[control_char(c)?])
            } else if meta {
                let c = if m.shift { c.to_ascii_uppercase() } else { c };
                let mut buf = [0; 4];
                esc(c.encode_utf8(&mut buf).as_bytes())
            } else {
                // Text: the platform's text input types it.
                return None;
            }
        }
    };
    Some(bytes)
}

/// The control character Ctrl-`c` sends, as xterm does: letters, the six symbols after them
/// in ASCII, and the digits 2 to 8 that stand for those symbols.
fn control_char(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &'static str) -> Keystroke {
        Keystroke::parse(source).unwrap()
    }

    fn send(source: &'static str) -> Option<Vec<u8>> {
        to_bytes(&key(source), TermMode::empty(), true)
    }

    fn app(source: &'static str) -> Option<Vec<u8>> {
        to_bytes(&key(source), TermMode::APP_CURSOR, true)
    }

    // covers UP-41
    #[test]
    fn editing_keys_send_their_sequences() {
        assert_eq!(send("enter").unwrap(), b"\r");
        assert_eq!(send("tab").unwrap(), b"\t");
        assert_eq!(send("shift-tab").unwrap(), b"\x1b[Z");
        assert_eq!(send("escape").unwrap(), b"\x1b");
        assert_eq!(send("backspace").unwrap(), b"\x7f");
        assert_eq!(send("ctrl-backspace").unwrap(), b"\x08");
        assert_eq!(send("delete").unwrap(), b"\x1b[3~");
        assert_eq!(send("pageup").unwrap(), b"\x1b[5~");
        assert_eq!(send("pagedown").unwrap(), b"\x1b[6~");
        assert_eq!(send("ctrl-space").unwrap(), b"\0");
    }

    // covers UP-41: full-screen programs (vi, less, top) turn application cursor keys on.
    #[test]
    fn cursor_keys_follow_the_application_cursor_mode() {
        assert_eq!(send("up").unwrap(), b"\x1b[A");
        assert_eq!(send("left").unwrap(), b"\x1b[D");
        assert_eq!(send("home").unwrap(), b"\x1b[H");
        assert_eq!(app("up").unwrap(), b"\x1bOA");
        assert_eq!(app("end").unwrap(), b"\x1bOF");
        // Modified keys use xterm's parameter form in either mode.
        assert_eq!(send("shift-up").unwrap(), b"\x1b[1;2A");
        assert_eq!(app("ctrl-right").unwrap(), b"\x1b[1;5C");
        assert_eq!(send("alt-left").unwrap(), b"\x1b[1;3D");
        assert_eq!(send("ctrl-delete").unwrap(), b"\x1b[3;5~");
    }

    #[test]
    fn function_keys_send_xterm_sequences() {
        assert_eq!(send("f1").unwrap(), b"\x1bOP");
        assert_eq!(send("f4").unwrap(), b"\x1bOS");
        assert_eq!(send("f5").unwrap(), b"\x1b[15~");
        assert_eq!(send("f12").unwrap(), b"\x1b[24~");
        assert_eq!(send("shift-f1").unwrap(), b"\x1b[1;2P");
        assert_eq!(send("ctrl-f5").unwrap(), b"\x1b[15;5~");
        assert_eq!(send("f20"), None);
    }

    // covers UP-41: Ctrl-C interrupts, Ctrl-D ends input, Ctrl-[ is Esc.
    #[test]
    fn ctrl_sends_control_characters() {
        assert_eq!(send("ctrl-c").unwrap(), [0x03]);
        assert_eq!(send("ctrl-d").unwrap(), [0x04]);
        assert_eq!(send("ctrl-a").unwrap(), [0x01]);
        assert_eq!(send("ctrl-z").unwrap(), [0x1a]);
        assert_eq!(send("ctrl-shift-r").unwrap(), [0x12]);
        assert_eq!(send("ctrl-[").unwrap(), [0x1b]);
        assert_eq!(send("ctrl-]").unwrap(), [0x1d]);
        assert_eq!(send("ctrl-\\").unwrap(), [0x1c]);
        assert_eq!(send("ctrl-2").unwrap(), [0]);
        assert_eq!(send("ctrl-8").unwrap(), [0x7f]);
        assert_eq!(send("ctrl-alt-c").unwrap(), [0x1b, 0x03]);
        assert_eq!(send("ctrl-,"), None);
    }

    #[test]
    fn alt_is_meta_only_where_it_does_not_compose_text() {
        assert_eq!(send("alt-b").unwrap(), b"\x1bb");
        assert_eq!(send("alt-shift-b").unwrap(), b"\x1bB");
        assert_eq!(send("alt-.").unwrap(), b"\x1b.");
        assert_eq!(send("alt-enter").unwrap(), b"\x1b\r");
        assert_eq!(send("alt-backspace").unwrap(), b"\x1b\x7f");
        // Without Meta (macOS), Option-letters are text for the input method.
        assert_eq!(to_bytes(&key("alt-b"), TermMode::empty(), false), None);
        assert_eq!(
            to_bytes(&key("alt-backspace"), TermMode::empty(), false).unwrap(),
            b"\x7f"
        );
    }

    #[test]
    fn text_and_app_shortcuts_are_not_sent() {
        assert_eq!(send("a"), None);
        assert_eq!(send("shift-a"), None);
        assert_eq!(send("space"), None);
        assert_eq!(send("1"), None);
        // The platform key (Cmd on macOS) is the app's.
        assert_eq!(send("cmd-c"), None);
        assert_eq!(send("cmd-up"), None);
    }
}
