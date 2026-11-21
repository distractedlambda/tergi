//! Showing more or fewer context lines around the hunks of a diff.
//!
//! The user can move the top and bottom edges of a hunk. Moving an edge out
//! shows an unchanged line from the file next to the hunk, and moving it in
//! hides a context line. `regroup` builds the hunks again from the diff and
//! those choices. When the context of two hunks meets, they become one.
//!
//! The unchanged lines between hunks come from the full texts of both
//! sides, which we load for syntax highlighting anyway.

use std::collections::BTreeMap;

use super::commit::FileTexts;
use super::diff::{DiffLine, Hunk, LineKind};

/// The side of a diff whose line numbers name the unchanged lines. Staging
/// changes one side of a diff and leaves the other alone. Naming lines by
/// the side that stays put keeps the user's choices on the same lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl Side {
    /// The number of `line` on this side.
    pub fn number(self, line: &DiffLine) -> Option<u32> {
        match self {
            Side::Old => line.old_no,
            Side::New => line.new_no,
        }
    }

    /// The first line that `hunk` covers on this side, and how many it
    /// covers.
    fn range(self, hunk: &Hunk) -> (u32, u32) {
        match self {
            Side::Old => (first_line(hunk.old_start, hunk.old_len), hunk.old_len),
            Side::New => (first_line(hunk.new_start, hunk.new_len), hunk.new_len),
        }
    }
}

/// The unchanged lines that the user chose to show (`true`) or hide
/// (`false`), by their number on one side. Other lines show up if git put
/// them in the diff.
pub type Choices = BTreeMap<u32, bool>;

/// The number of the first line a hunk covers. For an empty range, a hunk
/// header gives the line before it.
fn first_line(start: u32, len: u32) -> u32 {
    if len == 0 { start + 1 } else { start }
}

/// Which line to show or hide to move an edge of `hunk` by one line, and
/// whether to show it. `top` picks the edge, and `out` says whether it
/// moves away from the hunk's middle. Returns `None` when the hunk has no
/// context line to hide at that edge, or starts at the top of the file.
/// The line after the bottom edge may not exist. `regroup` then leaves
/// the hunk as it is.
pub fn edge_line(hunk: &Hunk, side: Side, top: bool, out: bool) -> Option<(u32, bool)> {
    let (first, len) = side.range(hunk);
    if out {
        let n = if top { first.checked_sub(1)? } else { first + len };
        (n > 0).then_some((n, true))
    } else {
        let line = if top {
            hunk.lines.first()
        } else {
            hunk.lines.last()
        }?;
        if line.kind != LineKind::Context {
            return None;
        }
        Some((side.number(line)?, false))
    }
}

/// The lines of one side of a file.
struct Text<'a> {
    lines: Vec<&'a str>,
    /// Whether the last line ends in a newline.
    eol: bool,
}

impl<'a> Text<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines: Vec<&str> = text.split('\n').collect();
        let eol = text.ends_with('\n') || text.is_empty();
        if eol {
            lines.pop();
        }
        Text { lines, eol }
    }

    fn len(&self) -> u32 {
        self.lines.len() as u32
    }

    /// Line `n`, counting from 1, and whether it lacks a newline.
    fn line(&self, n: u32) -> Option<(&'a str, bool)> {
        let text = self.lines.get(n.checked_sub(1)? as usize)?;
        Some((text, n == self.len() && !self.eol))
    }
}

/// A line of the file in order, with whether it shows up and which hunk of
/// the diff it came from.
struct Item {
    /// `None` when the texts don't tell us what the line is.
    line: Option<DiffLine>,
    shown: bool,
    hunk: Option<usize>,
    /// The numbers of the next old and new line at this point.
    at: (u32, u32),
}

