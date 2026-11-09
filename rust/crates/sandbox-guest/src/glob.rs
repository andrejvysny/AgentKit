//! Hand-rolled glob matching with toolpacks `compile_glob` semantics:
//! `*` (no `/`), `?` (one non-`/`), `**` (crosses `/`; `**/` matches any
//! number of leading segments), `[...]` classes (with `!`/`^` negation),
//! `{a,b}` alternation. No regex, so no compile cost and no unicode panics.

/// Match `text` against `pattern` (full-string match, no anchoring surprises).
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    m(&p, 0, &t, 0)
}

/// A glob matches the relative path, or just the file name for bare globs
/// (same rule as the coding pack's `glob_matches`).
#[must_use]
pub fn glob_match_path(pattern: &str, rel: &str) -> bool {
    glob_match(pattern, rel) || glob_match(pattern, basename(rel))
}

fn basename(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((_, name)) => name,
        None => rel,
    }
}

/// Backtracking matcher. Inputs are bounded upstream (glob <= 4 KiB, paths
/// <= 4 KiB), so worst-case backtracking stays cheap.
fn m(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    loop {
        if pi == p.len() {
            return ti == t.len();
        }
        match p[pi] {
            '*' if p.get(pi + 1) == Some(&'*') => {
                // `**` — greedy across '/'. `**/x` ≡ (`**/` eats segments) or `x`.
                let rest_pi = if p.get(pi + 2) == Some(&'/') {
                    pi + 3
                } else {
                    pi + 2
                };
                // Try every split point for the `.*`-like span.
                for k in ti..=t.len() {
                    if m(p, rest_pi, t, k) {
                        return true;
                    }
                }
                return false;
            }
            '*' => {
                for k in ti..=t.len() {
                    if t[k..].contains(&'/') {
                        break;
                    }
                    if m(p, pi + 1, t, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti < t.len() && t[ti] != '/' && m(p, pi + 1, t, ti + 1) {
                    return true;
                }
                return false;
            }
            '{' => {
                if let Some(end) = p[pi + 1..].iter().position(|c| *c == '}') {
                    let end = pi + 1 + end;
                    for alt in split_alternatives(&p[pi + 1..end]) {
                        // Expand `{alt}rest` by matching alt against a prefix.
                        if match_alt(&alt, &p[end + 1..], t, ti) {
                            return true;
                        }
                    }
                    return false;
                }
                // Unbalanced brace is literal.
                if ti < t.len() && t[ti] == '{' {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                return false;
            }
            '[' => {
                if let Some(end) = class_end(p, pi + 1) {
                    if ti < t.len() && class_matches(&p[pi + 1..end], t[ti]) {
                        // class_end returns the index of ']'; continue after it.
                        pi = end + 1;
                        ti += 1;
                        continue;
                    }
                    return false;
                }
                // Unbalanced '[' is literal.
                if ti < t.len() && t[ti] == '[' {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                return false;
            }
            c => {
                if ti < t.len() && t[ti] == c {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                return false;
            }
        }
    }
}

/// Match one brace alternative followed by the remaining pattern.
fn match_alt(alt: &[char], rest: &[char], t: &[char], ti: usize) -> bool {
    // Concatenate alt + rest and match from ti.
    let mut joined = alt.to_vec();
    joined.extend_from_slice(rest);
    m(&joined, 0, t, ti)
}

/// Split brace body on top-level commas (no nested braces handled: matches
/// toolpacks, which splits flat).
fn split_alternatives(inner: &[char]) -> Vec<Vec<char>> {
    let mut out: Vec<Vec<char>> = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    for c in inner {
        if *c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(*c);
        }
    }
    out.push(cur);
    out
}

/// Index of the `]` closing the class opened at `p[start-1]`.
fn class_end(p: &[char], start: usize) -> Option<usize> {
    let mut from = start;
    if matches!(p.get(from), Some('!') | Some('^')) {
        from += 1;
    }
    if p.get(from) == Some(&']') {
        from += 1;
    }
    p.get(from..)?
        .iter()
        .position(|c| *c == ']')
        .map(|off| from + off)
}

fn class_matches(body: &[char], c: char) -> bool {
    let (negated, members) = if matches!(body.first(), Some('!') | Some('^')) {
        (true, &body[1..])
    } else {
        (false, body)
    };
    let mut hit = false;
    let mut i = 0usize;
    while i < members.len() {
        if i + 2 < members.len() && members[i + 1] == '-' {
            if members[i] <= c && c <= members[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if members[i] == c {
                hit = true;
            }
            i += 1;
        }
    }
    hit != negated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_and_question() {
        assert!(glob_match("*.ts", "a.ts"));
        assert!(!glob_match("*.ts", "d/a.ts"));
        assert!(glob_match("**/*.test.tsx", "src/d/a.test.tsx"));
        assert!(glob_match("**/*.test.tsx", "a.test.tsx"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn braces_and_classes() {
        assert!(glob_match("*.{js,ts}", "x.ts"));
        assert!(glob_match("*.{js,ts}", "x.js"));
        assert!(!glob_match("*.{js,ts}", "x.rs"));
        assert!(glob_match("[abc]*", "bzz"));
        assert!(glob_match("[!abc]*", "dzz"));
        assert!(!glob_match("[!abc]*", "azz"));
        assert!(glob_match("[]x]", "]"));
    }

    #[test]
    fn path_or_basename_semantics() {
        assert!(glob_match_path("*.rs", "src/a.rs")); // basename match
        assert!(glob_match_path("src/*.rs", "src/a.rs"));
        assert!(!glob_match_path("src/*.rs", "src/d/a.rs"));
        assert!(glob_match_path("src/**/*.rs", "src/d/a.rs"));
    }
}
