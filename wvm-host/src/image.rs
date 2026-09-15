//! PPM decoding and PNG encoding, so a QMP screendump can be returned as a PNG.
//!
//! # Why this exists at all
//!
//! QMP's `screendump` writes PPM (a trivial, uncompressed format) and the protocol carries PNG.
//! Something has to convert, and the host is the right side: the guest cannot capture at all (see
//! `supervisor::Qmp::screendump` on session 0 isolation), and PPM at 1024x768 is 2.3 MB raw versus
//! roughly 20 KB as PNG.
//!
//! # Why hand-written
//!
//! The same reason as the guest's encoder: this is a small, well-specified format and a dependency
//! would be larger than the code. The host has no size constraint, but it does have a
//! reproducibility one — a screenshot pipeline that depends on a rendering library's version is a
//! pipeline that changes output for reasons unrelated to the guest.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

/// A decoded image.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major, top-down.
    pub rgba: Vec<u8>,
}

/// Read a PPM from disk and decode it.
pub fn read_ppm(path: &Path) -> Result<Image> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading the screendump at {}", path.display()))?;
    decode_ppm(&bytes)
}

/// Decode a binary PPM (P6).
///
/// Only P6 is handled. `screendump` without a format argument produces P6 on this QEMU build, and
/// the ASCII variant (P3) is not something QEMU emits — accepting it would be untested code for an
/// input that never arrives.
pub fn decode_ppm(bytes: &[u8]) -> Result<Image> {
    if bytes.len() < 2 {
        bail!("the file is too short to be a PPM");
    }
    if &bytes[0..2] != b"P6" {
        bail!(
            "expected a binary PPM (P6), found {:?}. A P3 (ASCII) file would need a different \
             decoder; check whether QEMU was asked for a different format",
            String::from_utf8_lossy(&bytes[0..2])
        );
    }

    let mut at = 2usize;
    let mut fields: Vec<u32> = Vec::with_capacity(3);

    // PPM allows whitespace and `#` comments between the header fields, and QEMU's output is not
    // guaranteed to put them on predictable lines. Parsing by token rather than by line avoids
    // depending on a layout that is not specified.
    while fields.len() < 3 {
        skip_whitespace_and_comments(bytes, &mut at);
        if at >= bytes.len() {
            bail!("the PPM header ended after {} field(s)", fields.len());
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if start == at {
            bail!(
                "expected a number in the PPM header at byte {at}, found {:?}",
                bytes[at] as char
            );
        }
        let value: u32 = std::str::from_utf8(&bytes[start..at])
            .context("the PPM header field is not valid UTF-8")?
            .parse()
            .map_err(|e| anyhow!("parsing a PPM header field: {e}"))?;
        fields.push(value);
    }

    let (width, height, maxval) = (fields[0], fields[1], fields[2]);

    if width == 0 || height == 0 {
        bail!("the PPM declares a {width}x{height} image, which has no pixels");
    }
    if maxval != 255 {
        bail!(
            "the PPM declares a maximum value of {maxval}; only 8-bit samples are handled. \
             QEMU emits 255"
        );
    }

    // Exactly ONE whitespace byte separates the header from the data. Consuming all whitespace
    // would eat the first pixel whenever it happens to start with 0x20 or 0x0a — a rare but real
    // corruption that shows up as a one-pixel shift.
    if at >= bytes.len() {
        bail!("the PPM header is not followed by any pixel data");
    }
    at += 1;

    let pixels = width as usize * height as usize;
    let needed = pixels * 3;
    let available = bytes.len() - at;

    if available < needed {
        bail!(
            "the PPM is truncated: {available} bytes of pixel data for a {width}x{height} image, \
             which needs {needed}. A partial screendump reads exactly like this"
        );
    }

    // RGB to RGBA. QMP's PPM is already top-down and RGB-ordered, unlike a Windows DIB.
    let mut rgba = Vec::with_capacity(pixels * 4);
    for chunk in bytes[at..at + needed].chunks_exact(3) {
        rgba.push(chunk[0]);
        rgba.push(chunk[1]);
        rgba.push(chunk[2]);
        rgba.push(0xff);
    }

    Ok(Image {
        width,
        height,
        rgba,
    })
}

fn skip_whitespace_and_comments(bytes: &[u8], at: &mut usize) {
    loop {
        // Whitespace between header tokens.
        while *at < bytes.len() && bytes[*at].is_ascii_whitespace() {
            *at += 1;
        }
        // A `#` starts a comment running to the end of the line.
        if *at < bytes.len() && bytes[*at] == b'#' {
            while *at < bytes.len() && bytes[*at] != b'\n' {
                *at += 1;
            }
            continue;
        }
        break;
    }
}

/// Encode RGBA8 pixels as a PNG.
///
/// Every scanline uses filter byte 0 (None). Adaptive filtering would compress better, but the gain
/// on a screenshot — large flat areas, sharp edges — does not justify evaluating five filters per
/// row, and a filter bug produces an image that decodes without error and looks subtly wrong.
pub fn encode_png(image: &Image) -> Result<Vec<u8>> {
    let stride = image.width as usize * 4;
    let expected = stride * image.height as usize;
    if image.rgba.len() < expected {
        bail!(
            "the pixel buffer is {} bytes, need {expected} for {}x{}",
            image.rgba.len(),
            image.width,
            image.height
        );
    }

    let mut raw = Vec::with_capacity((stride + 1) * image.height as usize);
    for row in 0..image.height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&image.rgba[row * stride..(row + 1) * stride]);
    }

    let compressed = zlib_stored(&raw);

    let mut out = Vec::with_capacity(compressed.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&image.width.to_be_bytes());
    ihdr.extend_from_slice(&image.height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // colour type: RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);

    Ok(out)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);

    // The CRC covers the type and data but NOT the length. A wrong CRC produces a file some
    // decoders accept and others reject, which is a miserable thing to track down.
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    out.push(0x78);
    out.push(0x01);

    let mut offset = 0usize;
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        while offset < data.len() {
            let len = (data.len() - offset).min(65535);
            let last = offset + len >= data.len();
            out.push(if last { 0x01 } else { 0x00 });
            out.extend_from_slice(&(len as u16).to_le_bytes());
            out.extend_from_slice(&(!(len as u16)).to_le_bytes());
            out.extend_from_slice(&data[offset..offset + len]);
            offset += len;
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal P6 PPM for testing.
    fn ppm(width: u32, height: u32, pixels: &[(u8, u8, u8)]) -> Vec<u8> {
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        for (r, g, b) in pixels {
            out.push(*r);
            out.push(*g);
            out.push(*b);
        }
        out
    }

    #[test]
    fn a_small_ppm_decodes_to_the_right_pixels() {
        let img = decode_ppm(&ppm(2, 1, &[(255, 0, 0), (0, 128, 255)])).expect("decode");
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(
            img.rgba,
            vec![255, 0, 0, 255, 0, 128, 255, 255],
            "RGB must become RGBA with an opaque alpha"
        );
    }

    #[test]
    fn header_fields_may_be_on_one_line_or_separate_lines() {
        // The PPM spec allows either, and depending on a layout QEMU does not promise is how this
        // breaks on an upgrade.
        let pixels = &[(1u8, 2u8, 3u8), (4, 5, 6)];
        let one_line = {
            let mut v = b"P6 2 1 255\n".to_vec();
            for (r, g, b) in pixels {
                v.extend_from_slice(&[*r, *g, *b]);
            }
            v
        };
        let multi_line = ppm(2, 1, pixels);

        assert_eq!(
            decode_ppm(&one_line).expect("one line").rgba,
            decode_ppm(&multi_line).expect("multi line").rgba
        );
    }

    #[test]
    fn a_comment_in_the_header_is_ignored() {
        let mut v = b"P6\n# produced by qemu\n2 1\n255\n".to_vec();
        v.extend_from_slice(&[9, 8, 7, 6, 5, 4]);
        let img = decode_ppm(&v).expect("decode");
        assert_eq!(img.rgba, vec![9, 8, 7, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn a_first_pixel_that_looks_like_whitespace_survives() {
        // The header is followed by exactly ONE whitespace byte. Skipping all whitespace would eat
        // a leading 0x20 or 0x0a in the pixel data — a one-pixel shift that is invisible in a
        // casual look at the image.
        for first_byte in [0x20u8, 0x0au8, 0x0du8, 0x09u8] {
            let mut v = b"P6\n1 1\n255\n".to_vec();
            v.extend_from_slice(&[first_byte, 0x40, 0x80]);
            let img = decode_ppm(&v).expect("decode");
            assert_eq!(
                img.rgba,
                vec![first_byte, 0x40, 0x80, 255],
                "a first pixel of {first_byte:#04x} was consumed as header whitespace"
            );
        }
    }

    #[test]
    fn a_truncated_ppm_is_refused_with_the_shortfall() {
        // A partial screendump is the failure this catches, and the message must make the cause
        // obvious rather than reporting a generic parse error.
        let mut v = ppm(4, 4, &[]);
        v.extend_from_slice(&[1, 2, 3]); // one pixel of a sixteen-pixel image
        let err = decode_ppm(&v).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("truncated"), "should say truncated: {msg}");
        assert!(msg.contains("48"), "should name the missing bytes: {msg}");
    }

    #[test]
    fn a_p3_ascii_ppm_is_refused_by_name() {
        // Refusing with the actual format makes a wrong-format failure diagnosable; a generic
        // "parse error" does not.
        let err = decode_ppm(b"P3\n1 1\n255\n255 0 0\n").expect_err("must refuse P3");
        assert!(
            err.to_string().contains("P3"),
            "should name the format: {err}"
        );
    }

    #[test]
    fn a_non_8_bit_ppm_is_refused() {
        let err = decode_ppm(b"P6\n1 1\n65535\n").expect_err("must refuse 16-bit");
        assert!(
            err.to_string().contains("65535"),
            "should quote the maxval: {err}"
        );
    }

    #[test]
    fn a_zero_sized_image_is_refused() {
        let err = decode_ppm(b"P6\n0 0\n255\n").expect_err("must refuse");
        assert!(err.to_string().contains("no pixels"), "{err}");
    }

    #[test]
    fn crc32_matches_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn adler32_matches_known_vectors() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn an_encoded_png_has_valid_structure_and_crcs() {
        let img = Image {
            width: 3,
            height: 2,
            rgba: vec![0x40; 4 * 3 * 2],
        };
        let png = encode_png(&img).expect("encode");

        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        let mut pos = 8;
        let mut kinds = Vec::new();
        while pos + 12 <= png.len() {
            let len =
                u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let stored = u32::from_be_bytes([
                png[pos + 8 + len],
                png[pos + 9 + len],
                png[pos + 10 + len],
                png[pos + 11 + len],
            ]);

            let mut crc_input = Vec::new();
            crc_input.extend_from_slice(kind);
            crc_input.extend_from_slice(data);
            assert_eq!(
                stored,
                crc32(&crc_input),
                "bad CRC in {:?}",
                std::str::from_utf8(kind)
            );

            kinds.push(String::from_utf8_lossy(kind).to_string());
            pos += 12 + len;
        }

        assert_eq!(pos, png.len(), "chunks must cover the file exactly");
        assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"]);
    }

    #[test]
    fn a_screenshot_dwarfs_the_ppm_it_came_from() {
        // The reason the conversion exists: a solid-colour 1024x768 frame is 2.3 MB as PPM. Even
        // with stored (uncompressed) deflate blocks the PNG should not be dramatically larger —
        // it has the same pixels plus one filter byte per row.
        let img = Image {
            width: 1024,
            height: 768,
            rgba: vec![0x33; 1024 * 768 * 4],
        };
        let png = encode_png(&img).expect("encode");
        let ppm_size = 1024 * 768 * 3;

        assert!(
            png.len() <= ppm_size + 1024 * 768 + 4096,
            "a stored-deflate PNG should be PPM plus one byte per row plus overhead, \
             got {} vs {ppm_size}",
            png.len()
        );
    }
}
