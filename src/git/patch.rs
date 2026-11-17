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
///
/// The patch is bytes, because the lines in it are copied from the file as
/// they are, and the file needn't be UTF-8.
pub fn build_patch(
    file: &FileDiff,
    selected: &dyn Fn(usize, usize) -> bool,
    reverse: bool,
) -> Result<Option<Vec<u8>>> {
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
        let mut body: Vec<(u8, &[u8], bool)> = Vec::new();
        let mut any = false;
        for (li, line) in h.lines.iter().enumerate() {
            let sel = line.kind != LineKind::Context && selected(hi, li);
            any |= sel;
            let tag = match (line.kind, sel, reverse) {
                (LineKind::Context, _, _) => b' ',
                (LineKind::Add, true, _) => b'+',
                (LineKind::Del, true, _) => b'-',
                (LineKind::Add, false, false) | (LineKind::Del, false, true) => continue,
                (LineKind::Del, false, false) | (LineKind::Add, false, true) => b' ',
            };
            body.push((tag, line.bytes(), line.no_eol));
        }
        if !any {
            continue;
        }
        // A line without a trailing newline must be the last line on its
        // side of the patch. Leaving out part of a change at the end of the
        // file can break that, and the patch would be garbage, so refuse.
        let ends_badly = |side: u8| {
            body.iter()
                .filter(|(tag, _, _)| *tag == b' ' || *tag == side)
                .rev()
                .skip(1)
                .any(|(_, _, no_eol)| *no_eol)
        };
        if ends_badly(b'-') || ends_badly(b'+') {
            bail!(
                "This selection splits a change to the last line of the file. \
                 Select the whole change at the end of the file instead."
            );
        }
        let old_count = body.iter().filter(|(t, _, _)| *t != b'+').count() as i64;
        let new_count = body.iter().filter(|(t, _, _)| *t != b'-').count() as i64;
        // Where the hunk begins on the side we apply the patch to, and where
        // it begins on the other side once the earlier hunks are applied.
        let (old_first, new_first) = if reverse {
            let new_first = first_line(h.new_start, h.new_len);
            (new_first - delta, new_first)
        } else {
            let old_first = first_line(h.old_start, h.old_len);
            (old_first, old_first + delta)
        };
        delta += new_count - old_count;
        out.extend_from_slice(
            format!(
                "@@ -{},{} +{},{} @@\n",
                range_start(old_first, old_count, reverse),
                old_count,
                range_start(new_first, new_count, !reverse),
                new_count
            )
            .as_bytes(),
        );
        for (tag, text, no_eol) in body {
            out.push(tag);
            out.extend_from_slice(text);
            out.push(b'\n');
            if no_eol {
                out.extend_from_slice(b"\\ No newline at end of file\n");
            }
        }
    }
    Ok(Some(out))
}

/// The number of the first line a hunk covers. For an empty range, a hunk
/// header gives the line before it instead.
fn first_line(start: u32, len: u32) -> i64 {
    if len == 0 {
        i64::from(start) + 1
    } else {
        i64::from(start)
    }
}

/// The start to write in a hunk header for a range that begins at line
/// `first`. `git apply` only needs this to be exact for patches without
/// context lines, where it has nothing else to find the place by.
///
/// For an empty range, a hunk header normally gives the line before it. But
/// `git apply` guesses where a hunk goes from the start of the side it is
/// applying towards (`towards`), and takes that number as is. For a hunk that
/// only removes lines, the guess is then one line too early. If that line
/// has the same text, git removes it instead. That changes nothing, unless
/// the removed line is the last one and has no newline. Then git also drops
/// the newline from the line before. So on that side we give the exact line.
fn range_start(first: i64, count: i64, towards: bool) -> i64 {
    if count == 0 && !towards {
        (first - 1).max(0)
    } else {
        first.max(0)
    }
}

/// The file header of the patch. Paths go in as bytes, because a file
/// name needn't be UTF-8.
fn header(file: &FileDiff, full: bool, reverse: bool) -> Vec<u8> {
    let old = file
        .old_path
        .as_ref()
        .or(file.new_path.as_ref())
        .map_or(&b""[..], |p| p.as_slice());
    let new = file
        .new_path
        .as_ref()
        .or(file.old_path.as_ref())
        .map_or(&b""[..], |p| p.as_slice());
    let a = quote_path(&[b"a/", old].concat());
    let b = quote_path(&[b"b/", new].concat());
    let mut s = Vec::new();
    let mut put = |parts: &[&[u8]]| {
        for p in parts {
            s.extend_from_slice(p);
        }
    };
    put(&[b"diff --git ", &a, b" ", &b, b"\n"]);
    if file.is_new() && (!reverse || full) {
        let mode = file.new_mode.as_deref().unwrap_or("100644");
        put(&[b"new file mode ", mode.as_bytes(), b"\n"]);
        put(&[b"--- /dev/null\n+++ ", &b, b"\n"]);
    } else if file.is_deleted() && (reverse || full) {
        let mode = file.old_mode.as_deref().unwrap_or("100644");
        put(&[b"deleted file mode ", mode.as_bytes(), b"\n"]);
        // Applied in reverse, this patch creates the file. `git apply`
        // then ignores the mode above and makes a plain file, unless the
        // patch also names a new mode.
        if reverse {
            put(&[b"new mode ", mode.as_bytes(), b"\n"]);
        }
        put(&[b"--- ", &a, b"\n+++ /dev/null\n"]);
    } else {
        if full
            && let (Some(om), Some(nm)) = (&file.old_mode, &file.new_mode)
            && om != nm
        {
            put(&[
                b"old mode ",
                om.as_bytes(),
                b"\nnew mode ",
                nm.as_bytes(),
                b"\n",
            ]);
        }
        put(&[b"--- ", &a, b"\n+++ ", &b, b"\n"]);
    }
    s
}

