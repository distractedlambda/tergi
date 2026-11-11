//! Building partial patches for staging, unstaging and discarding selected
//! lines, to be fed to `git apply`.
//!
//! The trick is the same one `git add -p` and `git gui` use. When applying a
//! patch forward (staging), an unselected `-` line still exists in the target,
//! so it becomes context, and an unselected `+` line is left out. When applying
//! in reverse (unstaging or discarding), the target is the "new" side of the
//! diff, so the rule flips: an unselected `+` line becomes context and an
//! unselected `-` line is left out.

use anyhow::{Result, bail};

use super::diff::{FileDiff, LineKind};

/// Build a patch containing only the selected lines of `file`.
///
/// `selected(hunk, line)` says whether a line is selected. Context lines are
/// never "selected". Returns `Ok(None)` when no changed line is selected.
pub fn build_patch(
    file: &FileDiff,
    selected: &dyn Fn(usize, usize) -> bool,
    reverse: bool,
) -> Result<Option<String>> {
    let mut total = 0;
    let mut chosen = 0;
    for (hi, h) in file.hunks.iter().enumerate() {
        for li in h.changed_lines() {
            total += 1;
            if selected(hi, li) {
                chosen += 1;
            }
        }
    }
    if chosen == 0 {
        return Ok(None);
    }
    let full = chosen == total;

    let mut out = header(file, full, reverse);
    let mut delta: i64 = 0;
    for (hi, h) in file.hunks.iter().enumerate() {
        let mut body: Vec<(char, &str, bool)> = Vec::new();
        let mut any = false;
        for (li, line) in h.lines.iter().enumerate() {
            let sel = line.kind != LineKind::Context && selected(hi, li);
            any |= sel;
            let tag = match (line.kind, sel, reverse) {
                (LineKind::Context, _, _) => ' ',
                (LineKind::Add, true, _) => '+',
                (LineKind::Del, true, _) => '-',
                (LineKind::Add, false, false) | (LineKind::Del, false, true) => continue,
                (LineKind::Del, false, false) | (LineKind::Add, false, true) => ' ',
            };
            body.push((tag, &line.text, line.no_eol));
        }
        if !any {
            continue;
        }
        // A line without a trailing newline must be the last line on its
        // side of the patch. If turning a change into context breaks that,
        // the result would be garbage, so refuse instead.
        for (i, (tag, _, no_eol)) in body.iter().enumerate() {
            if *no_eol && *tag == ' ' && i + 1 < body.len() {
                bail!(
                    "This selection splits a change to the last line of the file. \
                     Select the whole change at the end of the file instead."
                );
            }
        }
        let old_count = body.iter().filter(|(t, _, _)| *t != '+').count() as i64;
        let new_count = body.iter().filter(|(t, _, _)| *t != '-').count() as i64;
        let (old_start, new_start) = if reverse {
            let new_start = i64::from(h.new_start);
            (new_start - delta, new_start)
        } else {
            let old_start = i64::from(h.old_start);
            (old_start, old_start + delta)
        };
        delta += new_count - old_count;
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start.max(0),
            old_count,
            new_start.max(0),
            new_count
        ));
        for (tag, text, no_eol) in body {
            out.push(tag);
            out.push_str(text);
            out.push('\n');
            if no_eol {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    Ok(Some(out))
}

fn header(file: &FileDiff, full: bool, reverse: bool) -> String {
    let old = file.old_path.as_deref().or(file.new_path.as_deref()).unwrap_or("");
    let new = file.new_path.as_deref().or(file.old_path.as_deref()).unwrap_or("");
    let a = quote_path(&format!("a/{old}"));
    let b = quote_path(&format!("b/{new}"));
    let mut s = format!("diff --git {a} {b}\n");
    if file.is_new() && (!reverse || full) {
        let mode = file.new_mode.as_deref().unwrap_or("100644");
        s.push_str(&format!("new file mode {mode}\n--- /dev/null\n+++ {b}\n"));
    } else if file.is_deleted() && (reverse || full) {
        let mode = file.old_mode.as_deref().unwrap_or("100644");
        s.push_str(&format!("deleted file mode {mode}\n--- {a}\n+++ /dev/null\n"));
    } else {
        if full
            && let (Some(om), Some(nm)) = (&file.old_mode, &file.new_mode)
            && om != nm
        {
            s.push_str(&format!("old mode {om}\nnew mode {nm}\n"));
        }
        s.push_str(&format!("--- {a}\n+++ {b}\n"));
    }
    s
}

/// Quote a path the way git does when it contains characters that would
/// confuse the patch parser.
pub fn quote_path(p: &str) -> String {
    let needs = p
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control());
    if !needs {
        return p.to_string();
    }
    let mut s = String::from("\"");
    for c in p.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            '\r' => s.push_str("\\r"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    s.push_str(&format!("\\{b:03o}"));
                }
            }
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DEFAULT_LINE_LIMIT, parse_unified};

    const DIFF: &str = "\
diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -1,4 +1,4 @@
 a
-b
-c
+B
+C
 d
";

    #[test]
    fn stage_one_pair() {
        let f = &parse_unified(DIFF, DEFAULT_LINE_LIMIT)[0];
        // Select "-b" (line 1) and "+B" (line 3).
        let p = build_patch(f, &|_, l| l == 1 || l == 3, false).unwrap().unwrap();
        assert_eq!(
            p,
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n a\n-b\n c\n+B\n d\n"
        );
    }

    #[test]
    fn unstage_one_pair() {
        let f = &parse_unified(DIFF, DEFAULT_LINE_LIMIT)[0];
        let p = build_patch(f, &|_, l| l == 1 || l == 3, true).unwrap().unwrap();
        assert_eq!(
            p,
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n a\n-b\n+B\n C\n d\n"
        );
    }

    #[test]
    fn nothing_selected() {
        let f = &parse_unified(DIFF, DEFAULT_LINE_LIMIT)[0];
        assert!(build_patch(f, &|_, _| false, false).unwrap().is_none());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_path("a/b c"), "a/b c");
        assert_eq!(quote_path("a/b\"c"), "\"a/b\\\"c\"");
        assert_eq!(quote_path("a/\tx"), "\"a/\\tx\"");
    }
}