/// Build the hunks of a file again with the unchanged lines in `choices`
/// shown or hidden. Without choices, this gives back `hunks`.
///
/// A line between hunks can only be shown if both texts have it, with the
/// same text and line ending. The texts are read separately from the diff,
/// so the file may have changed in between. Lines that aren't valid UTF-8
/// aren't shown either, because a patch needs their bytes and the texts
/// only have a lossy copy.
pub fn regroup(hunks: &[Hunk], texts: &FileTexts, side: Side, choices: &Choices) -> Vec<Hunk> {
    if choices.is_empty() {
        return hunks.to_vec();
    }
    let old = texts.old.as_deref().map(Text::new);
    let new = texts.new.as_deref().map(Text::new);
    let unchanged = |o: u32, n: u32| -> Option<DiffLine> {
        let (a, a_no_eol) = old.as_ref()?.line(o)?;
        let (b, b_no_eol) = new.as_ref()?.line(n)?;
        if a != b || a_no_eol != b_no_eol || a.contains('\u{FFFD}') {
            return None;
        }
        let mut line = DiffLine::new(LineKind::Context, a.as_bytes(), Some(o), Some(n));
        line.no_eol = a_no_eol;
        Some(line)
    };
    let choice = |o: u32, n: u32| {
        let k = match side {
            Side::Old => o,
            Side::New => n,
        };
        choices.get(&k).copied()
    };
    let gap = |items: &mut Vec<Item>, o: u32, n: u32| {
        let line = unchanged(o, n);
        let shown = line.is_some() && choice(o, n) == Some(true);
        items.push(Item {
            line,
            shown,
            hunk: None,
            at: (o, n),
        });
    };

    let mut items = Vec::new();
    let (mut o, mut n) = (1, 1);
    for (hi, h) in hunks.iter().enumerate() {
        let first_o = first_line(h.old_start, h.old_len);
        let first_n = first_line(h.new_start, h.new_len);
        if first_o < o || first_n < n || first_o - o != first_n - n {
            // Not a diff we understand.
            return hunks.to_vec();
        }
        while o < first_o {
            gap(&mut items, o, n);
            o += 1;
            n += 1;
        }
        for l in &h.lines {
            let shown = match l.kind {
                LineKind::Context => choice(o, n) != Some(false),
                LineKind::Add | LineKind::Del => true,
            };
            items.push(Item {
                line: Some(l.clone()),
                shown,
                hunk: Some(hi),
                at: (o, n),
            });
            o += u32::from(l.kind != LineKind::Add);
            n += u32::from(l.kind != LineKind::Del);
        }
    }
    // The rest of the file, if both texts agree on how long it is.
    if let (Some(a), Some(b)) = (&old, &new)
        && a.len().checked_sub(o) == b.len().checked_sub(n)
    {
        while o <= a.len() {
            gap(&mut items, o, n);
            o += 1;
            n += 1;
        }
    }

    let mut out = Vec::new();
    for run in items.split(|i| !i.shown) {
        let Some(first_change) = run
            .iter()
            .find(|i| i.line.as_ref().is_some_and(|l| l.kind != LineKind::Context))
        else {
            // Shown lines that don't reach a change, which can happen when
            // the file changed since the user picked them.
            continue;
        };
        let lines: Vec<DiffLine> = run.iter().filter_map(|i| i.line.clone()).collect();
        let old_len = lines.iter().filter(|l| l.kind != LineKind::Add).count() as u32;
        let new_len = lines.iter().filter(|l| l.kind != LineKind::Del).count() as u32;
        let (o, n) = run[0].at;
        out.push(Hunk {
            old_start: if old_len == 0 { o - 1 } else { o },
            old_len,
            new_start: if new_len == 0 { n - 1 } else { n },
            new_len,
            section: first_change
                .hunk
                .and_then(|h| hunks.get(h))
                .map(|h| h.section.clone())
                .unwrap_or_default(),
            lines,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;
    use crate::git::commit::diff_texts;

    fn texts(old: &str, new: &str) -> FileTexts {
        FileTexts {
            old: Some(Arc::from(old)),
            new: Some(Arc::from(new)),
        }
    }

    fn render(hunks: &[Hunk]) -> String {
        let mut s = String::new();
        for h in hunks {
            s.push_str(&h.header());
            s.push('\n');
            for l in &h.lines {
                s.push(match l.kind {
                    LineKind::Context => ' ',
                    LineKind::Add => '+',
                    LineKind::Del => '-',
                });
                s.push_str(&l.text);
                s.push('\n');
                if l.no_eol {
                    s.push_str("\\ No newline at end of file\n");
                }
            }
        }
        s
    }

    const OLD: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    const NEW: &str = "1\n2\nthree\n4\n5\n6\n7\n8\nnine\n10\n";

    fn choices(list: &[(u32, bool)]) -> Choices {
        list.iter().copied().collect()
    }

    #[test]
    fn hunks_grow_hide_and_merge() {
        let hunks = diff_texts(OLD.as_bytes(), NEW.as_bytes(), 1);
        let t = texts(OLD, NEW);
        insta::assert_snapshot!(render(&hunks), @r"
        @@ -2,3 +2,3 @@
         2
        -3
        +three
         4
        @@ -8,3 +8,3 @@
         8
        -9
        +nine
         10
        ");
        // Show line 1 above the first hunk and hide line 10 below the last.
        let c = choices(&[(1, true), (10, false)]);
        insta::assert_snapshot!(render(&regroup(&hunks, &t, Side::New, &c)), @r"
        @@ -1,4 +1,4 @@
         1
         2
        -3
        +three
         4
        @@ -8,2 +8,2 @@
         8
        -9
        +nine
        ");
        // Showing the lines between them makes one hunk.
        let c = choices(&[(5, true), (6, true), (7, true)]);
        insta::assert_snapshot!(render(&regroup(&hunks, &t, Side::New, &c)), @r"
        @@ -2,9 +2,9 @@
         2
        -3
        +three
         4
         5
         6
         7
         8
        -9
        +nine
         10
        ");
        // A line that the texts don't have stays hidden.
        let c = choices(&[(5, true), (6, true), (7, true)]);
        let partial = FileTexts {
            old: Some(Arc::from("1\n2\n3\n4\n5\nsix\n7\n8\n9\n10\n")),
            new: t.new.clone(),
        };
        insta::assert_snapshot!(render(&regroup(&hunks, &partial, Side::New, &c)), @r"
        @@ -2,4 +2,4 @@
         2
        -3
        +three
         4
         5
        @@ -7,4 +7,4 @@
         7
         8
        -9
        +nine
         10
        ");
    }

    #[test]
    fn hunks_with_one_empty_side() {
        let old = "a\nb\nc\n";
        let new = "a\nb\nc\nd";
        let hunks = diff_texts(old.as_bytes(), new.as_bytes(), 0);
        let t = texts(old, new);
        let hidden = regroup(&hunks, &t, Side::New, &choices(&[(3, true)]));
        insta::assert_snapshot!(render(&hidden), @r"
        @@ -3 +3,2 @@
         c
        +d
        \ No newline at end of file
        ");
        let only_change = regroup(&hidden, &t, Side::New, &choices(&[(3, false)]));
        assert_eq!(only_change, hunks);
        assert_eq!(only_change[0].old_start, 3);
        assert_eq!(only_change[0].old_len, 0);
    }

    #[test]
    fn edges() {
        let hunks = diff_texts(OLD.as_bytes(), NEW.as_bytes(), 1);
        let (a, b) = (&hunks[0], &hunks[1]);
        assert_eq!(edge_line(a, Side::New, true, true), Some((1, true)));
        assert_eq!(edge_line(a, Side::New, true, false), Some((2, false)));
        assert_eq!(edge_line(a, Side::New, false, true), Some((5, true)));
        assert_eq!(edge_line(b, Side::New, false, false), Some((10, false)));
        let top = diff_texts(b"1\n", b"one\n", 0);
        assert_eq!(edge_line(&top[0], Side::Old, true, true), None);
        assert_eq!(edge_line(&top[0], Side::Old, true, false), None);
    }

    fn text() -> impl Strategy<Value = String> {
        let line = prop::sample::select(vec!["a", "b", "c", "", "x\r"]);
        (prop::collection::vec(line, 0..30), any::<bool>()).prop_map(|(lines, eol)| {
            let mut s = lines.join("\n");
            if eol && !lines.is_empty() {
                s.push('\n');
            }
            s
        })
    }

    /// Apply hunks to the old text, checking each line they keep or remove.
    fn apply(old: &str, hunks: &[Hunk]) -> String {
        let old: Vec<&str> = old.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut pos = 0;
        for h in hunks {
            let first = if h.old_len == 0 {
                h.old_start
            } else {
                h.old_start - 1
            } as usize;
            assert!(first >= pos, "hunks overlap or are out of order");
            out.extend(old[pos..first].iter().copied());
            pos = first;
            for l in &h.lines {
                if l.kind != LineKind::Add {
                    assert_eq!(old[pos].strip_suffix('\n').unwrap_or(old[pos]), l.text);
                    assert_eq!(!old[pos].ends_with('\n'), l.no_eol);
                    pos += 1;
                }
                if l.kind != LineKind::Del {
                    out.push_str(&l.text);
                    if !l.no_eol {
                        out.push('\n');
                    }
                }
            }
        }
        out.extend(old[pos..].iter().copied());
        out
    }

    proptest! {
        #[test]
        fn regrouped_hunks_still_turn_old_into_new(
            old in text(),
            new in text(),
            context in 0u32..4,
            picks in prop::collection::vec((1u32..35, any::<bool>()), 0..20),
            side_old in any::<bool>(),
        ) {
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let t = texts(&old, &new);
            let side = if side_old { Side::Old } else { Side::New };
            prop_assert_eq!(&regroup(&hunks, &t, side, &Choices::new()), &hunks);
            // Choices that agree with the diff change nothing.
            let same: Choices = hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == LineKind::Context)
                .filter_map(|l| side.number(l))
                .map(|n| (n, true))
                .collect();
            prop_assert_eq!(&regroup(&hunks, &t, side, &same), &hunks);

            let choices: Choices = picks.into_iter().collect();
            let got = regroup(&hunks, &t, side, &choices);
            prop_assert_eq!(apply(&old, &got), new);
            let changed = |hs: &[Hunk]| -> Vec<DiffLine> {
                hs.iter()
                    .flat_map(|h| &h.lines)
                    .filter(|l| l.kind != LineKind::Context)
                    .cloned()
                    .collect()
            };
            prop_assert_eq!(changed(&got), changed(&hunks));
            for h in &got {
                let count = |k: LineKind| h.lines.iter().filter(|l| l.kind == k).count() as u32;
                prop_assert_eq!(h.old_len, count(LineKind::Context) + count(LineKind::Del));
                prop_assert_eq!(h.new_len, count(LineKind::Context) + count(LineKind::Add));
                let (mut o, mut n) = (h.old_start.max(1), h.new_start.max(1));
                for l in &h.lines {
                    let want_old = (l.kind != LineKind::Add).then_some(o);
                    let want_new = (l.kind != LineKind::Del).then_some(n);
                    prop_assert_eq!((l.old_no, l.new_no), (want_old, want_new));
                    o += u32::from(want_old.is_some());
                    n += u32::from(want_new.is_some());
                    // A context line shows up when the user chose it, or
                    // when git put it in the diff and the user didn't hide
                    // it.
                    if l.kind == LineKind::Context {
                        let k = side.number(l).unwrap_or(0);
                        prop_assert_ne!(choices.get(&k), Some(&false));
                    }
                }
                prop_assert!(h.lines.iter().any(|l| l.kind != LineKind::Context));
            }
            // Hunks never touch, or they would be one.
            let first = |h: &Hunk| if h.old_len == 0 { h.old_start } else { h.old_start - 1 };
            for w in got.windows(2) {
                prop_assert!(first(&w[1]) > first(&w[0]) + w[0].old_len, "hunks touch");
            }
        }
    }
}
