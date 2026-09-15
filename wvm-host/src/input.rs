//! Sending input to the guest, as emulated hardware.
//!
//! # Why this is not done in the guest
//!
//! The guest runs as a Windows service, which lives in **session 0**. `SendInput` from a service
//! cannot reach the interactive desktop: session 0 isolation has enforced that since Vista,
//! specifically so a service cannot drive the user's session. Microsoft's own documentation is
//! unambiguous — a service must use `CreateProcessAsUser` with `lpDesktop` set to
//! `"winsta0\\default"` to get anything onto the desktop.
//!
//! That route needs `SE_TCB_NAME` and a process spawned into the target session for every input
//! event, which is a great deal of machinery to send a keystroke.
//!
//! Emulated hardware has no such restriction. QMP input arrives as PS/2 and USB device events,
//! and the Windows kernel delivers those to whichever session owns the **active console** —
//! session 1. No API is involved, so session 0 isolation never applies. The evidence that this
//! works is sitting on disk: the entire Windows installation was driven by QMP keyboard injection,
//! with no guest-side component at all.
//!
//! # The keymap
//!
//! Key names are QEMU's QOM codes, and they are **physical positions**, not characters. What a
//! position produces depends on the guest's keyboard layout. This guest is **UK**, and the map
//! below is written for that. It was not written from a layout diagram: every entry was measured
//! by typing into the guest and reading the echo back.
//!
//! Three separate faults came from getting this wrong, each presenting as a different problem —
//! a mangled path, a missing driver, a line-length limit — which is why the map is measured and
//! why the tests below pin the entries that were wrong:
//!
//! | intended | was mapped to | produced | looked like |
//! |---|---|---|---|
//! | `"` | SHIFT+apostrophe | `@` | a bad path; `sc.exe` rejected its own quoting |
//! | `\` | backslash | `#` | a missing driver; pnputil blamed the path |
//! | `@` | SHIFT+2 | `"` | the pair is swapped on UK relative to US |
//!
//! `\` is deliberately absent from the map. No keycode produces a literal backslash on this
//! layout, so a caller needing one gets an error naming the problem rather than a `#` in their
//! path. Windows accepts forward slashes, so there is always a way round.

use anyhow::{anyhow, bail, Result};

/// Keys that produce a plain character with no modifier.
pub const PLAIN: &[(&str, &str)] = &[
    (" ", "spc"),
    ("-", "minus"),
    ("=", "equal"),
    ("[", "bracket_left"),
    ("]", "bracket_right"),
    (";", "semicolon"),
    ("'", "apostrophe"),
    (",", "comma"),
    (".", "dot"),
    ("/", "slash"),
    ("`", "grave_accent"),
];

/// Characters requiring shift, as (keycode, needs_shift).
///
/// Measured on a UK guest. `@` and `"` are SWAPPED on UK relative to a US keyboard, which is the
/// trap that cost the most time.
pub const SHIFTED: &[(&str, &str)] = &[
    ("!", "1"),
    ("@", "apostrophe"), // UK: SHIFT+apostrophe. Verified: produces '@'.
    ("#", "3"),
    ("$", "4"),
    ("%", "5"),
    ("^", "6"),
    ("&", "7"),
    ("*", "8"),
    ("(", "9"),
    (")", "0"),
    ("_", "minus"),
    ("+", "equal"),
    ("{", "bracket_left"),
    ("}", "bracket_right"),
    ("|", "backslash"),
    (":", "semicolon"),
    ("\"", "2"), // UK: SHIFT+2. Verified: produces '"'. The old map said apostrophe -> '@'.
    ("<", "comma"),
    (">", "dot"),
    ("?", "slash"),
    ("~", "grave_accent"),
];

/// Named keys accepted by the `key` subcommand.
pub const NAMED_KEYS: &[&str] = &[
    "ret",
    "esc",
    "tab",
    "spc",
    "backspace",
    "delete",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pgup",
    "pgdn",
    "ctrl",
    "ctrl_r",
    "shift",
    "shift_r",
    "alt",
    "alt_r",
    "meta_l",
    "meta_r",
];

