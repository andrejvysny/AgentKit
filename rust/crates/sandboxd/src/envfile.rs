//! The image's command environment: `/etc/sandbox/environment`, one
//! `KEY=VALUE` per line. Blank lines and lines starting with `#` are
//! skipped; the key must be a shell identifier; the value is taken
//! verbatim (no quoting, no expansion, no `export`); a later key replaces
//! an earlier one. Malformed lines are skipped and only counted, never
//! echoed, so a log never carries the file's content.

use std::path::Path;

/// The variables of `path`, in file order. A missing or unreadable file
/// gives an empty environment: commands still run, with the shell's
/// built-in defaults.
pub(crate) fn load(path: &Path) -> Vec<(String, String)> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let (vars, skipped) = parse(&text);
            if skipped > 0 {
                eprintln!(
                    "agentkit-sandboxd: skipped {skipped} malformed line(s) in {}",
                    path.display()
                );
            }
            vars
        }
        Err(error) => {
            eprintln!(
                "agentkit-sandboxd: cannot read {} ({error}); commands get an empty environment",
                path.display()
            );
            Vec::new()
        }
    }
}

/// The variables of `text` and the count of malformed lines.
pub(crate) fn parse(text: &str) -> (Vec<(String, String)>, usize) {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut skipped = 0;
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            skipped += 1;
            continue;
        };
        if !valid_key(key) || value.contains('\0') {
            skipped += 1;
            continue;
        }
        match vars.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1 = value.to_owned(),
            None => vars.push((key.to_owned(), value.to_owned())),
        }
    }
    (vars, skipped)
}

fn valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests;
