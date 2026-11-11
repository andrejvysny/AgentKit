//! macOS Seatbelt (`sandbox-exec`) profile for the shell tools.
//!
//! Adapted from OneMind (https://github.com/andrejvysny/OneMind), commit
//! 100dad7, path `src-ts/src/code/pi-worker/shell-isolation.ts` (lines 5-90:
//! `SAFE_PATH`, denied read roots, credential patterns, toolchain dirs).
//! Copyright (c) 2026 OneMind contributors. MIT License.
//! Changes: the workspace root is writable (OneMind's shell is read-only),
//! network is opt-in, `.git` is read-only (hooks and config only, with
//! allowGitWrites) so a command cannot plant a hook or driver that later runs
//! unsandboxed.
//!
//! Ported from `packages/toolpacks-coding/src/sandbox.ts`.

use std::collections::HashSet;

use crate::errors::CodingError;

/// The macOS Seatbelt launcher.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Longest path (in bytes) the profile embeds.
const MAX_PROFILE_PATH_BYTES: usize = 1024;

/// Whether `path` can be embedded in an SBPL string literal as-is. Paths
/// sit only inside `"..."` literals, so the characters that matter are the
/// literal's own delimiter and escape (`"`, `\`) and control characters;
/// spaces and non-ASCII names (common macOS folders such as
/// `~/Documents/Default Project`) are ordinary literal content.
fn profile_safe_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && path.len() <= MAX_PROFILE_PATH_BYTES
        && !path
            .chars()
            .any(|ch| ch == '"' || ch == '\\' || ch.is_control())
}

// Home, volumes and shared temp dirs hold credentials and other projects.
const DENIED_READ_ROOTS: &[&str] = &[
    "/Users",
    "/Volumes",
    "/private/tmp",
    "/private/var/folders",
    "/private/var/root",
];
const CREDENTIAL_PATTERNS: &[&str] = &[
    "/\\.(netrc|npmrc|pypirc|git-credentials)$",
    "/\\.cargo/credentials(\\.toml)?$",
    "/\\.(ssh|aws|gnupg|kube|docker|azure|terraform\\.d|password-store)(/|$)",
    "/\\.pgpass$",
    "/\\.config/(gh|gcloud|hub)(/|$)",
    "/\\.local/share/keyrings(/|$)",
    "/Library/(Keychains|Cookies)(/|$)",
];
/// `(allow default)` leaves the Mach/AppleEvent plumbing open, which lets a
/// sandboxed command ask an UNSANDBOXED system service to act for it:
/// LaunchServices (`open`), AppleEvents (`osascript` driving Terminal),
/// launchd job submission, and another process's task port. Each is denied
/// here; verified empirically against real `sandbox-exec` (see
/// `tests/sandbox_escape.rs`).
///
/// `mach-priv-task-port` is kept on purpose: it blocks `task_for_pid` /
/// privileged task-port access, so debuggers (lldb, dtrace-style tools) and
/// anything that injects into other processes cannot work inside the
/// sandbox. That is an intentional trade-off, not an oversight.
const ESCAPE_DENIES: &str = concat!(
    "(deny lsopen)",
    "(deny appleevent-send)",
    "(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\") ",
    "(global-name-regex #\"^com\\.apple\\.lsd\\.\") ",
    "(global-name \"com.apple.coreservices.quarantine-resolver\"))",
    "(deny mach-priv-task-port)",
    "(deny mach-lookup (global-name-regex #\"^com\\.apple\\.xpc\\.launchd\\.domain\\.\"))",
);
/// With the network off, the resolver daemons are also closed: DNS lookups
/// through `mDNSResponder` would otherwise leak data out of a "no network"
/// sandbox, and `system-socket` closes the raw PF_SYSTEM control sockets.
const NO_NETWORK_DENIES: &str = concat!(
    "(deny mach-lookup (global-name \"com.apple.mDNSResponder\") ",
    "(global-name \"com.apple.dnssd.service\"))",
    "(deny system-socket)",
);
const TOOLCHAIN_DIRS: &[&str] = &[
    ".rustup",
    ".cargo",
    ".bun",
    ".nvm",
    ".volta",
    ".deno",
    ".pyenv",
    ".rbenv",
    ".pixi",
    ".local/share/mise",
    "go/pkg/mod",
];

/// What one sandboxed command may touch.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    pub root: String,
    pub scratch: String,
    pub network: bool,
    /// False: the whole `.git` subtree is read-only; true: only hooks and config are.
    pub allow_git_writes: bool,
    /// Extra absolute canonical directories the command may read.
    pub readable: Vec<String>,
}

/// Where the sandbox would run.
#[derive(Clone, Debug)]
pub struct SandboxEnvironment {
    pub platform: String,
    pub sandbox_exec_path: String,
}

