//! Screen capture.
//!
//! Grabs the guest's screen and returns it as a PNG.
//!
//! # Why the Win32 route rather than QMP `screendump`
//!
//! QEMU can capture the framebuffer directly over QMP, and this project already uses that
//! (`scripts/qmp_input.py`). It is simpler and needs nothing installed in the guest.
//!
//! It is also available to anything that can reach the QMP socket, which is the wrong shape for
//! this project: authority here is decided by the **host** against a capability grant, and every
//! operation goes through the journal. A capture that bypasses the guest is a second path to the
//! same data with no audit trail and no grant check, and two paths to one capability is exactly the
//! arrangement this design exists to avoid.
//!
//! So the guest captures its own screen. The consequence is that the guest must be able to render
//! — a headless guest with no display device returns an honest error rather than a black image,
//! because a black image is indistinguishable from a legitimately dark screen.
//!
//! # Why the encoders are dead code on the host
//!
//! PNG encoding and its two checksums are called only from the `#[cfg(windows)]` capture path, so a
//! host build reports them unused while testing them fully. The allow is scoped to this module so
//! genuine dead code elsewhere is still reported.
#![allow(dead_code)] // used by the #[cfg(windows)] capture path; tested on the host

use anyhow::{anyhow, Result};

/// A captured frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// PNG bytes.
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Largest frame we will produce, in bytes.
///
/// A 4K screenshot compresses to a few hundred KB for typical content, but a noisy image (a
/// photograph, a full-screen video) can be far larger. The protocol's frame limit is 16 MiB; this
/// sits under it so a capture cannot produce a frame the transport will refuse to carry.
pub const MAX_PNG_BYTES: usize = 12 * 1024 * 1024;

/// Capture the screen.
///
/// On non-Windows builds this reports the platform as absent rather than fabricating a frame. A
/// stub that returns a plausible blank image would be worse than useless: it would look like a
/// working capture of a black screen.
#[cfg(not(windows))]
pub fn capture(_monitor: u8) -> Result<Frame> {
    Err(anyhow!(
        "screen capture requires a Windows build of wvm-guest; \
         this binary was built for the host, where it exists to be type-checked and tested \
         rather than run"
    ))
}

#[cfg(windows)]
pub fn capture(_monitor: u8) -> Result<Frame> {
    use anyhow::Context;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };

    unsafe {
        // The screen device context. `GetDC(0)` is the entire virtual screen.
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return Err(anyhow!(
                "GetDC failed: no screen device context is available"
            ));
        }

        // The virtual screen, not the primary monitor: coordinates can be negative when a
        // multi-monitor arrangement places a display to the left of the origin, and using
        // `GetSystemMetrics(SM_CXSCREEN)` would silently crop such an arrangement.
        let x = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN,
        );
        let y = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN,
        );
        let width = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
        ) as u32;
        let height = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
        ) as u32;

        if width == 0 || height == 0 {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            // An honest refusal. A zero-sized virtual screen means there is no display device,
            // which in a headless guest is expected rather than exceptional — and returning a
            // 1x1 black pixel would look like a successful capture of a blank screen.
            return Err(anyhow!(
                "the virtual screen is {width}x{height}, so there is nothing to capture. \
                 A guest with no display device cannot be screenshotted; this is expected for a \
                 fully headless VM and is not a fault in the request"
            ));
        }

        let memory_dc = CreateCompatibleDC(screen_dc);
        if memory_dc.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(anyhow!("CreateCompatibleDC failed"));
        }

        let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
        if bitmap.is_null() {
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(anyhow!(
                "CreateCompatibleBitmap failed for {width}x{height}"
            ));
        }

        let previous = SelectObject(memory_dc, bitmap as _);

        // SRCCOPY takes the pixels as they are. CAPTUREBLT would also include layered windows, but
        // it makes the cursor flicker and is not what a screenshot should show.
        let copied = BitBlt(
            memory_dc,
            0,
            0,
            width as i32,
            height as i32,
            screen_dc,
            x,
            y,
            SRCCOPY,
        );

        if copied == 0 {
            SelectObject(memory_dc, previous);
            DeleteObject(bitmap as _);
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(anyhow!("BitBlt failed; the screen could not be read"));
        }

        // Pull the pixels out as BGRA, top-down.
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        // NEGATIVE height requests a top-down bitmap, so row 0 is the top of the screen. With a
        // positive height the image arrives bottom-up and every capture would be vertically
        // mirrored — which is easy to miss on a screenshot of a solid-colour test pattern.
        info.bmiHeader.biHeight = -(height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB as u32;

        let stride = width as usize * 4;
        let mut pixels = vec![0u8; stride * height as usize];

        let lines = GetDIBits(
            memory_dc,
            bitmap,
            0,
            height,
            pixels.as_mut_ptr() as *mut _,
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(memory_dc, previous);
        DeleteObject(bitmap as _);
        DeleteDC(memory_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        if lines == 0 {
            return Err(anyhow!("GetDIBits returned no scanlines"));
        }

        // BGRA to RGBA in place. Windows hands back blue first; PNG wants red first, and swapping
        // the wrong pair produces a colour-shifted image that still looks plausible — blue skies
        // become orange and it reads as a rendering fault rather than a byte-order one.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            // Force alpha opaque. GDI leaves it zero, and a fully transparent PNG renders as
            // nothing at all in most viewers.
            pixel[3] = 0xff;
        }

        let png = encode_png(&pixels, width, height).context("encoding the captured frame")?;

        if png.len() > MAX_PNG_BYTES {
            return Err(anyhow!(
                "the encoded frame is {} bytes, over the {MAX_PNG_BYTES}-byte limit. \
                 This happens with noisy full-screen content; capture a smaller region or reduce \
                 the guest's resolution",
                png.len()
            ));
        }

        Ok(Frame { png, width, height })
    }
}

