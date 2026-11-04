//! Language detection and server-binary discovery. No installation is
//! ever performed: when no candidate binary exists, discovery fails with
//! `lsp_server_not_found` naming the language and the binaries searched.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::error::{SERVER_NOT_FOUND, err};
use crate::pool::ServerCommand;

/// A language the registry knows how to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageSpec {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
}

pub const LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        id: "rust",
        extensions: &["rs"],
    },
    LanguageSpec {
        id: "typescript",
        extensions: &["ts", "tsx", "js", "jsx", "mts", "cts"],
    },
];

/// `.rs` → `rust`; `.ts/.tsx/.js/.jsx/.mts/.cts` → `typescript`.
pub fn language_for_extension(ext: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|spec| spec.extensions.contains(&ext))
        .map(|spec| spec.id)
}

pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())?;
    language_for_extension(&ext)
}

/// Pick the default language for a workspace root: the one whose source
/// files actually exist (bounded scan), preferring the language with more
/// files; ties resolve to `rust` (deterministic).
pub fn detect_language(root: &Path) -> Option<&'static str> {
    let mut rust_files = 0usize;
    let mut ts_files = 0usize;
    let mut budget = 4000usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if budget == 0 {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "node_modules" | ".git" | "target" | "dist" | "build" | ".next" | "out"
                ) || name.starts_with('.')
                {
                    continue;
                }
                stack.push(path);
                continue;
            }
            budget -= 1;
            match language_for_path(&path) {
                Some("rust") => rust_files += 1,
                Some("typescript") => ts_files += 1,
                _ => {}
            }
        }
    }
    if rust_files == 0 && ts_files == 0 {
        return None;
    }
    if ts_files > rust_files {
        Some("typescript")
    } else {
        Some("rust")
    }
}

