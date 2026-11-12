//! Word-level highlighting inside changed lines.
//!
//! In each run of removed lines followed by added lines, we pair them up in
//! order and diff each pair word by word. The words that differ get a
//! stronger background in the diff view.

use imara_diff::{Algorithm, Diff, InternedInput};

use super::diff::{Hunk, LineKind};

/// Byte ranges to emphasize, for each line of the hunk.
pub type Marks = Vec<Vec<(usize, usize)>>;

pub fn hunk_marks(hunk: &Hunk) -> Marks {
    let mut marks: Marks = vec![Vec::new(); hunk.lines.len()];
    let lines = &hunk.lines;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Add {
            i += 1;
        }
        let dels = add_start - del_start;
        let adds = i - add_start;
        for k in 0..dels.min(adds) {
            let (a, b) = line_marks(&lines[del_start + k].text, &lines[add_start + k].text);
            marks[del_start + k] = a;
            marks[add_start + k] = b;
        }
    }
    marks
}

fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let class = |c: char| {
            if c.is_alphanumeric() || c == '_' {
                0
            } else if c.is_whitespace() {
                1
            } else {
                2
            }
        };
        let k = class(c);
        if k == 2 {
            out.push(&s[i..i + c.len_utf8()]);
            start = i + c.len_utf8();
            continue;
        }
        while let Some(&(_, n)) = chars.peek() {
            if class(n) != k {
                break;
            }
            chars.next();
        }
        let end = chars.peek().map_or(s.len(), |&(j, _)| j);
        out.push(&s[start.max(i)..end]);
        start = end;
    }
    out
}

/// Returns emphasized byte ranges for the old and the new line.
pub fn line_marks(old: &str, new: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    if old.len() > 2000 || new.len() > 2000 {
        return (Vec::new(), Vec::new());
    }
    let a = tokens(old);
    let b = tokens(new);
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(a.iter().copied());
    input.update_after(b.iter().copied());
    let diff = Diff::compute(Algorithm::Myers, &input);

    let ranges = |toks: &[&str], changed: &dyn Fn(u32) -> bool| -> (Vec<(usize, usize)>, usize) {
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0;
        let mut changed_bytes = 0;
        for (i, t) in toks.iter().enumerate() {
            let end = pos + t.len();
            if changed(i as u32) {
                changed_bytes += t.trim().len();
                match out.last_mut() {
                    Some(last) if last.1 == pos => last.1 = end,
                    _ => out.push((pos, end)),
                }
            }
            pos = end;
        }
        (out, changed_bytes)
    };
    let (ra, ca) = ranges(&a, &|i| diff.is_removed(i));
    let (rb, cb) = ranges(&b, &|i| diff.is_added(i));
    let total = old.trim().len() + new.trim().len();
    // When most of the line changed, highlighting words is just noise.
    if total == 0 || (ca + cb) * 10 > total * 6 {
        return (Vec::new(), Vec::new());
    }
    (ra, rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize() {
        assert_eq!(tokens("foo(bar, 12)"), ["foo", "(", "bar", ",", " ", "12", ")"]);
        assert_eq!(tokens("  a"), ["  ", "a"]);
    }

    #[test]
    fn marks_changed_word() {
        let (a, b) = line_marks("let x = foo(1);", "let x = bar(1);");
        assert_eq!(a, [(8, 11)]);
        assert_eq!(b, [(8, 11)]);
    }

    #[test]
    fn no_marks_when_everything_changed() {
        let (a, b) = line_marks("completely different", "nothing alike here");
        assert!(a.is_empty() && b.is_empty());
    }
}
