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
use std::sync::LazyLock;

use regex::Regex;

use crate::errors::CodingError;

/// The macOS Seatbelt launcher.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

static SAFE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[A-Za-z0-9._/-]{1,512}$").expect("safe path pattern"));

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
    if !paths.iter().all(|path| SAFE_PATH.is_match(path)) {
        return Err(CodingError::new(
            "shell_isolation_unavailable",
            "a path needed by the sandbox profile contains characters outside [A-Za-z0-9._/-]; refusing to build the profile",
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
        "{}{}{}{}{}{}{}{}",
        "(version 1)(allow default)",
        if policy.network {
            ""
        } else {
            "(deny network*)"
        },
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
    let mut found: Vec<String> = found
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    found.retain(|path| SAFE_PATH.is_match(path));
    found.sort();
    found
}

/// `os.homedir()` fallback when HOME is unset: getpwuid(getuid()).
fn home_dir() -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe; getpwuid returns a static per-thread
        // buffer we copy synchronously.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() || (*pw).pw_dir.is_null() {
                return None;
            }
            Some(
                std::ffi::CStr::from_ptr((*pw).pw_dir)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
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
            r##"(version 1)(allow default)(deny network*)(deny file-write*)(allow file-write* (subpath "/w/root") (subpath "/w/scratch") (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/fd/[0-9]+$"))(deny file-write* (subpath "/w/root/.git"))(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/tmp") (subpath "/private/var/folders") (subpath "/private/var/root"))(allow file-read-data (subpath "/w/scratch") (subpath "/w/root"))(deny file-read-data (regex #"/\.(netrc|npmrc|pypirc|git-credentials)$") (regex #"/\.cargo/credentials(\.toml)?$") (regex #"/\.(ssh|aws|gnupg|kube|docker|azure|terraform\.d|password-store)(/|$)") (regex #"/\.pgpass$") (regex #"/\.config/(gh|gcloud|hub)(/|$)") (regex #"/\.local/share/keyrings(/|$)") (regex #"/Library/(Keychains|Cookies)(/|$)"))"##
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
        assert!(text.starts_with("(version 1)(allow default)(deny file-write*)"));
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
        for bad in ["/w/my root", "/w/\"root\"", "/w/r;oot"] {
            let mut p = policy();
            p.root = bad.to_owned();
            let error = sandbox_profile(&p).unwrap_err();
            assert_eq!(error.code, "shell_isolation_unavailable");
            assert!(error.message.contains("characters outside"));
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
}