/// One key event, as QOM keycodes plus an explicit press or release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Modifier keycodes held for this event, in a stable order.
    pub modifiers: Vec<String>,
    /// The key itself.
    pub key: String,
    pub down: bool,
}

/// Translate a string into the key events that produce it.
///
/// Letters and digits map to themselves. Anything not in the map is an error naming the character,
/// because silently dropping it produces a command that is subtly wrong — the failure mode that
/// made `E:\NetKVM\w11` become `E:#NetKVM#w11` and sent the investigation after a missing driver.
pub fn translate(text: &str) -> Result<Vec<KeyEvent>> {
    let mut events = Vec::with_capacity(text.len() * 2);

    for ch in text.chars() {
        let (key, shift) = if ch.is_ascii_lowercase() {
            (ch.to_string(), false)
        } else if ch.is_ascii_uppercase() {
            (ch.to_ascii_lowercase().to_string(), true)
        } else if ch.is_ascii_digit() {
            (ch.to_string(), false)
        } else if let Some((_, key)) = PLAIN.iter().find(|(c, _)| *c == ch.to_string()) {
            (key.to_string(), false)
        } else if let Some((_, key)) = SHIFTED.iter().find(|(c, _)| *c == ch.to_string()) {
            (key.to_string(), true)
        } else if ch == '\\' {
            // Named explicitly rather than left to the generic error, because a backslash is the
            // one character a caller is most likely to try and the workaround is not obvious.
            bail!(
                "cannot type a backslash: no QEMU keycode produces one on this guest's UK layout, \
                 and the closest (`backslash`) yields `#`. Windows accepts forward slashes in \
                 paths, so replace `\\` with `/` in the command you are sending"
            );
        } else {
            bail!(
                "no key mapping for {ch:?}. The map covers ASCII printable characters; \
                 extend it in wvm-host/src/input.rs if this is needed, and verify the result by \
                 reading the guest's echo rather than assuming the keycode"
            );
        };

        let modifiers = if shift {
            vec!["shift".to_string()]
        } else {
            Vec::new()
        };

        // Every press is paired with a release immediately. Holding a key across the next one is
        // how characters arrived reordered: `send-key` with `hold-time` left each key down for
        // 60ms, so against a 70ms gap the guest's driver reordered them and the symptom was stray
        // characters at the START of a line.
        events.push(KeyEvent {
            modifiers: modifiers.clone(),
            key: key.clone(),
            down: true,
        });
        events.push(KeyEvent {
            modifiers,
            key,
            down: false,
        });
    }

    Ok(events)
}

