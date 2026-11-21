//! Showing more or fewer unchanged lines around the hunks of a diff.
//!
//! The user can move the top and bottom edges of a hunk. Moving an edge out
//! shows an unchanged line from the file next to the hunk, and moving it in
//! hides a context line. `view` works out which lines the detail pane shows
//! and how they make up hunks. When two hunks meet, the pane shows them as
//! one.
//!
//! This only changes what the pane shows. Each hunk in the pane is made of
//! whole hunks of git's, and staging it applies those as git gave them. So
//! a patch never holds a line that git didn't give us.
//!
//! The unchanged lines between git's hunks come from the full texts of both
//! sides, which we load for syntax highlighting anyway.

use std::collections::BTreeMap;
use std::ops::Range;

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

/// A line that the pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Line `.1` of git's hunk `.0`.
    Diff(usize, usize),
    /// An unchanged line that none of git's hunks has, by its index in
    /// `View::extra`.
    Extra(usize),
}

/// One hunk as the pane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The hunks of git's that this one is made of.
    pub hunks: Range<usize>,
    /// The line ranges and section name for the header. It has no lines.
    pub header: Hunk,
    pub lines: Vec<Shown>,
}

/// The hunks of one file as the pane shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub groups: Vec<Group>,
    /// The unchanged lines that came from the texts.
    pub extra: Vec<DiffLine>,
}

impl View {
    /// Each of git's hunks on its own, as git gave it.
    pub fn plain(hunks: &[Hunk]) -> View {
        let groups = hunks
            .iter()
            .enumerate()
            .map(|(i, h)| Group {
                hunks: i..i + 1,
                header: header_of(h),
                lines: (0..h.lines.len()).map(|l| Shown::Diff(i, l)).collect(),
            })
            .collect();
        View {
            groups,
            extra: Vec::new(),
        }
    }

    pub fn line<'a>(&'a self, hunks: &'a [Hunk], line: Shown) -> Option<&'a DiffLine> {
        match line {
            Shown::Diff(h, l) => hunks.get(h)?.lines.get(l),
            Shown::Extra(i) => self.extra.get(i),
        }
    }

    /// The group that has git's hunk `hunk` in it.
    pub fn group_of(&self, hunk: usize) -> Option<&Group> {
        let i = self.groups.partition_point(|g| g.hunks.end <= hunk);
        self.groups.get(i).filter(|g| g.hunks.contains(&hunk))
    }
}

/// A hunk's ranges and section name, without its lines.
fn header_of(h: &Hunk) -> Hunk {
    Hunk {
        old_start: h.old_start,
        old_len: h.old_len,
        new_start: h.new_start,
        new_len: h.new_len,
        section: h.section.clone(),
        lines: Vec::new(),
    }
}

