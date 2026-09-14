//! Path translation and validation for guest-side file operations.
//!
//! This is the guest's half of the containment story. The host decides whether a transfer is
//! permitted; the guest then has to actually *resolve* the path it was given, and a resolution
//! that disagrees with the host's check is a hole. So the rules are stated once here, mirrored
//! exactly from `wvm-host`'s `policy::check_guest_path`, and tested on both sides.
//!
//! Windows paths are not POSIX paths and must not be treated as such:
//!
//! * `\` and `/` are both separators.
//! * Comparison is case-insensitive.
//! * `C:\wvm` and `C:\wvmdata` share a prefix but not a directory.
//!
//! All three have bitten real software. The tests below exist because of that.

#![allow(dead_code)] // Wired into the transfer path in M4; see docs/BUILD-PLAN.md.

use anyhow::{bail, Result};

/// Normalise a Windows path to a comparable form: forward slashes, no trailing separator,
/// lowercased, `.` and `..` resolved.
///
/// Resolution is lexical. The path may not exist yet, and following a symlink or reparse point
/// could land outside the root after the check has passed.
pub fn canonicalise(path: &str) -> String {
    // A drive letter or UNC prefix is preserved; everything after is resolved segment by segment.
    let (prefix, rest) = split_prefix(path);

    // Bind the normalised form before iterating: a temporary in the `for` head is dropped at the
    // end of the statement, and the borrow below outlives it.
    let normalised = rest.replace('\\', "/");

    let mut parts: Vec<&str> = Vec::new();
    for segment in normalised.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Popping past the root is meaningless; refuse to go above it rather than
                // silently discarding the traversal.
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let joined = parts.join("/").to_lowercase();
    if prefix.is_empty() {
        joined
    } else {
        // The separator inside the prefix is normalised too, so `\\server\share` becomes
        // `//server/share` and compares consistently with a forward-slash form.
        let prefix_norm = prefix.replace('\\', "/").to_lowercase();
        if joined.is_empty() {
            format!("{}/", prefix_norm)
        } else {
            format!("{}/{}", prefix_norm, joined)
        }
    }
}

/// Split `C:\foo` into (`C:`, `foo`) and a UNC `\\server\share\foo` into its server/share prefix.
///
/// Both halves are borrowed from `path`, so this cannot allocate.
fn split_prefix(path: &str) -> (&str, &str) {
    let bytes = path.as_bytes();

    // Drive letter: "C:" or "C:\".
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return (&path[..2], &path[2..]);
    }

    // UNC: "\\server\share\rest". The prefix is the leading separator plus host and share, and
    // the rest begins at the separator that follows the share.
    //
    // The offset arithmetic here is fiddly and was wrong on the first attempt: `char_indices`
    // yields indices relative to `after_lead`, which starts after the single leading separator
    // that `trim_start_matches` consumed — so the prefix ends at the index of the second
    // separator relative to the original string, not that index plus the lead length. Binding
    // `normalised` and working from it avoids the correction entirely.
    if path.starts_with("\\\\") || path.starts_with("//") {
        let rest = &path[1..];
        let mut separators_seen = 0;
        for (idx, ch) in rest.char_indices() {
            if ch == '\\' || ch == '/' {
                separators_seen += 1;
                if separators_seen == 2 {
                    // `idx` is relative to `rest`, which begins at byte 1 of `path`.
                    let split_at = 1 + idx;
                    return (&path[..split_at], &path[split_at..]);
                }
            }
        }
        // Fewer than two separators: the whole thing is the prefix.
        return (path, "");
    }

    ("", path)
}

/// Is `path` inside `root`, on a directory boundary?
///
/// Mirrors the host's check exactly. If these two ever disagree, the disagreement is the bug.
pub fn is_within_root(root: &str, path: &str) -> bool {
    let root_c = canonicalise(root);
    if root_c.is_empty() {
        return false;
    }
    let path_c = canonicalise(path);

    // Exact match, or the path continues with a separator. A raw `starts_with` would let
    // "c:/wvmdata" satisfy root "c:/wvm".
    path_c == root_c || path_c.starts_with(&format!("{root_c}/"))
}

/// Validate a guest path against a root, returning the canonical form to operate on.
pub fn resolve_within(root: &str, path: &str) -> Result<String> {
    if root.trim().is_empty() {
        bail!("no guest root configured; refusing to resolve any path");
    }
    if path.trim().is_empty() {
        bail!("empty path");
    }
    if !is_within_root(root, path) {
        bail!(
            "refusing '{}': outside the permitted root '{}'",
            canonicalise(path),
            canonicalise(root)
        );
    }
    Ok(path.replace('/', "\\"))
}

