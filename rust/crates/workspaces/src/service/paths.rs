//! Where a tool's path lands inside the guest. Below `/workspace` a
//! relative path resolves inside the binding's folder; below `/home/agent`
//! the folder does not apply. `RelPath` already refuses `..` above its
//! root, so a resolved path can never leave it.

use crate::error::SandboxError;
use crate::ids::{OperationId, Sha256};
use crate::path::{PathRoot, RelPath};

use super::types::FileRef;

fn outside(text: &str) -> SandboxError {
    SandboxError::PathOutsideWorkspace {
        path: text.to_owned(),
    }
}

fn parse(text: &str) -> Result<RelPath, SandboxError> {
    RelPath::parse(text).map_err(|_| outside(text))
}

/// `file` resolved against `folder`: the path a backend receives.
pub(super) fn resolve(folder: Option<&RelPath>, file: &FileRef) -> Result<FileRef, SandboxError> {
    let path = match (file.root, folder) {
        (PathRoot::Workspace, Some(folder)) => folder
            .join(&file.path)
            .map_err(|_| outside(file.path.as_str()))?,
        _ => file.path.clone(),
    };
    Ok(FileRef {
        root: file.root,
        path,
    })
}

/// The working directory of a command: `directory` inside `folder`.
pub(super) fn working_dir(
    folder: Option<&RelPath>,
    directory: Option<&RelPath>,
) -> Result<RelPath, SandboxError> {
    let directory = directory.cloned().unwrap_or_else(RelPath::root);
    Ok(resolve(folder, &FileRef::workspace(directory))?.path)
}

/// The guest path for messages and the operation summary.
pub(super) fn display(file: &FileRef) -> String {
    if file.path.is_root() {
        file.root.guest_dir().to_owned()
    } else {
        format!("{}/{}", file.root.guest_dir(), file.path.as_str())
    }
}

/// The full command log of `id` below `/workspace`. An id that is not a
/// plain file name is named by its digest instead.
pub(super) fn log_path(id: &OperationId) -> RelPath {
    let text = id.as_str();
    let plain = !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let name = if plain {
        text.to_owned()
    } else {
        Sha256::of(text.as_bytes()).to_string()
    };
    RelPath::parse(&format!(".sandbox/logs/{name}.log")).expect("a log name is one plain segment")
}

/// A copy-in file name: exactly one path segment.
pub(super) fn file_name(name: &str) -> Result<RelPath, SandboxError> {
    let path = parse(name)?;
    if path.segments().count() != 1 || name.contains('/') {
        return Err(outside(name));
    }
    Ok(path)
}

/// The `n`th name to try for a copy-in: `name` itself, then `stem-n.ext`.
pub(super) fn numbered(name: &RelPath, n: u32) -> Result<RelPath, SandboxError> {
    if n == 0 {
        return Ok(name.clone());
    }
    let text = name.as_str();
    let numbered = match text.rfind('.') {
        Some(dot) if dot > 0 => format!("{}-{n}{}", &text[..dot], &text[dot..]),
        _ => format!("{text}-{n}"),
    };
    parse(&numbered)
}

/// The parent directory of a path, and its last segment.
pub(super) fn split_parent(path: &RelPath) -> Option<(RelPath, String)> {
    let segments: Vec<&str> = path.segments().collect();
    let (last, parents) = segments.split_last()?;
    let parent = RelPath::parse(&parents.join("/")).ok()?;
    Some((parent, (*last).to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).unwrap()
    }

    #[test]
    fn workspace_paths_resolve_in_the_folder_home_paths_do_not() {
        let folder = rel("chats/c1");
        let file = FileRef::workspace(rel("a/b.txt"));
        assert_eq!(
            resolve(Some(&folder), &file).unwrap().path,
            rel("chats/c1/a/b.txt")
        );
        let home = FileRef {
            root: PathRoot::Home,
            path: rel(".npmrc"),
        };
        assert_eq!(resolve(Some(&folder), &home).unwrap(), home);
        assert_eq!(display(&home), "/home/agent/.npmrc");
        assert_eq!(working_dir(None, None).unwrap(), RelPath::root());
    }

    #[test]
    fn copy_in_names_are_one_segment_and_numbered() {
        assert!(file_name("a/b").is_err());
        assert!(file_name("..").is_err());
        assert!(file_name("").is_err());
        let name = file_name("report.tar.gz").unwrap();
        assert_eq!(numbered(&name, 0).unwrap(), name);
        assert_eq!(numbered(&name, 2).unwrap(), rel("report.tar-2.gz"));
        assert_eq!(numbered(&rel(".env"), 1).unwrap(), rel(".env-1"));
        assert_eq!(numbered(&rel("Makefile"), 1).unwrap(), rel("Makefile-1"));
    }

    #[test]
    fn log_names_are_plain_or_hashed() {
        let plain = log_path(&OperationId::new("ab12-cd"));
        assert_eq!(plain, rel(".sandbox/logs/ab12-cd.log"));
        let odd = log_path(&OperationId::new("../x"));
        assert_eq!(odd.segments().count(), 3);
        assert!(odd.as_str().ends_with(".log") && !odd.as_str().contains(".."));
    }
}
