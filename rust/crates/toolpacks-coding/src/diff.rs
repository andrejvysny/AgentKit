//! Minimal unified diff with 3 lines of context. Mirrors
//! `packages/toolpacks-coding/src/diff.ts`.

use crate::truncate::split_lines;

/// Cells of the LCS table above which the middle is shown as one block.
const MAX_LCS_CELLS: usize = 4_000_000;
const CONTEXT: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpKind {
    Same,
    Removed,
    Added,
}

impl OpKind {
    fn marker(self) -> &'static str {
        match self {
            OpKind::Same => " ",
            OpKind::Removed => "-",
            OpKind::Added => "+",
        }
    }
}

#[derive(Clone, Copy)]
struct Op<'a> {
    kind: OpKind,
    text: &'a str,
}

fn diff_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let mut head = 0usize;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0usize;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let mid_a = &a[head..a.len() - tail];
    let mid_b = &b[head..b.len() - tail];
    let mut ops: Vec<Op> = a[..head]
        .iter()
        .map(|text| Op {
            kind: OpKind::Same,
            text,
        })
        .collect();
    if mid_a.len().saturating_mul(mid_b.len()) > MAX_LCS_CELLS {
        for text in mid_a {
            ops.push(Op {
                kind: OpKind::Removed,
                text,
            });
        }
        for text in mid_b {
            ops.push(Op {
                kind: OpKind::Added,
                text,
            });
        }
    } else {
        ops.extend(lcs_ops(mid_a, mid_b));
    }
    for text in &a[a.len() - tail..] {
        ops.push(Op {
            kind: OpKind::Same,
            text,
        });
    }
    ops
}

fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let width = b.len() + 1;
    let mut table = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i * width + j] = if a[i] == b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            ops.push(Op {
                kind: OpKind::Same,
                text: a[i],
            });
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            ops.push(Op {
                kind: OpKind::Removed,
                text: a[i],
            });
            i += 1;
        } else {
            ops.push(Op {
                kind: OpKind::Added,
                text: b[j],
            });
            j += 1;
        }
    }
    while i < a.len() {
        ops.push(Op {
            kind: OpKind::Removed,
            text: a[i],
        });
        i += 1;
    }
    while j < b.len() {
        ops.push(Op {
            kind: OpKind::Added,
            text: b[j],
        });
        j += 1;
    }
    ops
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedDiff {
    pub diff: String,
    pub added: usize,
    pub removed: usize,
}

/// Minimal unified diff (3 lines of context) between two texts.
pub fn unified_diff(path: &str, before: &str, after: &str) -> UnifiedDiff {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    let ops = diff_ops(&before_lines, &after_lines);
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(index, op)| {
            if op.kind == OpKind::Same {
                None
            } else {
                Some(index)
            }
        })
        .collect();
    let added = ops.iter().filter(|op| op.kind == OpKind::Added).count();
    let removed = ops.iter().filter(|op| op.kind == OpKind::Removed).count();
    if changed.is_empty() {
        return UnifiedDiff {
            diff: String::new(),
            added,
            removed,
        };
    }
    let mut lines = vec![format!("--- a/{path}"), format!("+++ b/{path}")];
    // old_pos[i] / new_pos[i]: 1-based line numbers of ops[i] in each file.
    let mut old_pos = Vec::with_capacity(ops.len());
    let mut new_pos = Vec::with_capacity(ops.len());
    let mut o = 1usize;
    let mut n = 1usize;
    for op in &ops {
        old_pos.push(o);
        new_pos.push(n);
        if op.kind != OpKind::Added {
            o += 1;
        }
        if op.kind != OpKind::Removed {
            n += 1;
        }
    }
    let mut cursor = 0usize;
    while cursor < changed.len() {
        let mut first = changed[cursor];
        let mut last = first;
        cursor += 1;
        while cursor < changed.len() && changed[cursor] - last <= CONTEXT * 2 + 1 {
            last = changed[cursor];
            cursor += 1;
        }
        first = first.saturating_sub(CONTEXT);
        last = (last + CONTEXT).min(ops.len() - 1);
        let slice = &ops[first..last + 1];
        let old_count = slice.iter().filter(|op| op.kind != OpKind::Added).count();
        let new_count = slice.iter().filter(|op| op.kind != OpKind::Removed).count();
        let old_start = if old_count == 0 {
            old_pos[first] - 1
        } else {
            old_pos[first]
        };
        let new_start = if new_count == 0 {
            new_pos[first] - 1
        } else {
            new_pos[first]
        };
        lines.push(format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        for op in slice {
            lines.push(format!("{}{}", op.kind.marker(), op.text));
        }
    }
    UnifiedDiff {
        diff: lines.join("\n"),
        added,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_change_produces_one_hunk() {
        let d = unified_diff("f.txt", "one\ntwo\nthree\n", "one\nTWO\nthree\n");
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 1);
        assert_eq!(
            d.diff,
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three"
        );
    }

    #[test]
    fn identical_texts_produce_empty_diff() {
        let d = unified_diff("f.txt", "same\n", "same\n");
        assert_eq!(d.diff, "");
        assert_eq!(d.added, 0);
        assert_eq!(d.removed, 0);
    }

    #[test]
    fn far_apart_double_edit_has_two_hunks() {
        let body = (0..40)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let after = body.replace("l2\n", "L2\n").replace("l35\n", "L35\n");
        let d = unified_diff("d.txt", &body, &after);
        let hunks: Vec<&str> = d.diff.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(hunks, ["@@ -1,6 +1,6 @@", "@@ -33,7 +33,7 @@"]);
        assert_eq!(d.added, 2);
        assert_eq!(d.removed, 2);
    }

    #[test]
    fn pure_addition_uses_zero_start_hunk() {
        let d = unified_diff("f.txt", "", "a\n");
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 0);
        assert_eq!(d.diff, "--- a/f.txt\n+++ b/f.txt\n@@ -0,0 +1,1 @@\n+a");
    }

    #[test]
    fn pure_deletion_hunk() {
        let d = unified_diff("f.txt", "a\n", "");
        assert_eq!(d.added, 0);
        assert_eq!(d.removed, 1);
        assert_eq!(d.diff, "--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +0,0 @@\n-a");
    }

    #[test]
    fn adjacent_changes_merge_into_one_hunk() {
        // Gaps of at most 2*CONTEXT+1 keep one hunk (l2/l3 changed, gap of 0).
        let d = unified_diff("f.txt", "l0\nl1\nl2\nl3\nl4\n", "l0\nX\nY\nl3\nl4\n");
        assert_eq!(
            d.diff,
            "--- a/f.txt\n+++ b/f.txt\n@@ -1,5 +1,5 @@\n l0\n-l1\n-l2\n+X\n+Y\n l3\n l4"
        );
    }
}