/// Which line to show or hide to move an edge of `group` by one line, and
/// whether to show it. `top` picks the edge, and `out` says whether it
/// moves away from the group's middle. Returns `None` when the group has no
/// context line to hide at that edge, or starts at the top of the file.
/// The line after the bottom edge may not exist. `view` then shows the
/// same thing as before.
pub fn edge_line(
    view: &View,
    hunks: &[Hunk],
    group: &Group,
    side: Side,
    top: bool,
    out: bool,
) -> Option<(u32, bool)> {
    let (first, len) = side.range(&group.header);
    if out {
        let n = if top {
            first.checked_sub(1)?
        } else {
            first + len
        };
        (n > 0).then_some((n, true))
    } else {
        let edge = if top {
            group.lines.first()
        } else {
            group.lines.last()
        }?;
        let line = view.line(hunks, *edge)?;
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

/// What a line of the file is, as far as we know.
enum What {
    Diff(usize, usize),
    Text(DiffLine),
    /// The texts don't tell us.
    Missing,
}

/// A line of the file in order, with whether the pane shows it.
struct Item {
    what: What,
    change: bool,
    shown: bool,
    /// The numbers of the next old and new line at this point.
    at: (u32, u32),
}

/// Work out what the pane shows of a file whose hunks are `hunks`, with the
/// unchanged lines in `choices` shown or hidden. Without choices, this is
/// `View::plain`.
///
/// Each of git's hunks stays whole. The context lines between its first and
/// last change always show up, and so do its context lines next to a
/// change, up to the first one the user hid. A context line further out
/// that the user chose to show is treated like a line from the texts.
///
/// A line between git's hunks can only be shown if both texts have it,
/// with the same text and line ending. The texts are read separately from
/// the diff, so the file may have changed in between.
pub fn view(hunks: &[Hunk], texts: &FileTexts, side: Side, choices: &Choices) -> View {
    if choices.is_empty() {
        return View::plain(hunks);
    }
    let old = texts.old.as_deref().map(Text::new);
    let new = texts.new.as_deref().map(Text::new);
    let unchanged = |o: u32, n: u32| -> Option<DiffLine> {
        let (a, a_no_eol) = old.as_ref()?.line(o)?;
        let (b, b_no_eol) = new.as_ref()?.line(n)?;
        if a != b || a_no_eol != b_no_eol {
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
        let (what, shown) = match unchanged(o, n) {
            Some(line) => (What::Text(line), choice(o, n) == Some(true)),
            None => (What::Missing, false),
        };
        items.push(Item {
            what,
            change: false,
            shown,
            at: (o, n),
        });
    };

    let mut items = Vec::new();
    // Where each of git's hunks starts in `items`.
    let mut starts = Vec::new();
    let (mut o, mut n) = (1, 1);
    for (hi, h) in hunks.iter().enumerate() {
        let first_o = first_line(h.old_start, h.old_len);
        let first_n = first_line(h.new_start, h.new_len);
        if first_o < o || first_n < n || first_o - o != first_n - n {
            // Not a diff we understand.
            return View::plain(hunks);
        }
        while o < first_o {
            gap(&mut items, o, n);
            o += 1;
            n += 1;
        }
        starts.push(items.len());
        for (li, l) in h.lines.iter().enumerate() {
            let change = l.kind != LineKind::Context;
            items.push(Item {
                what: What::Diff(hi, li),
                change,
                shown: change || choice(o, n) != Some(false),
                at: (o, n),
            });
            o += u32::from(l.kind != LineKind::Add);
            n += u32::from(l.kind != LineKind::Del);
        }
    }
    starts.push(items.len());
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

    // Keep each of git's hunks whole, and only keep the context lines that
    // reach its changes as part of it.
    for (hi, h) in hunks.iter().enumerate() {
        let (start, end) = (starts[hi], starts[hi + 1]);
        let part = &mut items[start..end];
        let (top, bottom) = match part.iter().position(|i| i.change) {
            Some(first) => {
                let last = part.iter().rposition(|i| i.change).map_or(first, |l| l + 1);
                for item in &mut part[first..last] {
                    item.shown = true;
                }
                let top = part[..first]
                    .iter()
                    .rposition(|i| !i.shown)
                    .map_or(0, |i| i + 1);
                let bottom = part[last..]
                    .iter()
                    .position(|i| !i.shown)
                    .map_or(part.len(), |i| i + last);
                (top, bottom)
            }
            // git always puts a change in a hunk, but if one has none,
            // none of its lines belong to it.
            None => (part.len(), part.len()),
        };
        let (above, rest) = part.split_at_mut(top);
        let below = &mut rest[bottom.saturating_sub(top)..];
        for item in above.iter_mut().chain(below) {
            if let What::Diff(_, l) = item.what {
                item.what = What::Text(h.lines[l].clone());
            }
        }
    }

    let mut view = View::default();
    for run in items.split(|i| !i.shown) {
        // Shown lines that don't reach a change, which can happen when the
        // file changed since the user picked them.
        if !run.iter().any(|i| i.change) {
            continue;
        }
        let mut lines = Vec::with_capacity(run.len());
        let mut kinds = Vec::with_capacity(run.len());
        let mut first_hunk = None;
        let mut last_hunk = 0;
        for item in run {
            match &item.what {
                &What::Diff(h, l) => {
                    first_hunk.get_or_insert(h);
                    last_hunk = h;
                    lines.push(Shown::Diff(h, l));
                    kinds.push(hunks[h].lines[l].kind);
                }
                What::Text(line) => {
                    lines.push(Shown::Extra(view.extra.len()));
                    kinds.push(LineKind::Context);
                    view.extra.push(line.clone());
                }
                What::Missing => {}
            }
        }
        let Some(first_hunk) = first_hunk else {
            continue;
        };
        let old_len = kinds.iter().filter(|&&k| k != LineKind::Add).count() as u32;
        let new_len = kinds.iter().filter(|&&k| k != LineKind::Del).count() as u32;
        let (o, n) = run[0].at;
        view.groups.push(Group {
            hunks: first_hunk..last_hunk + 1,
            header: Hunk {
                old_start: if old_len == 0 { o - 1 } else { o },
                old_len,
                new_start: if new_len == 0 { n - 1 } else { n },
                new_len,
                section: hunks[first_hunk].section.clone(),
                lines: Vec::new(),
            },
            lines,
        });
    }
    view
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

    /// The lines of each group, with the group's header.
    fn groups(view: &View, hunks: &[Hunk]) -> Vec<Hunk> {
        view.groups
            .iter()
            .map(|g| Hunk {
                lines: g
                    .lines
                    .iter()
                    .filter_map(|&s| view.line(hunks, s).cloned())
                    .collect(),
                ..g.header.clone()
            })
            .collect()
    }

    fn render(view: &View, hunks: &[Hunk]) -> String {
        let mut s = String::new();
        for h in groups(view, hunks) {
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
    fn hunks_grow_hide_and_join() {
        let hunks = diff_texts(OLD.as_bytes(), NEW.as_bytes(), 1);
        let t = texts(OLD, NEW);
        insta::assert_snapshot!(render(&View::plain(&hunks), &hunks), @r"
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
        let v = view(&hunks, &t, Side::New, &c);
        insta::assert_snapshot!(render(&v, &hunks), @r"
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
        assert_eq!(v.groups[0].lines[0], Shown::Extra(0));
        assert_eq!(v.groups[1].hunks, 1..2);
        // Showing the lines between them makes one group.
        let c = choices(&[(5, true), (6, true), (7, true)]);
        let v = view(&hunks, &t, Side::New, &c);
        insta::assert_snapshot!(render(&v, &hunks), @r"
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
        assert_eq!(v.groups[0].hunks, 0..2);
        assert_eq!(v.extra.len(), 3);
        // A line that the texts don't have stays hidden.
        let partial = FileTexts {
            old: Some(Arc::from("1\n2\n3\n4\n5\nsix\n7\n8\n9\n10\n")),
            new: t.new,
        };
        insta::assert_snapshot!(render(&view(&hunks, &partial, Side::New, &c), &hunks), @r"
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
        let shown = view(&hunks, &t, Side::New, &choices(&[(3, true)]));
        insta::assert_snapshot!(render(&shown, &hunks), @r"
        @@ -3 +3,2 @@
         c
        +d
        \ No newline at end of file
        ");
        let hidden = view(&hunks, &t, Side::New, &choices(&[(3, false)]));
        assert_eq!(hidden, View::plain(&hunks));
        assert_eq!(hidden.groups[0].header.old_start, 3);
        assert_eq!(hidden.groups[0].header.old_len, 0);
    }

    #[test]
    fn context_inside_a_hunk_stays() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "one\n2\n3\n4\nfive\n";
        let hunks = diff_texts(old.as_bytes(), new.as_bytes(), 3);
        assert_eq!(hunks.len(), 1);
        let t = texts(old, new);
        let v = view(&hunks, &t, Side::New, &choices(&[(3, false)]));
        assert_eq!(v, View::plain(&hunks));
    }

    #[test]
    fn context_cut_off_from_its_hunk_can_join_another() {
        // Hiding line 9 cuts line 10 off from the first hunk. Showing line
        // 11 joins line 10 to the second hunk instead.
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old
            .replace("\n8\n", "\neight\n")
            .replace("\n14\n", "\nfourteen\n");
        let hunks = diff_texts(old.as_bytes(), new.as_bytes(), 2);
        assert_eq!(hunks.len(), 2);
        let t = texts(&old, &new);
        let v = view(&hunks, &t, Side::New, &choices(&[(9, false), (11, true)]));
        insta::assert_snapshot!(render(&v, &hunks), @r"
        @@ -6,3 +6,3 @@
         6
         7
        -8
        +eight
        @@ -10,7 +10,7 @@
         10
         11
         12
         13
        -14
        +fourteen
         15
         16
        ");
        assert_eq!(v.groups[1].hunks, 1..2);
        assert_eq!(v.groups[1].lines[0], Shown::Extra(0));
    }

    #[test]
    fn edges() {
        let hunks = diff_texts(OLD.as_bytes(), NEW.as_bytes(), 1);
        let v = View::plain(&hunks);
        let (a, b) = (&v.groups[0], &v.groups[1]);
        let edge = |g, top, out| edge_line(&v, &hunks, g, Side::New, top, out);
        assert_eq!(edge(a, true, true), Some((1, true)));
        assert_eq!(edge(a, true, false), Some((2, false)));
        assert_eq!(edge(a, false, true), Some((5, true)));
        assert_eq!(edge(b, false, false), Some((10, false)));
        let top = diff_texts(b"1\n", b"one\n", 0);
        let v = View::plain(&top);
        let g = &v.groups[0];
        assert_eq!(edge_line(&v, &top, g, Side::Old, true, true), None);
        assert_eq!(edge_line(&v, &top, g, Side::Old, true, false), None);
    }

    fn text() -> impl Strategy<Value = String> {
        let line = prop::sample::select(vec!["a", "b", "c", "", "x\r", "\u{FFFD}"]);
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
        fn views_keep_git_hunks_whole(
            old in text(),
            new in text(),
            context in 0u32..4,
            picks in prop::collection::vec((1u32..35, any::<bool>()), 0..20),
            side_old in any::<bool>(),
        ) {
            let hunks = diff_texts(old.as_bytes(), new.as_bytes(), context);
            let t = texts(&old, &new);
            let side = if side_old { Side::Old } else { Side::New };
            prop_assert_eq!(&view(&hunks, &t, side, &Choices::new()), &View::plain(&hunks));
            // Choices that agree with the diff change nothing.
            let same: Choices = hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == LineKind::Context)
                .filter_map(|l| side.number(l))
                .map(|n| (n, true))
                .collect();
            prop_assert_eq!(&view(&hunks, &t, side, &same), &View::plain(&hunks));

            let choices: Choices = picks.into_iter().collect();
            let v = view(&hunks, &t, side, &choices);
            let got = groups(&v, &hunks);
            prop_assert_eq!(apply(&old, &got), new);

            // The groups split git's hunks into runs, in order, and each
            // group's lines from git are in order.
            let mut next = 0;
            for g in &v.groups {
                prop_assert_eq!(g.hunks.start, next);
                prop_assert!(g.hunks.end > g.hunks.start);
                next = g.hunks.end;
                let diff: Vec<(usize, usize)> = g
                    .lines
                    .iter()
                    .filter_map(|s| match *s {
                        Shown::Diff(h, l) => Some((h, l)),
                        Shown::Extra(_) => None,
                    })
                    .collect();
                prop_assert!(diff.windows(2).all(|w| w[0] < w[1]));
                prop_assert!(diff.iter().all(|(h, _)| g.hunks.contains(h)));
                // Every changed line of its hunks shows up.
                for h in g.hunks.clone() {
                    for l in hunks[h].changed_lines() {
                        prop_assert!(diff.contains(&(h, l)));
                    }
                }
            }
            prop_assert_eq!(next, hunks.len());
            for h in 0..hunks.len() {
                prop_assert_eq!(v.group_of(h).map(|g| g.hunks.contains(&h)), Some(true));
            }

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
                }
                prop_assert!(h.lines.iter().any(|l| l.kind != LineKind::Context));
            }
            // A context line that the user hid only shows up between two
            // changes of one of git's hunks.
            for g in &v.groups {
                let kinds: Vec<LineKind> = g
                    .lines
                    .iter()
                    .filter_map(|&s| v.line(&hunks, s).map(|l| l.kind))
                    .collect();
                let first = kinds.iter().position(|&k| k != LineKind::Context);
                let last = kinds.iter().rposition(|&k| k != LineKind::Context);
                for (i, &s) in g.lines.iter().enumerate() {
                    let l = v.line(&hunks, s).expect("line");
                    let hidden = side.number(l).and_then(|k| choices.get(&k)) == Some(&false);
                    if l.kind == LineKind::Context && hidden {
                        prop_assert!(matches!(s, Shown::Diff(..)));
                        prop_assert!(first < Some(i) && Some(i) < last);
                    }
                }
            }
            // Groups never touch, or they would be one.
            let first = |h: &Hunk| if h.old_len == 0 { h.old_start } else { h.old_start - 1 };
            for w in got.windows(2) {
                prop_assert!(first(&w[1]) > first(&w[0]) + w[0].old_len, "groups touch");
            }
        }
    }
}