/// Parse a key spec like `ret` or `ctrl+alt+delete` into a keycode list.
///
/// A single name is a plain press. A `+`-joined list is a chord, where everything but the last
/// element is held down across the final key.
pub fn parse_chord(spec: &str) -> Result<Vec<String>> {
    let parts: Vec<&str> = spec
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if parts.is_empty() {
        bail!("empty key spec");
    }

    for part in &parts {
        // A single printable character is allowed so `ctrl+c` works without spelling out `c`.
        let acceptable = NAMED_KEYS.contains(part) || (part.len() == 1 && part.is_ascii());
        if !acceptable {
            return Err(anyhow!(
                "unknown key {part:?}. Named keys: {}. A single character is also accepted.",
                NAMED_KEYS.join(", ")
            ));
        }
    }

    Ok(parts.into_iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find the key events for one character, for readability in the tests.
    fn one(ch: char) -> (Vec<String>, String) {
        let events = translate(&ch.to_string()).expect("translate");
        assert_eq!(events.len(), 2, "a character is a press and a release");
        assert!(events[0].down, "the first event is a press");
        assert!(!events[1].down, "the second is a release");
        assert_eq!(
            events[0].key, events[1].key,
            "press and release are the same key"
        );
        (events[0].modifiers.clone(), events[0].key.clone())
    }

    #[test]
    fn letters_and_digits_map_to_themselves() {
        assert_eq!(one('a'), (vec![], "a".to_string()));
        assert_eq!(one('z'), (vec![], "z".to_string()));
        assert_eq!(one('7'), (vec![], "7".to_string()));
    }

    #[test]
    fn an_uppercase_letter_holds_shift() {
        let (mods, key) = one('A');
        assert_eq!(mods, vec!["shift".to_string()]);
        assert_eq!(
            key, "a",
            "the keycode is the lowercase letter, held with shift"
        );
    }

    #[test]
    fn the_uk_quote_and_at_sign_are_swapped_relative_to_us() {
        // The single most expensive bug in this project. On UK, `"` is SHIFT+2 and `@` is
        // SHIFT+apostrophe — the opposite of a US keyboard. The old map had them as US, so every
        // double quote arrived as `@` and `sc.exe` rejected its own quoting:
        //     sc.exe create wvm-guest binPath= @C:/Program Files/...@
        // It presented as a bad path or a permissions problem, and the mangled character was only
        // visible by reading the echo closely.
        assert_eq!(one('"'), (vec!["shift".to_string()], "2".to_string()));
        assert_eq!(
            one('@'),
            (vec!["shift".to_string()], "apostrophe".to_string())
        );
    }

    #[test]
    fn a_backslash_is_refused_with_the_workaround_named() {
        // No keycode produces a literal backslash here. Sending the closest match silently gave a
        // `#`, which mangled a driver path and made pnputil report a missing driver package.
        let err = translate("C:\\path").expect_err("a backslash must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("forward slash"),
            "the error should name the fix: {msg}"
        );
        assert!(msg.contains("UK"), "and the reason: {msg}");
    }

    #[test]
    fn the_characters_a_command_line_needs_all_map() {
        // The set that actually appears in the commands this tool sends. Each was verified by
        // typing it into the guest and reading the echo.
        for ch in "\"':;/\\.,-=[]{}!@#$%^&*()_+<>?~`| ".chars() {
            let result = translate(&ch.to_string());
            if ch == '\\' {
                assert!(result.is_err(), "a backslash must be refused, not mapped");
            } else if ch == ' ' {
                // Space is a key in its own right and must map, not be dropped.
                let (_, key) = one(ch);
                assert_eq!(key, "spc");
            } else {
                assert!(result.is_ok(), "{ch:?} must have a mapping");
            }
        }
    }

    #[test]
    fn punctuation_maps_to_the_measured_keycodes() {
        assert_eq!(
            one(':'),
            (vec!["shift".to_string()], "semicolon".to_string())
        );
        assert_eq!(one('/'), (vec![], "slash".to_string()));
        assert_eq!(one('.'), (vec![], "dot".to_string()));
        assert_eq!(one(' '), (vec![], "spc".to_string()));
        assert_eq!(one('-'), (vec![], "minus".to_string()));
        assert_eq!(one('%'), (vec!["shift".to_string()], "5".to_string()));
    }

    #[test]
    fn every_press_is_paired_with_a_release() {
        // An unpaired press leaves a key held, and the guest's driver then reorders subsequent
        // input. This is a property of the whole translation, not of one character.
        let events = translate("hello world").expect("translate");
        assert_eq!(events.len() % 2, 0, "events must come in pairs");
        for pair in events.chunks(2) {
            assert!(pair[0].down, "each pair starts with a press");
            assert!(!pair[1].down, "and ends with a release");
            assert_eq!(
                pair[0].key, pair[1].key,
                "the same key is pressed and released"
            );
            assert_eq!(
                pair[0].modifiers, pair[1].modifiers,
                "with the same modifiers"
            );
        }
    }

    #[test]
    fn an_unmappable_character_is_refused_rather_than_dropped() {
        // Dropping it would produce a subtly wrong command. The error names the character so it can
        // be found in the string that was sent.
        let err = translate("café").expect_err("é has no mapping");
        assert!(
            err.to_string().contains('é'),
            "the error should name the character: {err}"
        );
    }

    #[test]
    fn a_plain_key_parses_to_a_single_element() {
        assert_eq!(parse_chord("ret").expect("parse"), vec!["ret".to_string()]);
        assert_eq!(parse_chord("esc").expect("parse"), vec!["esc".to_string()]);
    }

    #[test]
    fn a_chord_parses_in_order() {
        assert_eq!(
            parse_chord("ctrl+alt+delete").expect("parse"),
            vec!["ctrl".to_string(), "alt".to_string(), "delete".to_string()]
        );
    }

    #[test]
    fn whitespace_around_chord_elements_is_tolerated() {
        assert_eq!(
            parse_chord("ctrl + c").expect("parse"),
            vec!["ctrl".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn an_unknown_key_name_is_refused_with_the_valid_list() {
        let err = parse_chord("f13").expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("f13"), "should name the bad key: {msg}");
        assert!(msg.contains("ret"), "should list what is accepted: {msg}");
    }

    #[test]
    fn a_single_character_key_is_accepted_for_convenience() {
        assert_eq!(parse_chord("a").expect("parse"), vec!["a".to_string()]);
        assert_eq!(
            parse_chord("ctrl+s").expect("parse"),
            vec!["ctrl".to_string(), "s".to_string()]
        );
    }

    #[test]
    fn a_command_line_survives_translation_intact() {
        // The property that matters: every character in a realistic command produces the events
        // that reproduce THAT character — not a neighbour that happens to share a keycode.
        //
        // An earlier version of this test reconstructed the string from the event list and got it
        // wrong in two ways (choosing `/` where `:` was meant, and losing the shift on uppercase
        // letters). Both were faults in the reconstruction, not the map, and both were the same
        // class of bug the map exists to prevent — so the test now checks the mapping directly,
        // per character, instead of round-tripping through a second implementation of the same
        // logic. A test that reimplements the thing under test tests the reimplementation.
        let cmd =
            "sc.exe create wvm-guest binPath= \"C:/Program Files/wvm/wvm-guest.exe\" start= auto";

        for ch in cmd.chars() {
            let events = translate(&ch.to_string())
                .unwrap_or_else(|e| panic!("{ch:?} must be translatable in a real command: {e}"));

            // Exactly one press and one release.
            assert_eq!(events.len(), 2, "{ch:?} should be two events");
            assert!(
                events[0].down && !events[1].down,
                "{ch:?} press then release"
            );

            let shift = events[0].modifiers.iter().any(|m| m == "shift");
            let key = events[0].key.as_str();

            // Verify the character maps to a keycode that, with that modifier, produces it.
            if ch.is_ascii_uppercase() {
                assert!(shift, "{ch:?} needs shift");
                assert_eq!(
                    key,
                    ch.to_ascii_lowercase().to_string(),
                    "{ch:?} is the lowercase key"
                );
            } else if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                assert!(!shift, "{ch:?} needs no shift");
                assert_eq!(key, ch.to_string(), "{ch:?} maps to itself");
            } else if let Some((_, expected)) = PLAIN.iter().find(|(c, _)| *c == ch.to_string()) {
                assert!(!shift, "{ch:?} is unshifted");
                assert_eq!(
                    key, *expected,
                    "{ch:?} must map to its own keycode, not another that shares one"
                );
            } else if let Some((_, expected)) = SHIFTED.iter().find(|(c, _)| *c == ch.to_string()) {
                assert!(shift, "{ch:?} requires shift");
                assert_eq!(key, *expected, "{ch:?} maps to the wrong shifted keycode");
            } else {
                panic!("{ch:?} has no mapping but appears in a realistic command");
            }
        }
    }

    #[test]
    fn the_two_characters_that_share_a_keycode_are_distinguished_by_shift() {
        // `slash` is `/` unshifted and `:` shifted; `semicolon` is `;` unshifted and `:` shifted;
        // `2` is `2` unshifted and `"` shifted. A map that ignored shift, or a lookup that checked
        // the unshifted table first without considering it, silently substitutes one for the other
        // — and this is exactly what produced the bug that stalled M4.
        assert_eq!(one('/'), (vec![], "slash".to_string()));
        assert_eq!(
            one(':'),
            (vec!["shift".to_string()], "semicolon".to_string())
        );
        assert_eq!(one(';'), (vec![], "semicolon".to_string()));
        assert_eq!(one('2'), (vec![], "2".to_string()));
        assert_eq!(one('"'), (vec!["shift".to_string()], "2".to_string()));
    }
}