/// Encode RGBA8 pixels as a PNG.
///
/// Written by hand rather than pulling in an image crate. The guest binary is deliberately kept
/// small so it is cheap to deploy into a debloated guest image, and PNG's minimum viable form is
/// small enough that a dependency is not worth its weight here: a signature, one IHDR, one IDAT of
/// zlib-compressed scanlines, one IEND.
///
/// Every scanline is prefixed with filter byte 0 (None). Adaptive filtering would compress better
/// but requires evaluating all five filters per row, and for screenshots — large flat areas, sharp
/// edges — the gain does not justify the code.
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let stride = width as usize * 4;
    if rgba.len() < stride * height as usize {
        return Err(anyhow!(
            "pixel buffer is {} bytes, need {} for {width}x{height}",
            rgba.len(),
            stride * height as usize
        ));
    }

    // Raw scanlines, each preceded by a filter byte.
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0u8); // filter: None
        raw.extend_from_slice(&rgba[row * stride..(row + 1) * stride]);
    }

    let compressed = zlib_store(&raw);

    let mut out = Vec::with_capacity(compressed.len() + 64);

    // PNG signature.
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    // IHDR: width, height, bit depth 8, colour type 6 (RGBA), no compression/filter/interlace.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // colour type: truecolour with alpha
    ihdr.push(0); // compression method: deflate
    ihdr.push(0); // filter method
    ihdr.push(0); // interlace: none
    write_chunk(&mut out, b"IHDR", &ihdr);

    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);

    Ok(out)
}

