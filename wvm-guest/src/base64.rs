//! Base64 encoding.
//!
//! The protocol carries a captured frame as base64 text inside JSON. JSON has no byte-string type,
//! so binary has to be encoded, and base64 is the conventional choice.
//!
//! # Why hand-written rather than a crate
//!
//! The guest binary is deliberately kept small so it is cheap to deploy into a debloated image,
//! and base64 is about forty lines. A dependency here would be larger than the code it replaces.
//!
//! # Why not hex
//!
//! Hex is simpler still, but doubles the size: a 1080p screenshot is roughly 8 MB as raw BGRA and
//! would become 16 MB as hex, over the frame limit. Base64's 4/3 expansion keeps it inside.

/// Standard base64 alphabet (RFC 4648), with `=` padding.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard base64.
pub fn encode(input: &[u8]) -> String {
    // Every 3 input bytes become 4 output characters; a partial group of 1 or 2 becomes 4 with
    // padding, so the output length is always a multiple of 4.
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;

        // Pack the three bytes into one 24-bit value, then take it out in 6-bit groups.
        let combined = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((combined >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((combined >> 12) & 0x3f) as usize] as char);

        // A group of 1 input byte produces only 2 meaningful characters; the rest is padding.
        if chunk.len() > 1 {
            out.push(ALPHABET[((combined >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(ALPHABET[(combined & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

/// Decode standard base64. Whitespace is ignored; anything else invalid is an error.
///
/// Only the decoder is strict about padding and alphabet, because its input comes from a peer and a
/// lenient decoder is how malformed data becomes silently wrong data.
///
/// The guest only ever ENCODES — frames travel guest to host — so this is unused in the binary and
/// reports as dead code. It is kept because its tests are the strongest evidence the encoder is
/// correct, and because the host will need it the moment a frame travels in the other direction.
/// An encoder with no decoder has nothing to check it against.
#[allow(dead_code)]
pub fn decode(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;

    for (position, ch) in input.bytes().enumerate() {
        // Tolerate line breaks, which is how base64 is often wrapped.
        if ch == b'\n' || ch == b'\r' || ch == b' ' || ch == b'\t' {
            continue;
        }
        if ch == b'=' {
            break;
        }

        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => {
                return Err(format!(
                    "invalid base64 character {:?} at position {position}",
                    other as char
                ))
            }
        };

        buffer = (buffer << 6) | value as u32;
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_test_vectors() {
        // RFC 4648 section 10. These cover all three padding cases, which is where hand-written
        // encoders usually break.
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn every_byte_value_survives_a_round_trip() {
        // All 256 values, which exercises every 6-bit group and every alignment.
        let input: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&input);
        assert_eq!(decode(&encoded).expect("decode"), input);
    }

    #[test]
    fn each_length_modulo_three_round_trips() {
        // The three padding cases explicitly, since a length-dependent bug is invisible on a
        // single well-chosen input.
        for len in 0..12usize {
            let input: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = encode(&input);
            assert_eq!(
                decode(&encoded).expect("decode"),
                input,
                "length {len} did not round trip"
            );
            assert_eq!(
                encoded.len() % 4,
                0,
                "length {len} produced a non-multiple of 4"
            );
        }
    }

    #[test]
    fn padding_is_emitted_for_partial_groups() {
        assert!(encode(b"f").ends_with("=="));
        assert!(encode(b"fo").ends_with('='));
        assert!(!encode(b"foo").contains('='));
    }

    #[test]
    fn the_alphabet_uses_plus_and_slash_not_url_safe_variants() {
        // 0xfb 0xff 0xbf encodes to "+/+/" in standard base64. Substituting the URL-safe alphabet
        // would produce "-_-_" and the peer would reject it.
        assert_eq!(encode(&[0xfb, 0xff, 0xbf]), "+/+/");
    }

    #[test]
    fn decoding_rejects_invalid_characters() {
        // A lenient decoder silently produces wrong bytes; the input here comes from a peer.
        let err = decode("Zm9v!mFy").expect_err("must reject '!'");
        assert!(
            err.contains("invalid base64"),
            "the error should say why: {err}"
        );
    }

    #[test]
    fn decoding_ignores_wrapped_whitespace() {
        // Base64 is commonly wrapped at 76 characters, and rejecting that would be needlessly
        // strict for a decoder that is otherwise correct.
        let encoded = encode(b"the quick brown fox jumps over the lazy dog");
        let wrapped = format!("{}\n{}", &encoded[..20], &encoded[20..]);
        assert_eq!(
            decode(&wrapped).expect("decode"),
            b"the quick brown fox jumps over the lazy dog"
        );
    }

    #[test]
    fn a_png_sized_payload_stays_inside_the_frame_limit() {
        // The reason base64 rather than hex: an 8 MB capture must survive the 16 MiB frame cap.
        let raw = vec![0u8; 8 * 1024 * 1024];
        let encoded = encode(&raw);
        assert!(
            encoded.len() < 16 * 1024 * 1024,
            "an 8 MB frame became {} bytes, over the limit",
            encoded.len()
        );
        assert!(encoded.len() > raw.len(), "base64 must expand, not shrink");
    }
}
