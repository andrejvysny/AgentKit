//! Kernel command-line boot contract (§8.1, T18 shape).
//!
//! The guest refuses to serve unless `/proc/cmdline` carries a well-formed
//! `agentkit.boot_nonce=<hex>`. The nonce is echoed on handshake purely as
//! channel association (never attestation, §3.3). Optional knobs:
//! `agentkit.allow_guest_root=1` (default deny) and `agentkit.workspace=<path>`.

/// Default private workspace mount (overridable via `agentkit.workspace=`).
pub const DEFAULT_WORKSPACE: &str = "/workspace";

/// Minimum accepted nonce length in hex characters (8 bytes of entropy).
pub const MIN_NONCE_HEX: usize = 16;
/// Maximum accepted nonce length in hex characters (64 bytes).
pub const MAX_NONCE_HEX: usize = 128;

/// Parsed, validated boot parameters. Absent nonce is represented, not
/// defaulted: `main` decides to refuse (exit 3) before binding any listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootParams {
    /// Decoded nonce bytes; `None` when the kernel cmdline omitted or
    /// malformed it — the caller MUST refuse to serve.
    pub boot_nonce: Option<Vec<u8>>,
    /// `agentkit.allow_guest_root=1` only; the wire never selects root.
    pub allow_guest_root: bool,
    /// `agentkit.workspace=<path>`; defaults to `/workspace`.
    pub workspace: String,
}

impl BootParams {
    /// Well-formed nonce present?
    #[must_use]
    pub fn has_valid_nonce(&self) -> bool {
        self.boot_nonce.as_ref().is_some_and(|b| !b.is_empty())
    }
}

/// Parse a raw kernel command line string.
#[must_use]
pub fn parse(raw: &str) -> BootParams {
    let mut params = BootParams {
        boot_nonce: None,
        allow_guest_root: false,
        workspace: DEFAULT_WORKSPACE.to_string(),
    };
    for token in raw.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "agentkit.boot_nonce" => {
                params.boot_nonce = parse_nonce(value);
            }
            "agentkit.allow_guest_root" => {
                params.allow_guest_root = value == "1";
            }
            // Only accept an absolute, printable path; anything else keeps
            // the default (fail closed to a known-good value).
            "agentkit.workspace" if is_sane_workspace(value) => {
                params.workspace = value.to_string();
            }
            _ => {}
        }
    }
    params
}

/// Read and parse the real kernel command line (Linux only). Any I/O error
/// yields "no nonce", which the caller treats as refuse-to-serve.
#[must_use]
pub fn from_proc_cmdline() -> BootParams {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string("/proc/cmdline") {
            Ok(raw) => parse(&raw),
            Err(_) => BootParams {
                boot_nonce: None,
                allow_guest_root: false,
                workspace: DEFAULT_WORKSPACE.to_string(),
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        BootParams {
            boot_nonce: None,
            allow_guest_root: false,
            workspace: DEFAULT_WORKSPACE.to_string(),
        }
    }
}

fn parse_nonce(value: &str) -> Option<Vec<u8>> {
    if value.len() < MIN_NONCE_HEX
        || value.len() > MAX_NONCE_HEX
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    if out.is_empty() { None } else { Some(out) }
}

fn is_sane_workspace(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value.starts_with('/')
        && !value.contains('\0')
        && !value.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
        && !value.ends_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_known_knobs() {
        let p = parse(
            "console=ttyS0 agentkit.boot_nonce=00ff112233445566778899aabbccddee agentkit.allow_guest_root=1 agentkit.workspace=/mnt/ws ro",
        );
        assert_eq!(
            p.boot_nonce.as_deref(),
            Some(
                [
                    0x00u8, 0xff, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                    0xcc, 0xdd, 0xee
                ]
                .as_slice()
            )
        );
        assert!(p.allow_guest_root);
        assert_eq!(p.workspace, "/mnt/ws");
    }

    #[test]
    fn defaults_without_knobs() {
        let p = parse("quiet splash");
        assert!(p.boot_nonce.is_none());
        assert!(!p.allow_guest_root);
        assert_eq!(p.workspace, DEFAULT_WORKSPACE);
    }

    #[test]
    fn malformed_nonces_are_none() {
        for bad in [
            "",
            "00ff",                  // too short
            "zz".repeat(8).as_str(), // non-hex
            "00ff10"
                .repeat(30) // too long (180 hex chars)
                .as_str(),
            "00ff102", // odd length
        ] {
            let p = parse(&format!("agentkit.boot_nonce={bad}"));
            assert!(p.boot_nonce.is_none(), "must reject {bad:?}");
        }
    }

    #[test]
    fn bogus_workspace_keeps_default() {
        let p = parse("agentkit.workspace=relative/x agentkit.boot_nonce=0011223344556677");
        assert_eq!(p.workspace, DEFAULT_WORKSPACE);
        let p = parse("agentkit.workspace=/w/ agentkit.boot_nonce=0011223344556677");
        assert_eq!(p.workspace, DEFAULT_WORKSPACE);
    }

    #[test]
    fn last_nonce_wins_but_malformed_clobbers() {
        let p = parse("agentkit.boot_nonce=0011223344556677 agentkit.boot_nonce=zz");
        assert!(p.boot_nonce.is_none());
    }
}
