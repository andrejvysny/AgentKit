//! `file://` URI conversion with correct percent-encoding (spaces, `#`,
//! `%`, non-ASCII). Round-trip tested.

use percent_encoding_free::Set;
use std::path::{Path, PathBuf};

/// Bytes that may appear literally in a path component of a file URI.
/// Everything else (space, `#`, `?`, `%`, non-ASCII...) is percent-encoded
/// from its UTF-8 bytes. `/` is preserved; it only ever appears as a
/// separator because we encode per whole-path with `/` allowed.
const UNRESERVED: &Set = &Set::new(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~/",
    true,
);

mod percent_encoding_free {
    /// Minimal ASCII set membership helper (no external crate).
    pub struct Set {
        table: [bool; 128],
    }
    impl Set {
        pub const fn new(allowed: &str, deny_by_default: bool) -> Self {
            let mut table = [!deny_by_default; 128];
            let bytes = allowed.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                table[bytes[i] as usize] = true;
                i += 1;
            }
            Self { table }
        }
        pub fn contains(&self, byte: u8) -> bool {
            if byte < 128 {
                self.table[byte as usize]
            } else {
                false
            }
        }
    }
}

/// Absolute path → `file:///encoded/path`.
pub fn path_to_uri(path: &Path) -> String {
    let encoded = encode_path(&path.to_string_lossy());
    format!("file://{encoded}")
}

fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for byte in path.as_bytes() {
        if UNRESERVED.contains(*byte) {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// `file://` URI → local path. Percent-decodes UTF-8; rejects non-`file`
/// schemes and malformed escapes.
pub fn uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let rest = uri
        .strip_prefix("file://")
        .ok_or_else(|| format!("not a file:// URI: {uri}"))?;
    // The authority runs to the first `/`; only an empty authority or
    // `localhost` names the local filesystem. Anything else is a foreign
    // host and must be refused — a naive prefix strip would read
    // `file://localhostevil/x` as the local path `evil/x`.
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if !(authority.is_empty() || authority == "localhost") {
        return Err(format!(
            "file URI has a foreign authority {authority:?}: {uri}"
        ));
    }
    decode_percent(path)
        .map(PathBuf::from)
        .ok_or_else(|| format!("file URI does not contain valid UTF-8 percent escapes: {uri}"))
}

fn decode_percent(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn round_trip(path: &str) {
        let p = PathBuf::from(path);
        let uri = path_to_uri(&p);
        assert!(uri.starts_with("file:///"), "{uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), p, "round trip {path}");
    }

    #[test]
    fn round_trips_plain_and_awkward_paths() {
        round_trip("/home/user/proj/src/lib.rs");
        round_trip("/tmp/my project/file name.rs");
        round_trip("/tmp/we#ird/q?mark.pc%t.ts");
        round_trip("/tmp/ünïcödé/文件.ts");
        round_trip("/tmp/emoji-🚀/main.tsx");
        round_trip("/tmp/pluss+and,comma;semi/`tick`.ts");
    }

    #[test]
    fn encodes_space_as_percent20() {
        assert_eq!(
            path_to_uri(Path::new("/tmp/a b/c d.rs")),
            "file:///tmp/a%20b/c%20d.rs"
        );
    }

    #[test]
    fn encodes_reserved_delimiters() {
        let uri = path_to_uri(Path::new("/a#b?c%d"));
        assert_eq!(uri, "file:///a%23b%3Fc%25d");
    }

    #[test]
    fn accepts_localhost_authority() {
        assert_eq!(
            uri_to_path("file://localhost/tmp/x.rs").unwrap(),
            PathBuf::from("/tmp/x.rs")
        );
    }

    #[test]
    fn localhost_prefixes_are_foreign_authorities() {
        // A naive prefix strip read `file://localhostevil/x` as the local
        // path `evil/x`; the authority must terminate at the first `/` and
        // be exactly empty or `localhost` — anything else is foreign.
        assert!(uri_to_path("file://localhostevil/x").is_err());
        assert!(uri_to_path("file://localhost:8080/tmp/x.rs").is_err());
        assert!(uri_to_path("file://localhost.evil.com/x").is_err());
    }

    #[test]
    fn rejects_foreign_schemes_and_bad_escapes() {
        assert!(uri_to_path("http://x/y").is_err());
        assert!(uri_to_path("file:///tmp/%zz").is_err());
        assert!(uri_to_path("file:///tmp/%e0%a0").is_err()); // invalid UTF-8
    }

    #[test]
    fn unicode_encodes_via_utf8_bytes() {
        let uri = path_to_uri(Path::new("/tmp/日本.rs"));
        assert_eq!(uri, "file:///tmp/%E6%97%A5%E6%9C%AC.rs");
    }
}
