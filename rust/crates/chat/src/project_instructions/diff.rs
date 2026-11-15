//! A minimal line-based unified diff (LCS), for the rules-changed note of the
//! turn context (roadmap 1.4). The workspace carries no diff crate and the
//! inputs are instruction files of at most 64 KiB.

const CONTEXT: usize = 3;
/// Beyond this many LCS cells the diff is not computed; the caller sends the
/// full text instead.
const MAX_CELLS: usize = 4_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// Unified-diff hunks turning `old` into `new`; `None` when the line
/// sequences are equal or too large to compare.
pub(super) fn unified_diff(old: &str, new: &str) -> Option<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let (n, m) = (am.len(), bm.len());
    if n.saturating_mul(m) > MAX_CELLS {
        return None;
    }
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if am[i] == bm[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let mut ops: Vec<(Op, &str)> = a[..head].iter().map(|l| (Op::Keep, *l)).collect();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && am[i] == bm[j] {
            ops.push((Op::Keep, am[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
            ops.push((Op::Del, am[i]));
            i += 1;
        } else {
            ops.push((Op::Add, bm[j]));
            j += 1;
        }
    }
    ops.extend(a[a.len() - tail..].iter().map(|l| (Op::Keep, *l)));
    render(&ops)
}

fn render(ops: &[(Op, &str)]) -> Option<String> {
    // Each change claims CONTEXT lines around it; overlapping claims merge.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (index, (op, _)) in ops.iter().enumerate() {
        if *op == Op::Keep {
            continue;
        }
        let (start, end) = (
            index.saturating_sub(CONTEXT),
            (index + CONTEXT + 1).min(ops.len()),
        );
        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (start, end) in ranges {
        let before = &ops[..start];
        let old_line = before.iter().filter(|(op, _)| *op != Op::Add).count() + 1;
        let new_line = before.iter().filter(|(op, _)| *op != Op::Del).count() + 1;
        let hunk = &ops[start..end];
        let old_len = hunk.iter().filter(|(op, _)| *op != Op::Add).count();
        let new_len = hunk.iter().filter(|(op, _)| *op != Op::Del).count();
        out.push_str(&format!(
            "@@ -{old_line},{old_len} +{new_line},{new_len} @@\n"
        ));
        for (op, line) in hunk {
            out.push(match op {
                Op::Keep => ' ',
                Op::Del => '-',
                Op::Add => '+',
            });
            out.push_str(line);
            out.push('\n');
        }
    }
    out.truncate(out.trim_end_matches('\n').len());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::unified_diff;

    #[test]
    fn equal_text_has_no_diff() {
        assert_eq!(unified_diff("a\nb\n", "a\nb\n"), None);
    }

    #[test]
    fn a_changed_line_is_one_hunk_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\nfour\n5\n6\n7\n8\n";
        assert_eq!(
            unified_diff(old, new).unwrap(),
            "@@ -1,7 +1,7 @@\n 1\n 2\n 3\n-4\n+four\n 5\n 6\n 7"
        );
    }

    #[test]
    fn distant_changes_make_separate_hunks() {
        let lines = |two: &str, nineteen: &str| -> String {
            (1..=20)
                .map(|n| match n {
                    2 => format!("{two}\n"),
                    19 => format!("{nineteen}\n"),
                    _ => format!("{n}\n"),
                })
                .collect()
        };
        let (old, new) = (lines("2", "19"), lines("two", "nineteen"));
        let diff = unified_diff(&old, &new).unwrap();
        assert_eq!(diff.matches("@@ ").count(), 2, "{diff}");
        assert!(diff.contains("-2\n+two"));
        assert!(diff.contains("-19\n+nineteen"));
    }

    #[test]
    fn insertions_and_deletions_are_found() {
        let diff = unified_diff("a\nb\nc", "a\nc\nd").unwrap();
        assert_eq!(diff, "@@ -1,3 +1,3 @@\n a\n-b\n c\n+d");
    }
}