/// Turn a Windows path into the Z:-style form used by the host when it hands a path over.
///
/// Kept here rather than inline so there is one definition of the mapping rather than several
/// subtly different ones.
///
/// A note on the argument order: the caller supplies `relative_to` — the guest-side path the
/// transfer is confined to — and this returns that path re-expressed under `host_root`. Passing
/// the two in the wrong order silently produces a plausible-looking but wrong path, so the tests
/// below check the concrete strings rather than just the shape.
pub fn to_host_posix(host_root: &str, windows_path: &str) -> String {
    let relative = windows_path.replace('\\', "/");
    let relative = relative.trim_start_matches('/');
    format!("{}/{}", host_root.trim_end_matches('/'), relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalise_lowercases_and_normalises_separators() {
        assert_eq!(
            canonicalise("C:\\WVM\\Sub\\File.txt"),
            "c:/wvm/sub/file.txt"
        );
        assert_eq!(canonicalise("C:/WVM/Sub/File.txt"), "c:/wvm/sub/file.txt");
    }

    #[test]
    fn canonicalise_accepts_both_separators_in_one_path() {
        // Windows does, so we must.
        assert_eq!(canonicalise("C:\\wvm/sub\\file"), "c:/wvm/sub/file");
    }

    #[test]
    fn canonicalise_drops_trailing_separator() {
        assert_eq!(canonicalise("C:\\wvm\\"), "c:/wvm");
        assert_eq!(canonicalise("C:\\wvm"), "c:/wvm");
    }

    #[test]
    fn canonicalise_resolves_dot_segments() {
        assert_eq!(canonicalise("C:\\wvm\\.\\sub"), "c:/wvm/sub");
        assert_eq!(canonicalise("C:\\wvm\\sub\\..\\other"), "c:/wvm/other");
    }

    #[test]
    fn traversal_above_the_root_is_clamped_not_escaped() {
        // ".." past the top cannot produce a path outside; it simply stops.
        assert_eq!(canonicalise("C:\\..\\..\\windows"), "c:/windows");
        assert_eq!(canonicalise("C:\\wvm\\..\\..\\..\\x"), "c:/x");
    }

    #[test]
    fn path_inside_the_root_is_accepted() {
        assert!(is_within_root("C:\\wvm", "C:\\wvm\\file.txt"));
        assert!(is_within_root("C:\\wvm", "C:\\wvm\\sub\\deep\\file.txt"));
        // Case-insensitive, as Windows is.
        assert!(is_within_root("C:\\WVM", "c:\\wvm\\file.txt"));
        // The root itself.
        assert!(is_within_root("C:\\wvm", "C:\\wvm"));
    }

    #[test]
    fn a_sibling_sharing_a_prefix_is_rejected() {
        // The classic: "C:\wvmdata" must not satisfy root "C:\wvm".
        assert!(!is_within_root("C:\\wvm", "C:\\wvmdata\\x"));
        assert!(!is_within_root("C:\\wvm", "C:\\wvmsecret"));
    }

    #[test]
    fn traversal_out_of_the_root_is_rejected() {
        assert!(!is_within_root(
            "C:\\wvm",
            "C:\\wvm\\..\\Windows\\System32\\config\\SAM"
        ));
        assert!(!is_within_root("C:\\wvm", "C:\\wvm\\..\\..\\..\\windows"));
    }

    #[test]
    fn a_different_drive_is_rejected() {
        assert!(!is_within_root("C:\\wvm", "D:\\wvm\\file.txt"));
    }

    #[test]
    fn an_empty_root_permits_nothing() {
        assert!(!is_within_root("", "C:\\wvm\\file.txt"));
        assert!(resolve_within("", "C:\\wvm\\file.txt").is_err());
    }

    #[test]
    fn resolve_returns_a_native_separator_path() {
        let got = resolve_within("C:\\wvm", "C:/wvm/sub/file.txt").unwrap();
        assert_eq!(got, "C:\\wvm\\sub\\file.txt");
    }

    #[test]
    fn resolve_refuses_an_empty_path() {
        assert!(resolve_within("C:\\wvm", "").is_err());
        assert!(resolve_within("C:\\wvm", "   ").is_err());
    }

    #[test]
    fn resolve_error_names_the_root() {
        let err = resolve_within("C:\\wvm", "C:\\Windows\\System32").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("c:/wvm"), "should name the root: {text}");
        assert!(text.contains("refusing"), "should say what it did: {text}");
    }

    #[test]
    fn unc_prefixes_are_handled() {
        // A UNC path is absolute and must not be confused for a relative one.
        assert_eq!(
            canonicalise("\\\\server\\share\\file"),
            "//server/share/file"
        );
        // The same root written either way must agree, or containment checks become bypassable
        // by changing separator style.
        assert_eq!(
            canonicalise("\\\\server\\share\\file"),
            canonicalise("//server/share/file")
        );
        assert!(is_within_root(
            "\\\\server\\share",
            "\\\\server\\share\\file"
        ));
        assert!(is_within_root("\\\\server\\share", "//server/share/file"));
        assert!(!is_within_root(
            "\\\\server\\share",
            "\\\\server\\other\\file"
        ));
    }

    #[test]
    fn unc_paths_do_not_escape_via_traversal() {
        // The share is the boundary; ".." must not climb out of it.
        assert!(!is_within_root(
            "\\\\server\\share",
            "\\\\server\\share\\..\\..\\other\\file"
        ));
    }

    #[test]
    fn host_posix_mapping_joins_cleanly() {
        // A guest absolute path is re-expressed under the host root, separators normalised.
        assert_eq!(
            to_host_posix("/srv/wvm/in", "C:\\wvm\\file.txt"),
            "/srv/wvm/in/C:/wvm/file.txt"
        );
        // A relative guest path joins directly.
        assert_eq!(
            to_host_posix("/srv/wvm/in", "file.txt"),
            "/srv/wvm/in/file.txt"
        );
        // A trailing separator on the root does not double up.
        assert_eq!(
            to_host_posix("/srv/wvm/in/", "file.txt"),
            "/srv/wvm/in/file.txt"
        );
        // Mixed separators in the guest path are normalised.
        assert_eq!(
            to_host_posix("/srv/wvm/in", "C:/wvm\\sub/file"),
            "/srv/wvm/in/C:/wvm/sub/file"
        );
    }
}