/// True only where the OS can really enforce the profile.
pub fn sandbox_available(env: &SandboxEnvironment) -> bool {
    env.platform == "darwin" && std::path::Path::new(&env.sandbox_exec_path).exists()
}

/// SBPL applies the last matching rule: broad deny, narrow allow, final deny.
pub fn sandbox_profile(policy: &SandboxPolicy) -> Result<String, CodingError> {
    let paths: Vec<&String> = [&policy.scratch, &policy.root]
        .into_iter()
        .chain(policy.readable.iter())
        .collect();
    if !paths.iter().all(|path| profile_safe_path(path)) {
        return Err(CodingError::new(
            "shell_isolation_unavailable",
            "a path needed by the sandbox profile contains a quote, backslash or control character; refusing to build the profile",
        ));
    }
    let subpath = |path: &str| format!("(subpath \"{path}\")");
    // Config, hooks, attributes, refs and objects all steer later unsandboxed
    // git runs, so the default is to fence the whole subtree.
    let git_guards: Vec<String> = if policy.allow_git_writes {
        vec![
            join_path(&policy.root, ".git/hooks"),
            join_path(&policy.root, ".git/config"),
        ]
    } else {
        vec![join_path(&policy.root, ".git")]
    };
    Ok(format!(
        "{}{}{}{}{}{}{}{}{}",
        "(version 1)(allow default)",
        if policy.network {
            ""
        } else {
            "(deny network*)"
        },
        format_args!(
            "{ESCAPE_DENIES}{}",
            if policy.network {
                ""
            } else {
                NO_NETWORK_DENIES
            }
        ),
        "(deny file-write*)",
        format_args!(
            "(allow file-write* {} {} (literal \"/dev/null\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\"))",
            subpath(&policy.root),
            subpath(&policy.scratch)
        ),
        format_args!(
            "(deny file-write* {})",
            git_guards
                .iter()
                .map(|p| subpath(p))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        format_args!(
            "(deny file-read-data {})",
            DENIED_READ_ROOTS
                .iter()
                .map(|p| subpath(p))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        format_args!(
            "(allow file-read-data {})",
            paths
                .iter()
                .map(|p| subpath(p))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        format_args!(
            "(deny file-read-data {})",
            CREDENTIAL_PATTERNS
                .iter()
                .map(|pattern| format!("(regex #\"{pattern}\")"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    ))
}

/// `path.join` for absolute posix roots with simple relative segments.
/// A trailing separator on the root collapses like `path.join` does, so a
/// caller that forgot to canonicalize still gets a well-formed `(subpath …)`
/// fence rather than a silently doubled-slash (i.e. non-matching) guard.
fn join_path(root: &str, tail: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        format!("/{tail}")
    } else {
        format!("{root}/{}", tail.trim_start_matches('/'))
    }
}

fn real_directory(path: &str) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Toolchains under $HOME plus the running executable's directory. `env` is
/// the environment to inspect (HOME, PATH); the current process's executable
/// stands in for Node's `process.execPath`.
pub fn toolchain_paths(env: &std::collections::HashMap<String, String>) -> Vec<String> {
    let home = env
        .get("HOME")
        .filter(|h| !h.is_empty())
        .cloned()
        .or_else(home_dir)
        .unwrap_or_default();
    let mut found: Vec<String> = Vec::new();
    for entry in env.get("PATH").map(String::as_str).unwrap_or("").split(':') {
        if entry.is_empty()
            || !entry.starts_with('/')
            || !(entry == home || entry.starts_with(&format!("{home}/")))
        {
            continue;
        }
        // Only the bin dir itself: a tool's home may hold its auth tokens.
        if let Some(real) = real_directory(entry) {
            found.push(real);
        }
    }
    for dir in TOOLCHAIN_DIRS {
        if let Some(real) = real_directory(&join_path(&home, dir)) {
            found.push(real);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Some(real) = real_directory(&dir.to_string_lossy())
    {
        found.push(real);
    }
    // A readable grant of $HOME (or any ancestor) would re-expose every
    // credential directory beneath it, so such an entry is never granted.
    let real_home = real_directory(&home);
    found.retain(|path| !is_home_or_ancestor(path, &home, real_home.as_deref()));
    let mut found: Vec<String> = found
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    found.retain(|path| profile_safe_path(path));
    found.sort();
    found
}

/// True for "/", "/Users", "/home", and any path equal to or containing the
/// (raw or canonical) home directory.
fn is_home_or_ancestor(path: &str, home: &str, real_home: Option<&str>) -> bool {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/Users" || path == "/home" {
        return true;
    }
    let covers = |home: &str| {
        let home = home.trim_end_matches('/');
        !home.is_empty() && (home == path || home.starts_with(&format!("{path}/")))
    };
    covers(home) || real_home.is_some_and(covers)
}

/// `os.homedir()` fallback when HOME is unset: getpwuid(getuid()).
fn home_dir() -> Option<String> {
    #[cfg(unix)]
    {
        // Caller-owned buffer; `getpwuid_r` answers ERANGE when it is too
        // small, so grow and retry up to a sane ceiling.
        let mut size = 1024usize;
        while size <= 1 << 20 {
            let mut buffer = vec![0 as libc::c_char; size];
            // SAFETY: an all-zero `passwd` is a valid initial value (null
            // pointers, zero ids); it is only read after `getpwuid_r`
            // reports success.
            let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `passwd` and `result` are valid for writes, `buffer`
            // is a live allocation of exactly `size` bytes owned by this
            // frame, and `getuid` is always safe.
            let rc = unsafe {
                libc::getpwuid_r(
                    libc::getuid(),
                    &mut passwd,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                )
            };
            if rc == libc::ERANGE {
                size *= 2;
                continue;
            }
            if rc != 0 || result.is_null() || passwd.pw_dir.is_null() {
                return None;
            }
            // SAFETY: on success `pw_dir` points at a NUL-terminated string
            // inside `buffer`, which is still alive and unmodified here; it
            // is copied out before `buffer` drops.
            return Some(
                unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy {
            root: "/w/root".to_owned(),
            scratch: "/w/scratch".to_owned(),
            network: false,
            allow_git_writes: false,
            readable: vec![],
        }
    }

    #[test]
    fn profile_blocks_network_and_fences_git_by_default() {
        let text = sandbox_profile(&policy()).unwrap();
        assert_eq!(
            text,
            r##"(version 1)(allow default)(deny network*)__DENIES__(deny file-write*)(allow file-write* (subpath "/w/root") (subpath "/w/scratch") (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/fd/[0-9]+$"))(deny file-write* (subpath "/w/root/.git"))(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/tmp") (subpath "/private/var/folders") (subpath "/private/var/root"))(allow file-read-data (subpath "/w/scratch") (subpath "/w/root"))(deny file-read-data (regex #"/\.(netrc|npmrc|pypirc|git-credentials)$") (regex #"/\.cargo/credentials(\.toml)?$") (regex #"/\.(ssh|aws|gnupg|kube|docker|azure|terraform\.d|password-store)(/|$)") (regex #"/\.pgpass$") (regex #"/\.config/(gh|gcloud|hub)(/|$)") (regex #"/\.local/share/keyrings(/|$)") (regex #"/Library/(Keychains|Cookies)(/|$)"))"##
            .replace("__DENIES__", &format!("{ESCAPE_DENIES}{NO_NETWORK_DENIES}"))
        );
    }

    #[test]
    fn trailing_separator_on_the_root_still_fences_git() {
        // A non-canonical root must not produce `(subpath "/w/root//.git")`
        // — the doubled slash would match nothing, silently dropping the
        // .git write fence while everything else still looks enforced.
        let mut p = policy();
        p.root = "/w/root/".to_owned();
        let text = sandbox_profile(&p).unwrap();
        assert!(
            text.contains("(deny file-write* (subpath \"/w/root/.git\"))"),
            "{text}"
        );
        assert!(!text.contains("//.git"), "{text}");
    }

    #[test]
    fn profile_allows_network_when_opted_in() {
        let mut p = policy();
        p.network = true;
        let text = sandbox_profile(&p).unwrap();
        assert!(text.starts_with(&format!(
            "(version 1)(allow default){ESCAPE_DENIES}(deny file-write*)"
        )));
        assert!(!text.contains("(deny network*)"));
    }

    #[test]
    fn git_writes_narrow_the_denial_to_hooks_and_config() {
        let mut p = policy();
        p.allow_git_writes = true;
        let text = sandbox_profile(&p).unwrap();
        assert!(text.contains(
            "(deny file-write* (subpath \"/w/root/.git/hooks\") (subpath \"/w/root/.git/config\"))"
        ));
    }

    #[test]
    fn extra_readable_dirs_are_read_allowed() {
        let mut p = policy();
        p.readable = vec!["/Users/me/.cargo/bin".to_owned()];
        let text = sandbox_profile(&p).unwrap();
        assert!(text
            .contains("(allow file-read-data (subpath \"/w/scratch\") (subpath \"/w/root\") (subpath \"/Users/me/.cargo/bin\"))"));
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in [
            "/w/\"root\"",
            "/w/ro\\ot",
            "/w/ro\not",
            "relative/root",
            "/",
        ] {
            let mut p = policy();
            p.root = bad.to_owned();
            let error = sandbox_profile(&p).unwrap_err();
            assert_eq!(error.code, "shell_isolation_unavailable");
            assert!(error.message.contains("quote, backslash or control"));
        }
    }

    #[test]
    fn spaces_and_unicode_paths_build_a_profile() {
        for good in [
            "/Users/me/Documents/Default Project/web",
            "/w/ž dir",
            "/w/r;oot",
        ] {
            let mut p = policy();
            p.root = good.to_owned();
            let profile = sandbox_profile(&p).expect("profile");
            assert!(profile.contains(&format!("(subpath \"{good}\")")));
        }
    }

    #[test]
    fn availability_requires_darwin_and_the_binary() {
        assert!(!sandbox_available(&SandboxEnvironment {
            platform: "linux".to_owned(),
            sandbox_exec_path: "/bin/sh".to_owned(),
        }));
        assert!(!sandbox_available(&SandboxEnvironment {
            platform: "darwin".to_owned(),
            sandbox_exec_path: "/nonexistent/sandbox-exec".to_owned(),
        }));
    }

    #[test]
    fn toolchain_paths_keeps_home_path_dirs_and_toolchains_sorted() {
        let home = std::env::temp_dir().join(format!("toolchain-test-{}", std::process::id()));
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join("outside")).unwrap();
        std::fs::create_dir_all("/tmp/not-home-bin").unwrap();
        let env = std::collections::HashMap::from([
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            (
                "PATH".to_owned(),
                format!(
                    "{}/bin:{}/outside:/usr/bin:{}/nope",
                    home.to_string_lossy(),
                    home.to_string_lossy(),
                    home.to_string_lossy()
                ),
            ),
        ]);
        let paths = toolchain_paths(&env);
        // toolchain_paths resolves aliases such as macOS's /var before
        // returning directories, so expected paths use the same identity.
        let home_s = std::fs::canonicalize(&home)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(paths.contains(&format!("{home_s}/bin")));
        assert!(paths.contains(&format!("{home_s}/.cargo")));
        // Any real dir under $HOME counts (the code checks existence, not a
        // bin name); dirs outside $HOME and missing ones never appear.
        assert!(paths.contains(&format!("{home_s}/outside")));
        assert!(
            !paths
                .iter()
                .any(|p| p == "/usr/bin" || p == "/tmp/not-home-bin")
        );
        assert!(!paths.iter().any(|p| p.ends_with("/nope")));
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn profile_denies_the_launch_services_apple_event_and_launchd_escapes() {
        for network in [false, true] {
            let mut p = policy();
            p.network = network;
            let text = sandbox_profile(&p).unwrap();
            for rule in [
                "(deny lsopen)",
                "(deny appleevent-send)",
                "(deny mach-priv-task-port)",
                "com.apple.coreservices.launchservicesd",
                r"com\.apple\.xpc\.launchd\.domain",
            ] {
                assert!(text.contains(rule), "{rule}: {text}");
            }
            // The resolver/PF_SYSTEM denies belong to the no-network profile only.
            assert_eq!(text.contains("com.apple.mDNSResponder"), !network);
            assert_eq!(text.contains("(deny system-socket)"), !network);
        }
    }

    #[test]
    fn home_and_its_ancestors_are_never_readable_grants() {
        assert!(is_home_or_ancestor("/", "/Users/me", None));
        assert!(is_home_or_ancestor("/Users", "/Users/me", None));
        assert!(is_home_or_ancestor("/home", "/home/me", None));
        assert!(is_home_or_ancestor("/Users/me", "/Users/me", None));
        assert!(is_home_or_ancestor("/Users/me/", "/Users/me", None));
        assert!(is_home_or_ancestor(
            "/private/Users",
            "/x",
            Some("/private/Users/me")
        ));
        assert!(!is_home_or_ancestor("/Users/me/bin", "/Users/me", None));
        assert!(!is_home_or_ancestor("/Users/meow", "/Users/me", None));
        assert!(!is_home_or_ancestor("/usr/bin", "/Users/me", None));
    }

    #[test]
    fn a_path_entry_equal_to_home_is_dropped_by_toolchain_paths() {
        let home = std::env::temp_dir().join(format!("toolchain-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let env = std::collections::HashMap::from([
            ("HOME".to_owned(), home_s.clone()),
            ("PATH".to_owned(), format!("{home_s}:{home_s}/bin")),
        ]);
        let real = std::fs::canonicalize(&home).unwrap();
        let paths = toolchain_paths(&env);
        assert!(
            !paths.iter().any(|p| p == real.to_str().unwrap()),
            "{paths:?}"
        );
        assert!(
            paths.contains(&format!("{}/bin", real.display())),
            "{paths:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn home_dir_fallback_resolves_through_getpwuid_r() {
        let home = home_dir().expect("passwd entry for the current uid");
        assert!(home.starts_with('/'), "{home}");
    }
}
