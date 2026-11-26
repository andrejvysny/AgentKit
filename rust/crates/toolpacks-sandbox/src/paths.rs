//! A path as the model writes it, turned into what the workspace service
//! takes. Relative paths resolve in the working folder; an absolute path is
//! taken literally: `/workspace/...` must lie inside the working folder
//! (the service joins the folder onto every `/workspace` path, so the
//! folder is stripped here), `~/...` and `/home/agent/...` name the home
//! tree, and anything else is `path_outside_workspace`. Repeated slashes
//! count once (`/workspace//a` is `/workspace/a`); collapsing them never
//! changes which segment a `..` removes, so an escape is still refused.

use agentkit_workspaces::{FileRef, PathRoot, RelPath, SandboxError};

const WORKSPACE: &str = "/workspace";
const HOME: &str = "/home/agent";

/// Which trees a tool may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Roots {
    /// `/workspace` only (a command's working directory).
    Workspace,
    /// `/workspace` and `/home/agent`.
    Both,
}

fn outside(text: &str) -> SandboxError {
    SandboxError::PathOutsideWorkspace {
        path: text.to_owned(),
    }
}

fn rel(text: &str, shown: &str) -> Result<RelPath, SandboxError> {
    RelPath::parse(text).map_err(|_| outside(shown))
}

/// `rest` below an absolute `/workspace`, made relative to `folder`.
fn inside_folder(
    rest: &str,
    folder: Option<&RelPath>,
    shown: &str,
) -> Result<RelPath, SandboxError> {
    let path = rel(rest, shown)?;
    let Some(folder) = folder.filter(|folder| !folder.is_root()) else {
        return Ok(path);
    };
    if !path.starts_with(folder) {
        return Err(SandboxError::PathOutsideWorkspace {
            path: format!("{shown} (outside the working folder {WORKSPACE}/{folder})"),
        });
    }
    let remainder = path.as_str()[folder.as_str().len()..].trim_start_matches('/');
    rel(remainder, shown)
}

/// The path below `prefix`, when `text` is `prefix` or lies below it.
fn below<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    match text.strip_prefix(prefix)? {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    }
}

/// `text` with each run of `/` written once.
fn collapse_slashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_slash = false;
    for c in text.chars() {
        if !(c == '/' && after_slash) {
            out.push(c);
        }
        after_slash = c == '/';
    }
    out
}

/// The service's view of the model's `text` (messages quote `text`).
pub(crate) fn file_ref(
    text: &str,
    folder: Option<&RelPath>,
    roots: Roots,
) -> Result<FileRef, SandboxError> {
    let path = collapse_slashes(text);
    let home = below(&path, HOME).or_else(|| below(&path, "~"));
    if let Some(rest) = home {
        if roots == Roots::Workspace {
            return Err(outside(text));
        }
        return Ok(FileRef {
            root: PathRoot::Home,
            path: rel(rest, text)?,
        });
    }
    if let Some(rest) = below(&path, WORKSPACE) {
        return Ok(FileRef::workspace(inside_folder(rest, folder, text)?));
    }
    if path.starts_with('/') {
        return Err(outside(text));
    }
    Ok(FileRef::workspace(rel(&path, text)?))
}

/// The absolute guest path of `file` (below the working folder for a
/// `/workspace` path), for messages and previews.
pub(crate) fn guest_path(folder: Option<&RelPath>, file: &FileRef) -> String {
    let joined = match (file.root, folder) {
        (PathRoot::Workspace, Some(folder)) => folder
            .join(&file.path)
            .unwrap_or_else(|_| file.path.clone()),
        _ => file.path.clone(),
    };
    root_path(file.root, &joined)
}

/// `path` below `root` as an absolute guest path.
pub(crate) fn root_path(root: PathRoot, path: &RelPath) -> String {
    if path.is_root() {
        root.guest_dir().to_owned()
    } else {
        format!("{}/{}", root.guest_dir(), path.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> RelPath {
        RelPath::parse("chats/c1").unwrap()
    }

    fn shown(text: &str, folder: Option<&RelPath>) -> String {
        let file = file_ref(text, folder, Roots::Both).unwrap();
        guest_path(folder, &file)
    }

    #[test]
    fn absolute_paths_round_trip_through_the_folder() {
        let f = folder();
        assert_eq!(shown("a.txt", Some(&f)), "/workspace/chats/c1/a.txt");
        assert_eq!(
            shown("/workspace/chats/c1/a.txt", Some(&f)),
            "/workspace/chats/c1/a.txt"
        );
        assert_eq!(
            shown("/workspace/chats/c1", Some(&f)),
            "/workspace/chats/c1"
        );
        assert_eq!(shown("/workspace/a.txt", None), "/workspace/a.txt");
        assert_eq!(shown("~/.npmrc", Some(&f)), "/home/agent/.npmrc");
        assert_eq!(shown("/home/agent/x/y", None), "/home/agent/x/y");
        assert_eq!(shown("~", None), "/home/agent");
        assert_eq!(
            shown("/workspace//chats/c1/a.txt", Some(&f)),
            "/workspace/chats/c1/a.txt"
        );
        assert_eq!(shown("//workspace//x///y", None), "/workspace/x/y");
        assert_eq!(shown("~//x", None), "/home/agent/x");
        assert_eq!(shown("/home/agent//x", None), "/home/agent/x");
    }

    #[test]
    fn paths_outside_the_folder_or_the_roots_are_refused() {
        let f = folder();
        for text in [
            "/workspace/chats/c2/a.txt",
            "/workspace/chats/c1/../c2/a",
            "../c2/a.txt",
            "/etc/passwd",
            "/tmp/x",
            "/workspacex/a",
            "/home/agentx",
            "/workspace//chats/c1/..//../c2/a",
            "/workspace//..//etc",
            "//etc/passwd",
            "/home/agent//../agent2/x",
        ] {
            let error = file_ref(text, Some(&f), Roots::Both).unwrap_err();
            assert_eq!(error.code(), "path_outside_workspace", "{text}");
        }
        let home = file_ref("~/x", None, Roots::Workspace).unwrap_err();
        assert_eq!(home.code(), "path_outside_workspace");
    }
}