/// Quote a path the way git does when it contains bytes that would confuse
/// the patch parser. Like git with `core.quotePath` off, this leaves bytes
/// over 0x7f alone, whether or not they are UTF-8.
pub fn quote_path(p: &[u8]) -> Vec<u8> {
    let special = |b: u8| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f;
    if !p.iter().any(|&b| special(b)) {
        return p.to_vec();
    }
    let mut s = vec![b'"'];
    for &b in p {
        match b {
            b'"' => s.extend_from_slice(b"\\\""),
            b'\\' => s.extend_from_slice(b"\\\\"),
            b'\n' => s.extend_from_slice(b"\\n"),
            b'\t' => s.extend_from_slice(b"\\t"),
            b'\r' => s.extend_from_slice(b"\\r"),
            b if special(b) => s.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            b => s.push(b),
        }
    }
    s.push(b'"');
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
        let p = build_patch(f, &|_, l| l == 1 || l == 3, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(p).unwrap(),
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n a\n-b\n c\n+B\n d\n"
        );
    }

    #[test]
    fn unstage_one_pair() {
        let f = &parse_unified(DIFF, DEFAULT_LINE_LIMIT)[0];
        let p = build_patch(f, &|_, l| l == 1 || l == 3, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(p).unwrap(),
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n a\n-b\n+B\n C\n d\n"
        );
    }

    #[test]
    fn keeps_bytes_that_are_not_utf8() {
        // "caf\xe9" is "café" in Latin-1.
        let diff = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n caf\xe9\n-x\n+y\xff\n";
        let f = &parse_unified(diff, DEFAULT_LINE_LIMIT)[0];
        assert_eq!(f.hunks[0].lines[0].text, "caf\u{fffd}");
        let p = build_patch(f, &|_, l| l == 2, false).unwrap().unwrap();
        assert_eq!(
            p,
            b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,3 @@\n caf\xe9\n x\n+y\xff\n"
        );
    }

    #[test]
    fn nothing_selected() {
        let f = &parse_unified(DIFF, DEFAULT_LINE_LIMIT)[0];
        assert!(build_patch(f, &|_, _| false, false).unwrap().is_none());
    }

    #[test]
    fn refuses_to_split_a_newline_change() {
        // Unstaging only "-c" would need "c" without a newline followed by
        // another line.
        let diff = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -2 +2 @@
-c
\\ No newline at end of file
+c
";
        let f = &parse_unified(diff, DEFAULT_LINE_LIMIT)[0];
        assert!(build_patch(f, &|_, l| l == 0, true).is_err());
        assert!(build_patch(f, &|_, l| l == 1, false).is_err());
        assert!(build_patch(f, &|_, _| true, true).unwrap().is_some());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_path(b"a/b c"), b"a/b c");
        assert_eq!(quote_path(b"a/b\"c"), b"\"a/b\\\"c\"");
        assert_eq!(quote_path(b"a/\tx"), b"\"a/\\tx\"");
        assert_eq!(quote_path(b"a/\x01\x7f"), b"\"a/\\001\\177\"");
        assert_eq!(quote_path(b"caf\xe9"), b"caf\xe9");
    }

    #[test]
    fn keeps_paths_that_are_not_utf8() {
        let diff =
            b"diff --git a/caf\xe9 b/caf\xe9\n--- a/caf\xe9\n+++ b/caf\xe9\n@@ -1 +1 @@\n-x\n+y\n";
        let f = &parse_unified(diff, DEFAULT_LINE_LIMIT)[0];
        let p = build_patch(f, &|_, _| true, false).unwrap().unwrap();
        assert_eq!(
            p,
            b"diff --git a/caf\xe9 b/caf\xe9\n--- a/caf\xe9\n+++ b/caf\xe9\n@@ -1,1 +1,1 @@\n-x\n+y\n"
        );
    }
}
