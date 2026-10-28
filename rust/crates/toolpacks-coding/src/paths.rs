//! Workspace confinement and the sensitive-path denylist. Mirrors
//! `packages/toolpacks-coding/src/paths.ts` (adapted from OneMind, MIT).

use std::path::{Path, PathBuf};

use crate::errors::CodingError;

fn sensitive_directories() -> &'static [&'static str] {
    &[
        ".git",
        ".ssh",
        ".aws",
        ".gnupg",
        ".kube",
        ".docker",
        ".azure",
        ".terraform.d",
    ]
}

fn sensitive_extensions() -> &'static [&'static str] {
    &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".p8",
        ".jwk",
        ".kdbx",
    ]
}

fn sensitive_names() -> &'static [&'static str] {
    &[
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".htpasswd",
        ".pgpass",
    ]
}

/// A name that merely starts with one of these is sensitive (`id_rsa.pub`).
fn sensitive_prefixes() -> &'static [&'static str] {
    &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "id_ed448",
        "credentials",
        "secrets",
        "service-account",
    ]
}

/// Terraform state holds provider credentials in clear (`x.tfstate.backup`).
fn sensitive_infixes() -> &'static [&'static str] {
    &[".tfstate"]
}

/// Committed `.env` templates document configuration; real env files do not.
fn template_suffixes() -> &'static [&'static str] {
    &[
        ".example",
        ".sample",
        ".template",
        ".tpl",
        ".dist",
        ".default",
    ]
}

/// Glob excludes mirroring the denylist, for tools that cannot filter per file.
pub fn sensitive_git_excludes() -> Vec<String> {
    let mut excludes: Vec<String> = sensitive_directories()
        .iter()
        .filter(|name| **name != ".git")
        .map(|name| format!(":(exclude,glob)**/{name}/**"))
        .collect();
    excludes.extend(
        sensitive_extensions()
            .iter()
            .map(|ext| format!(":(exclude,glob)**/*{ext}")),
    );
    excludes.extend(
        sensitive_names()
            .iter()
            .map(|name| format!(":(exclude,glob)**/{name}")),
    );
    excludes.extend(
        sensitive_prefixes()
            .iter()
            .map(|name| format!(":(exclude,glob)**/{name}*")),
    );
    excludes.extend(
        sensitive_infixes()
            .iter()
            .map(|infix| format!(":(exclude,glob)**/*{infix}*")),
    );
    excludes.push(":(exclude,glob)**/.env".to_owned());
    excludes.push(":(exclude,glob)**/.env.*".to_owned());
    excludes.push(":(exclude,glob)**/*.env".to_owned());
    excludes
}

/// True when one path segment names a credential, key or git metadata.
pub fn is_sensitive_segment(raw: &str) -> bool {
    let segment = raw.trim().to_lowercase();
    if segment.is_empty() || segment == "." || segment == "**" {
        return false;
    }
    if sensitive_directories().contains(&segment.as_str())
        || sensitive_names().contains(&segment.as_str())
    {
        return true;
    }
    if sensitive_prefixes()
        .iter()
        .any(|prefix| segment.starts_with(prefix))
    {
        return true;
    }
    if sensitive_infixes()
        .iter()
        .any(|infix| segment.contains(infix))
    {
        return true;
    }
    if segment.starts_with(".env") {
        return !template_suffixes()
            .iter()
            .any(|suffix| segment.ends_with(suffix));
    }
    if segment.ends_with(".env") {
        return true;
    }
    match segment.rfind('.') {
        Some(dot) if dot > 0 => sensitive_extensions().contains(&&segment[dot..]),
        _ => false,
    }
}

fn segments_of(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect()
}

pub fn is_git_segment(segment: &str) -> bool {
    segment.to_lowercase() == ".git"
}

/// True when any segment of a root-relative path is sensitive.
pub fn is_sensitive_path(path: &str) -> bool {
    segments_of(path).into_iter().any(is_sensitive_segment)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfinedPath {
    /// Canonical workspace root.
    pub root: String,
    /// Canonical target: existing parents resolved, missing children appended.
    pub path: String,
    /// Target relative to `root`; `""` for the root itself.
    pub relative_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug)]
pub struct ConfineOptions {
    pub access: PathAccess,
    pub allow_sensitive: bool,
}

fn denied(code: &str, message: impl Into<String>) -> CodingError {
    CodingError::new(code, message)
}

fn escapes(rel: &str) -> bool {
    rel == ".." || rel.starts_with("../") || rel == "/" || rel.starts_with('/')
}

/// JS `path.relative(from, to)` for absolute POSIX paths (the canonical root
/// is always absolute). Falls back to the target itself when `to` is not
/// under `from` by component, which `escapes` then rejects.
pub fn relative(from: &str, to: &str) -> String {
    let from_parts: Vec<&str> = from.split('/').filter(|p| !p.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|p| !p.is_empty()).collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = vec!["..".to_owned(); from_parts.len() - common];
    parts.extend(to_parts[common..].iter().map(|p| (*p).to_owned()));
    parts.join("/")
}