/// Write a PNG chunk: length, type, data, CRC.
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);

    // The CRC covers the type and the data, but NOT the length field. Getting that wrong produces
    // a file that some decoders accept and others reject, which is a miserable thing to debug.
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// CRC-32 as PNG specifies it (IEEE 802.3, reflected).
pub fn crc32(data: &[u8]) -> u32 {
    // Table built once per call. The cost is negligible next to a screen capture, and a lazily
    // initialised static would need synchronisation for no benefit at this call frequency.
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

/// zlib-wrapped DEFLATE using stored (uncompressed) blocks.
///
/// Stored blocks rather than real compression: implementing DEFLATE is a large amount of code, and
/// this keeps the guest dependency-free. The cost is size — a 1920x1080 frame becomes roughly
/// 8 MB rather than the few hundred KB a real encoder would produce — which is under the frame
/// limit but wasteful.
///
/// This is the one place in the capture path where an external dependency would earn its keep. It
/// is left as stored blocks deliberately, with the limitation stated, rather than pulling a
/// compression crate into a binary whose whole point is being small enough to deploy anywhere.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);

    // zlib header: deflate, 32 KiB window, no dictionary, default compression level.
    out.push(0x78);
    out.push(0x01);

    // Split into stored blocks. Each carries a 5-byte header: 1 byte of flags/bits, then LEN and
    // NLEN as little-endian u16. LEN limits a stored block to 65535 bytes.
    let mut offset = 0usize;
    if data.is_empty() {
        // An empty stream still needs one final empty block.
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        while offset < data.len() {
            let len = (data.len() - offset).min(65535);
            let last = offset + len >= data.len();

            out.push(if last { 0x01 } else { 0x00 });
            out.extend_from_slice(&(len as u16).to_le_bytes());
            // NLEN is the one's complement of LEN. A decoder checks this, so a wrong value is
            // rejected even though the data itself is fine.
            out.extend_from_slice(&(!(len as u16)).to_le_bytes());
            out.extend_from_slice(&data[offset..offset + len]);

            offset += len;
        }
    }

    // Adler-32 of the uncompressed data, big-endian.
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Adler-32, as zlib specifies.
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    // 65521 is the largest prime below 2^16, required so the sums fit in 32 bits with deferred
    // reduction. Reducing every 5552 bytes keeps `b` from overflowing before the modulo.
    const MOD: u32 = 65521;
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

    #[test]
    fn crc32_matches_known_vectors() {
        // The IEEE CRC-32 of "123456789" is 0xCBF43926. A wrong table or a missing final XOR
        // produces a plausible-looking number, so check against a published value.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"a"), 0xe8b7_be43);
    }

    #[test]
    fn adler32_matches_known_vectors() {
        // zlib's documented example: adler32("Wikipedia") == 0x11E60398.
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn a_png_has_a_valid_structure() {
        let pixels = vec![0x80u8; 4 * 2 * 2]; // 2x2 RGBA
        let png = encode_png(&pixels, 2, 2).expect("encode");

        // Signature.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        // First chunk must be IHDR, and its declared length must be 13.
        assert_eq!(&png[12..16], b"IHDR");
        let ihdr_len = u32::from_be_bytes([png[8], png[9], png[10], png[11]]);
        assert_eq!(ihdr_len, 13);

        // Width and height as declared.
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((w, h), (2, 2));

        // Last chunk must be IEND, and it must be the final 12 bytes.
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn every_chunk_crc_is_correct() {
        // Walk the chunks and recompute each CRC. A wrong CRC is the difference between a PNG that
        // opens and one that some viewers reject, and it is invisible without checking.
        let png = encode_png(&[0x11u8; 4 * 3 * 2], 3, 2).expect("encode");

        let mut pos = 8; // past the signature
        let mut chunks = 0;
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
                "CRC mismatch in chunk {:?}",
                std::str::from_utf8(kind)
            );

            chunks += 1;
            pos += 12 + len;
        }

        assert_eq!(pos, png.len(), "chunks must exactly cover the file");
        assert_eq!(chunks, 3, "a minimal PNG is IHDR, IDAT, IEND");
    }

    #[test]
    fn a_multi_block_image_round_trips_through_zlib() {
        // Larger than one 65535-byte stored block, so the block splitting path is exercised.
        // Getting the split wrong corrupts everything after the first block.
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let z = zlib_store(&data);

        // Header.
        assert_eq!(z[0], 0x78, "zlib CMF");
        assert_eq!(z[1], 0x01, "zlib FLG");

        // Trailer: Adler-32 of the original data.
        let n = z.len();
        let stored = u32::from_be_bytes([z[n - 4], z[n - 3], z[n - 2], z[n - 1]]);
        assert_eq!(
            stored,
            adler32(&data),
            "trailer must be the data's Adler-32"
        );

        // Pull the blocks back out and confirm every byte survives.
        let mut recovered = Vec::with_capacity(data.len());
        let mut pos = 2;
        loop {
            let header = z[pos];
            let last = header & 1 == 1;
            let len = u16::from_le_bytes([z[pos + 1], z[pos + 2]]) as usize;
            let nlen = u16::from_le_bytes([z[pos + 3], z[pos + 4]]);
            assert_eq!(
                nlen,
                !(len as u16),
                "NLEN must be the one's complement of LEN"
            );

            recovered.extend_from_slice(&z[pos + 5..pos + 5 + len]);
            pos += 5 + len;

            if last {
                break;
            }
        }

        assert_eq!(recovered.len(), data.len(), "byte count must match");
        assert_eq!(recovered, data, "bytes must match exactly");
    }

    #[test]
    fn an_empty_image_still_produces_a_valid_png() {
        // A zero-size capture is not useful, but producing structurally invalid output for it
        // would be worse: the failure should be in structure, not in a malformed file.
        let png = encode_png(&[], 0, 0).expect("encode");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn a_short_pixel_buffer_is_refused() {
        // Fewer bytes than width*height*4 must be an error, not a panic and not garbage output.
        let err = encode_png(&[0u8; 10], 4, 4).expect_err("must refuse a short buffer");
        assert!(
            err.to_string().contains("need"),
            "the error should say what is missing: {err}"
        );
    }

    #[test]
    fn capture_without_a_windows_guest_says_so() {
        if !cfg!(windows) {
            let err = capture(0).expect_err("must not succeed off Windows");
            assert!(
                err.to_string().contains("Windows build"),
                "the refusal must name the platform: {err}"
            );
        }
    }
}