/// Resolve the command to spawn for `language` under `root`:
/// explicit config → env override → well-known candidates. The
/// workspace-local `node_modules/.bin` walk runs only with
/// `allow_workspace_binaries` — executing a binary that came with the
/// repository is the host operator's call, never the repository's.
pub fn resolve_server_command(
    config_servers: &BTreeMap<String, ServerCommand>,
    language: &str,
    root: &Path,
    allow_workspace_binaries: bool,
) -> Result<ServerCommand, agentkit_core::CoreError> {
    if let Some(explicit) = config_servers.get(language) {
        return Ok(explicit.clone());
    }
    let searched: Vec<String>;
    let found: Option<ServerCommand> = match language {
        "rust" => {
            searched = vec!["AGENTKIT_LSP_RUST".into(), "rust-analyzer".into()];
            env_command("AGENTKIT_LSP_RUST", &["rust-analyzer"], root, false)
        }
        "typescript" => {
            searched = vec![
                "AGENTKIT_LSP_TYPESCRIPT".into(),
                "typescript-language-server".into(),
            ];
            env_command(
                "AGENTKIT_LSP_TYPESCRIPT",
                &[
                    "node_modules/.bin/typescript-language-server",
                    "typescript-language-server",
                ],
                root,
                allow_workspace_binaries,
            )
        }
        other => {
            return Err(err(
                SERVER_NOT_FOUND,
                format!(
                    "no language server is registered for '{other}'; supported: {}",
                    LANGUAGES
                        .iter()
                        .map(|s| s.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    };
    found.ok_or_else(|| {
        err(
            SERVER_NOT_FOUND,
            format!(
                "no {language} language server binary was found; searched: {} \
                 (also checked AGENTKIT_LSP_{} and the {} configuration). \
                 Install the server or set the environment variable / servers \
                 configuration entry.",
                searched.join(", "),
                language.to_uppercase(),
                language
            ),
        )
    })
}

/// env override first (command + args split on whitespace), then candidate
/// paths: ancestor `node_modules/.bin/<name>` walk (when `node_bin_walk`),
/// then plain PATH lookup.
fn env_command(
    env_var: &str,
    candidates: &[&str],
    root: &Path,
    node_bin_walk: bool,
) -> Option<ServerCommand> {
    if let Ok(value) = std::env::var(env_var) {
        let mut parts = value.split_whitespace().map(str::to_owned);
        if let Some(command) = parts.next() {
            let args: Vec<String> = parts.collect();
            // Explicit env command: PATH, absolute, then root-relative.
            if let Some(found) = find_on_path(&command) {
                return Some(command_for(found, args));
            }
            let path = PathBuf::from(&command);
            let usable = if path.is_absolute() {
                path.exists().then_some(path)
            } else {
                let joined = root.join(&path);
                is_executable(&joined).then_some(joined)
            };
            if let Some(program) = usable {
                return Some(command_for(program, args));
            }
            // fall through to the well-known candidates
        }
    }
    resolve_program_nodejs_style(candidates, root, node_bin_walk)
}

fn resolve_program_nodejs_style(
    candidates: &[&str],
    root: &Path,
    node_bin_walk: bool,
) -> Option<ServerCommand> {
    for candidate in candidates {
        let base = candidate.rsplit('/').next().unwrap_or(candidate);
        // typescript-language-server: <ws>/node_modules/.bin/… and every
        // ancestor's node_modules/.bin up to the filesystem root, then PATH.
        if node_bin_walk {
            let mut dir = Some(root);
            while let Some(d) = dir {
                let bin = d.join(candidate);
                if is_executable(&bin) {
                    return Some(command_for(bin, default_args(base)));
                }
                dir = d.parent();
            }
        }
        if let Some(found) = find_on_path(base) {
            return Some(command_for(found, default_args(base)));
        }
    }
    None
}

fn default_args(candidate: &str) -> Vec<String> {
    // typescript-language-server speaks stdio only with --stdio.
    if candidate.contains("typescript-language-server") {
        vec!["--stdio".to_owned()]
    } else {
        vec![]
    }
}

fn command_for(path: PathBuf, args: Vec<String>) -> ServerCommand {
    ServerCommand {
        command: path.to_string_lossy().into_owned(),
        args,
        env: BTreeMap::new(),
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| {
            m.is_file() && {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
        })
        .unwrap_or(false)
}

/// Minimal `which`: first executable match in `$PATH`.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

// POSIX paths, permission bits and `#!/bin/sh` fakes; the pool refuses
// non-unix hosts outright.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn extension_mapping() {
        assert_eq!(language_for_extension("rs"), Some("rust"));
        for e in ["ts", "tsx", "js", "jsx", "mts", "cts"] {
            assert_eq!(language_for_extension(e), Some("typescript"), "{e}");
        }
        assert_eq!(language_for_extension("py"), None);
        assert_eq!(
            language_for_path(Path::new("/a/b/Main.tsx")),
            Some("typescript")
        );
        assert_eq!(language_for_path(Path::new("x.RS")), Some("rust"));
        assert_eq!(language_for_path(Path::new("noext")), None);
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let bin_dir = if name.contains('/') {
            dir.join(name).parent().unwrap().to_path_buf()
        } else {
            dir.to_path_buf()
        };
        fs::create_dir_all(&bin_dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn typescript_finds_ancestor_node_modules_bin() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("packages/app/src");
        fs::create_dir_all(&nested).unwrap();
        fake_bin(root.path(), "node_modules/.bin/typescript-language-server");
        let cmd = resolve_server_command(&BTreeMap::new(), "typescript", &nested, true).unwrap();
        assert!(cmd.command.ends_with("typescript-language-server"));
        assert_eq!(cmd.args, vec!["--stdio"]);
    }

    #[test]
    fn workspace_binaries_require_opt_in() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("packages/app/src");
        fs::create_dir_all(&nested).unwrap();
        let planted = fake_bin(root.path(), "node_modules/.bin/typescript-language-server");
        match resolve_server_command(&BTreeMap::new(), "typescript", &nested, false) {
            // A PATH-installed server stays acceptable; what must NEVER
            // happen without the opt-in is executing the binary that came
            // with the repository itself.
            Ok(cmd) => assert!(
                !cmd.command.starts_with(root.path().to_str().unwrap()),
                "repository-planted server executed without opt-in: {}",
                cmd.command
            ),
            Err(err) => assert_eq!(err.code, SERVER_NOT_FOUND),
        }
        let _ = planted;
    }

    #[test]
    fn missing_server_names_language_and_candidates() {
        let root = tempfile::tempdir().unwrap();
        let err =
            resolve_server_command(&BTreeMap::new(), "typescript", root.path(), true).unwrap_err();
        assert_eq!(err.code, SERVER_NOT_FOUND);
        assert!(err.message.contains("typescript"), "{}", err.message);
        assert!(
            err.message.contains("typescript-language-server"),
            "{}",
            err.message
        );
    }

    #[test]
    fn explicit_config_wins() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            ServerCommand {
                command: "/opt/custom-ra".into(),
                args: vec!["--quiet".into()],
                env: BTreeMap::new(),
            },
        );
        let cmd = resolve_server_command(&servers, "rust", Path::new("/tmp"), false).unwrap();
        assert_eq!(cmd.command, "/opt/custom-ra");
        assert_eq!(cmd.args, ["--quiet"]);
    }

    #[test]
    fn unknown_language_errors() {
        let err = resolve_server_command(&BTreeMap::new(), "cobol", Path::new("/tmp"), false)
            .unwrap_err();
        assert_eq!(err.code, SERVER_NOT_FOUND);
        assert!(err.message.contains("cobol"));
    }

    #[test]
    fn detection_prefers_the_populated_language() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.rs"), "fn a(){}").unwrap();
        assert_eq!(detect_language(root.path()), Some("rust"));
        fs::write(root.path().join("b.ts"), "1").unwrap();
        fs::write(root.path().join("c.tsx"), "1").unwrap();
        assert_eq!(detect_language(root.path()), Some("typescript"));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(detect_language(empty.path()), None);
    }

    #[test]
    fn node_modules_dirs_are_skipped_by_detection() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::write(root.path().join("node_modules/pkg/x.ts"), "1").unwrap();
        fs::write(root.path().join("real.rs"), "fn r(){}").unwrap();
        assert_eq!(detect_language(root.path()), Some("rust"));
    }
}