/// JS `path.resolve(root, raw)` lexical normalisation for POSIX paths:
/// an absolute `raw` replaces `root`; `.` and `..` collapse.
fn resolve_path(root: &str, raw: &str) -> String {
    let base = if raw.starts_with('/') {
        String::new()
    } else {
        root.to_owned()
    };
    let mut stack: Vec<&str> = base
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    format!("/{}", stack.join("/"))
}

fn parent_of(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        None | Some(0) => Some("/"),
        Some(at) => Some(&trimmed[..at]),
    }
}

/// Canonical root, or a clear error when the workspace is not usable.
pub async fn canonical_root(root: &str) -> Result<String, CodingError> {
    tokio::fs::canonicalize(root)
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| {
            denied(
                "workspace_unavailable",
                "the workspace root does not exist or cannot be resolved",
            )
        })
}

async fn is_symlink(path: &str) -> bool {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Resolve `raw` against the canonical root and refuse anything that leaves
/// it. Symlinks are followed to their real target; a dangling symlink counts
/// as an escape because writing through it would create its target somewhere
/// else.
pub async fn resolve_inside_root(root: &str, raw: &str) -> Result<ConfinedPath, CodingError> {
    let utf16_len = raw.encode_utf16().count();
    if raw.is_empty() || utf16_len > 4096 {
        return Err(denied(
            "path_invalid",
            "path must be a non-empty string (max 4096)",
        ));
    }
    if raw.contains('\0') {
        return Err(denied("path_invalid", "path contains a NUL byte"));
    }
    if raw.starts_with('~') {
        return Err(denied(
            "path_invalid",
            "paths starting with ~ are not supported",
        ));
    }
    let target = resolve_path(root, raw);
    let mut existing = target;
    let mut missing: Vec<String> = vec![];
    loop {
        match tokio::fs::canonicalize(&existing).await {
            Ok(resolved) => {
                existing = resolved.to_string_lossy().into_owned();
                break;
            }
            Err(_) => {
                if is_symlink(&existing).await {
                    return Err(denied(
                        "path_dangling_symlink",
                        format!("{raw} is a dangling symlink; refusing to follow it"),
                    ));
                }
                let parent = parent_of(&existing).unwrap_or("/");
                if parent == existing {
                    return Err(denied(
                        "path_outside_root",
                        format!("{raw} is outside the workspace"),
                    ));
                }
                missing.insert(0, existing[parent.len() + 1..].to_owned());
                existing = parent.to_owned();
            }
        }
    }
    let path = if missing.is_empty() {
        existing
    } else {
        Path::new(&existing)
            .join(PathBuf::from(missing.join("/")))
            .to_string_lossy()
            .into_owned()
    };
    let relative_path = relative(root, &path);
    if escapes(&relative_path) {
        return Err(denied(
            "path_outside_root",
            format!("{raw} is outside the workspace"),
        ));
    }
    Ok(ConfinedPath {
        root: root.to_owned(),
        path,
        relative_path,
    })
}

/// The full gate every path argument passes through: confinement, the `.git`
/// write ban, and the sensitive denylist. The denylist is checked on the raw
/// segments AND on the resolved root-relative path, so a symlink inside the
/// workspace that points at `.env` is refused too.
pub async fn confine(
    root: &str,
    raw: &str,
    options: ConfineOptions,
) -> Result<ConfinedPath, CodingError> {
    let confined = resolve_inside_root(root, raw).await?;
    let lexical = relative(root, &resolve_path(root, raw));
    let candidates = [&confined.relative_path, lexical.as_str()]
        .into_iter()
        .filter(|rel| !escapes(rel))
        .collect::<Vec<_>>();
    let segments: Vec<&str> = candidates.into_iter().flat_map(segments_of).collect();
    if options.access == PathAccess::Write && segments.iter().any(|s| is_git_segment(s)) {
        return Err(denied(
            "path_git_write",
            "writes under .git are never allowed",
        ));
    }
    if !options.allow_sensitive
        && let Some(hit) = segments
            .into_iter()
            .find(|segment| is_sensitive_segment(segment))
    {
        return Err(denied(
            "path_sensitive",
            format!("{raw} names a sensitive path ({hit}); refused"),
        ));
    }
    Ok(confined)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_root(tag: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "ak-paths-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&path).await.unwrap();
        tokio::fs::canonicalize(&path)
            .await
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    const STRICT: ConfineOptions = ConfineOptions {
        access: PathAccess::Read,
        allow_sensitive: false,
    };
    const WRITE: ConfineOptions = ConfineOptions {
        access: PathAccess::Write,
        allow_sensitive: false,
    };

    async fn code(root: &str, raw: &str, options: ConfineOptions) -> String {
        confine(root, raw, options)
            .await
            .expect_err("must be refused")
            .code
    }

    #[tokio::test]
    async fn refuses_escapes() {
        let root = temp_root("escape").await;
        assert_eq!(code(&root, "../x", STRICT).await, "path_outside_root");
        assert_eq!(code(&root, "a/../../x", STRICT).await, "path_outside_root");
        assert_eq!(
            code(&root, "/etc/passwd", STRICT).await,
            "path_outside_root"
        );
    }

    #[tokio::test]
    async fn allows_inside_and_root_itself() {
        let root = temp_root("inside").await;
        tokio::fs::write(format!("{root}/a.txt"), b"x")
            .await
            .unwrap();
        let confined = confine(&root, &format!("{root}/a.txt"), STRICT)
            .await
            .unwrap();
        assert_eq!(confined.relative_path, "a.txt");
        let root_confined = confine(&root, ".", STRICT).await.unwrap();
        assert_eq!(root_confined.relative_path, "");
    }

    #[tokio::test]
    async fn refuses_symlink_pointing_outside() {
        let root = temp_root("symroot").await;
        let outside = temp_root("symoutside").await;
        tokio::fs::write(format!("{outside}/secret.txt"), b"s")
            .await
            .unwrap();
        std::os::unix::fs::symlink(&outside, format!("{root}/link")).unwrap();
        std::os::unix::fs::symlink(format!("{outside}/secret.txt"), format!("{root}/file-link"))
            .unwrap();
        for target in ["link", "link/secret.txt", "file-link", "link/new.txt"] {
            assert_eq!(code(&root, target, STRICT).await, "path_outside_root");
            assert_eq!(code(&root, target, WRITE).await, "path_outside_root");
        }
    }

    #[tokio::test]
    async fn refuses_dangling_symlink() {
        let root = temp_root("dangling").await;
        std::os::unix::fs::symlink(format!("{root}/missing-target"), format!("{root}/dangling"))
            .unwrap();
        assert_eq!(
            code(&root, "dangling", WRITE).await,
            "path_dangling_symlink"
        );
        assert_eq!(
            code(&root, "dangling", STRICT).await,
            "path_dangling_symlink"
        );
    }

    #[tokio::test]
    async fn refuses_malformed() {
        let root = temp_root("malformed").await;
        for bad in ["~", "~/x", "", "a\0b"] {
            assert_eq!(code(&root, bad, STRICT).await, "path_invalid", "{bad:?}");
        }
    }

    #[tokio::test]
    async fn symlink_to_sensitive_file_is_refused() {
        let root = temp_root("symenv").await;
        tokio::fs::write(format!("{root}/.env"), b"A=1")
            .await
            .unwrap();
        std::os::unix::fs::symlink(format!("{root}/.env"), format!("{root}/innocent.txt")).unwrap();
        assert_eq!(code(&root, "innocent.txt", STRICT).await, "path_sensitive");
    }

    #[tokio::test]
    async fn new_file_under_missing_directory_resolves() {
        let root = temp_root("missing-dir").await;
        let confined = confine(&root, "a/b/c.txt", WRITE).await.unwrap();
        assert_eq!(confined.relative_path, "a/b/c.txt");
    }

    const SENSITIVE: &[&str] = &[
        ".ssh/config",
        ".aws/credentials",
        ".gnupg/key",
        ".kube/config",
        ".docker/config.json",
        ".azure/x",
        ".terraform.d/x",
        "server.pem",
        "a.key",
        "a.p12",
        "a.pfx",
        "a.jks",
        "a.keystore",
        "a.p8",
        "a.jwk",
        "a.kdbx",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        ".htpasswd",
        "id_rsa",
        "id_rsa.pub",
        "credentials.json",
        "secrets.yaml",
        "service-account.json",
        "service-account-prod.json",
        ".pgpass",
        "terraform.tfstate",
        "terraform.tfstate.backup",
        ".env",
        ".env.local",
        ".env.production",
        "nested/dir/.env",
        ".SSH/config",
    ];

    #[tokio::test]
    async fn sensitive_denylist() {
        let root = temp_root("sensitive").await;
        let open_read = ConfineOptions {
            access: PathAccess::Read,
            allow_sensitive: true,
        };
        let open_write = ConfineOptions {
            access: PathAccess::Write,
            allow_sensitive: true,
        };
        for rel in SENSITIVE {
            assert_eq!(code(&root, rel, STRICT).await, "path_sensitive", "{rel}");
            assert_eq!(code(&root, rel, WRITE).await, "path_sensitive", "{rel}");
            confine(&root, rel, open_read)
                .await
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            confine(&root, rel, open_write)
                .await
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
        }
        for rel in [
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            "config.ts",
        ] {
            confine(&root, rel, STRICT)
                .await
                .unwrap_or_else(|e| panic!("{rel} is a normal file: {e}"));
        }
    }

    #[tokio::test]
    async fn git_paths() {
        let root = temp_root("git").await;
        tokio::fs::create_dir_all(format!("{root}/.git/hooks"))
            .await
            .unwrap();
        assert_eq!(code(&root, ".git/config", STRICT).await, "path_sensitive");
        assert_eq!(code(&root, ".git/hooks/x", WRITE).await, "path_git_write");
        let open_write = ConfineOptions {
            access: PathAccess::Write,
            allow_sensitive: true,
        };
        assert_eq!(
            code(&root, ".git/hooks/x", open_write).await,
            "path_git_write"
        );
        let open_read = ConfineOptions {
            access: PathAccess::Read,
            allow_sensitive: true,
        };
        confine(&root, ".git/HEAD", open_read).await.unwrap();
    }
}
