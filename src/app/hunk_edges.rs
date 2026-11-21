//! Moving the edges of hunks in the detail pane, which shows more or fewer
//! context lines around them. `git::context` builds the hunks. This file
//! keeps the user's choices, puts the hunks built from them in place of
//! git's, and handles the keys and the mouse.

use std::sync::Arc;

use gix::bstr::BString;

use super::doc::{Row, Section, section_texts};
use super::{App, Drag, Level};
use crate::git::context::{self, Side, regroup};
use crate::git::diff::{DiffLine, FileDiff, LineKind};

/// A line of a diff, by its kind and its numbers. It names the same line
/// before and after the hunks are built again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineId {
    kind: LineKind,
    old_no: Option<u32>,
    new_no: Option<u32>,
}

impl LineId {
    fn of(l: &DiffLine) -> Self {
        LineId {
            kind: l.kind,
            old_no: l.old_no,
            new_no: l.new_no,
        }
    }

    fn is(&self, l: &DiffLine) -> bool {
        *self == LineId::of(l)
    }
}

/// The side whose line numbers name the unchanged lines of files in `sec`.
/// Staging changes the new side of a staged diff and the old side of an
/// unstaged one, so each section uses the other side. Untracked and
/// conflicted files show every line as added, so they have no context.
pub fn side_of(sec: Section) -> Option<Side> {
    match sec {
        Section::Staged => Some(Side::Old),
        Section::Unstaged | Section::Commit => Some(Side::New),
        Section::Untracked | Section::Conflicted => None,
    }
}

/// The first changed line of a hunk, which stays in the same hunk however
/// its edges move.
fn first_change(file: &FileDiff, hunk: usize) -> Option<LineId> {
    file.hunks
        .get(hunk)?
        .lines
        .iter()
        .find(|l| l.kind != LineKind::Context)
        .map(LineId::of)
}

fn hunk_with(file: &FileDiff, line: LineId) -> Option<usize> {
    file.hunks
        .iter()
        .position(|h| h.lines.iter().any(|l| line.is(l)))
}

/// Where the cursor was in a hunk, so it can be put back after the hunk is
/// built again.
#[derive(Clone, Copy)]
enum Spot {
    Header,
    End,
    Line(LineId),
}

impl App {
    fn files_mut(&mut self, sec: Section) -> Option<&mut Vec<Arc<FileDiff>>> {
        Some(match sec {
            Section::Commit => &mut Arc::make_mut(self.detail.commit.as_mut()?).files,
            Section::Conflicted => &mut self.workdir.as_mut()?.conflicted,
            Section::Untracked => &mut self.workdir.as_mut()?.untracked,
            Section::Unstaged => &mut self.workdir.as_mut()?.unstaged,
            Section::Staged => &mut self.workdir.as_mut()?.staged,
        })
    }

    /// Put the hunks built from the user's choices in place of the ones git
    /// gave. Each rebuild of the pane does this first, so that a file that
    /// just reloaded gets them again.
    pub(super) fn apply_shown_lines(&mut self) {
        let keys: Vec<(Section, BString)> = self.detail.shown_lines.keys().cloned().collect();
        for key in keys {
            let (sec, path) = &key;
            let Some(i) = self
                .section_files(*sec)
                .iter()
                .position(|f| f.path() == path)
            else {
                // The file left its section, so its choices go too. A
                // change that comes back later starts out as git shows it.
                let loaded = match sec {
                    Section::Commit => self.detail.commit.is_some(),
                    _ => self.workdir.is_some(),
                };
                if loaded {
                    self.detail.shown_lines.remove(&key);
                }
                continue;
            };
            let file = self.section_files(*sec)[i].clone();
            let current = self.detail.shown_lines.get(&key).is_some_and(|s| {
                s.built
                    .as_ref()
                    .is_some_and(|(_, view)| Arc::ptr_eq(view, &file))
            });
            if !current {
                self.build_hunks(*sec, i, file);
            }
        }
    }

    /// Build the hunks of file `i` in `sec` from `base`, the file as git
    /// gave it, and put the result in the pane's data. Returns the file the
    /// pane shows now.
    fn build_hunks(
        &mut self,
        sec: Section,
        i: usize,
        base: Arc<FileDiff>,
    ) -> Option<Arc<FileDiff>> {
        let side = side_of(sec)?;
        let texts = section_texts(self.workdir.as_ref(), self.detail.commit.as_deref(), sec)
            .and_then(|t| t.get(i))
            .cloned()
            .unwrap_or_default();
        let shown = self
            .detail
            .shown_lines
            .get_mut(&(sec, base.path().to_owned()))?;
        let hunks = regroup(&base.hunks, &texts, side, &shown.choices);
        let view = if hunks == base.hunks {
            base.clone()
        } else {
            let mut f = (*base).clone();
            f.hunks = hunks;
            Arc::new(f)
        };
        shown.built = Some((base, view.clone()));
        *self.files_mut(sec)?.get_mut(i)? = view.clone();
        Some(view)
    }

    /// Drop the word marks and syntax colors of a file whose hunks changed.
    /// Both are kept by hunk and line, so they would land on other lines.
    fn forget_colors(&mut self, sec: Section, i: usize) {
        self.detail.marks.remove(&(sec, i));
        let key = (sec as u8, i);
        let done = self.detail.highlights.remove(&key).is_some();
        if !done && self.detail.requested.contains(&key) {
            // A job for the old hunks may still be running, and its result
            // would look current.
            self.detail.reset_highlights();
        }
        self.detail.requested.remove(&key);
    }

    /// Move one edge of hunk `hunk` of file `i` in `sec` by one line.
    /// `top` picks the edge, and `out` moves it away from the middle of the
    /// hunk. Returns whether anything changed. The caller rebuilds the
    /// pane.
    pub fn move_hunk_edge(
        &mut self,
        sec: Section,
        i: usize,
        hunk: usize,
        top: bool,
        out: bool,
    ) -> bool {
        let Some(side) = side_of(sec) else {
            return false;
        };
        let Some(file) = self.section_files(sec).get(i).cloned() else {
            return false;
        };
        let Some((n, show)) = file
            .hunks
            .get(hunk)
            .and_then(|h| context::edge_line(h, side, top, out))
        else {
            return false;
        };
        let key = (sec, file.path().to_owned());
        let shown = self.detail.shown_lines.entry(key.clone()).or_default();
        let base = match &shown.built {
            Some((base, view)) if Arc::ptr_eq(view, &file) => base.clone(),
            _ => file.clone(),
        };
        let before = shown.choices.insert(n, show);
        let view = self.build_hunks(sec, i, base);
        if view.is_some_and(|v| v.hunks != file.hunks) {
            self.forget_colors(sec, i);
            return true;
        }
        // The line isn't there, or the texts don't have it. Leave the
        // choices as they were.
        if let Some(shown) = self.detail.shown_lines.get_mut(&key) {
            match before {
                Some(b) => shown.choices.insert(n, b),
                None => shown.choices.remove(&n),
            };
            if shown.choices.is_empty() {
                self.detail.shown_lines.remove(&key);
                if let Some(f) = self.files_mut(sec).and_then(|f| f.get_mut(i)) {
                    *f = file;
                }
            }
        }
        false
    }

    /// Where the cursor is in its hunk, if it's in one: the section, file
    /// and hunk, the hunk's first changed line and the cursor's place.
    fn cursor_spot(&self) -> Option<(Section, usize, usize, LineId, Spot)> {
        let row = self.detail.rows.get(self.detail.cursor)?;
        let (sec, i, hunk) = row.hunk()?;
        let file = self.section_files(sec).get(i)?;
        let change = first_change(file, hunk)?;
        let line = |l: Option<usize>| {
            let l = file.hunks.get(hunk)?.lines.get(l?)?;
            Some(Spot::Line(LineId::of(l)))
        };
        let spot = match *row {
            Row::HunkEnd { .. } => Spot::End,
            Row::Line { line: l, .. } => line(Some(l))?,
            Row::Split { left, right, .. } => line(right.or(left))?,
            _ => Spot::Header,
        };
        Some((sec, i, hunk, change, spot))
    }

    /// Put the cursor back on its place in the hunk that holds `change`,
    /// after a rebuild. If its line went away, it goes to the hunk's
    /// header.
    fn restore_spot(&mut self, sec: Section, path: &BString, change: LineId, spot: Spot) {
        let Some((i, file)) = self
            .section_files(sec)
            .iter()
            .enumerate()
            .find(|(_, f)| f.path() == path)
        else {
            return;
        };
        let Some(hunk) = hunk_with(file, change) else {
            return;
        };
        let lines = &file.hunks[hunk].lines;
        let in_hunk = |r: &Row| r.hunk() == Some((sec, i, hunk));
        let found = self.detail.rows.iter().position(|r| {
            in_hunk(r)
                && match (&spot, r) {
                    (Spot::Header, Row::Hunk { .. }) | (Spot::End, Row::HunkEnd { .. }) => true,
                    (Spot::Line(id), Row::Line { line, .. }) => {
                        lines.get(*line).is_some_and(|l| id.is(l))
                    }
                    (Spot::Line(id), Row::Split { left, right, .. }) => [left, right]
                        .into_iter()
                        .flatten()
                        .any(|&l| lines.get(l).is_some_and(|l| id.is(l))),
                    _ => false,
                }
        });
        let header = || {
            self.detail
                .rows
                .iter()
                .position(|r| in_hunk(r) && matches!(r, Row::Hunk { .. }))
        };
        if let Some(row) = found.or_else(header) {
            self.detail.cursor = row;
        }
    }

    /// Move an edge of the hunk at the cursor, for the keys.
    pub fn move_edge_at_cursor(&mut self, top: bool, out: bool) {
        let Some((sec, i, hunk, change, spot)) = self.cursor_spot() else {
            return;
        };
        if !self.move_hunk_edge(sec, i, hunk, top, out) {
            let msg = match (out, top) {
                (true, true) => "There are no more lines above this hunk",
                (true, false) => "There are no more lines below this hunk",
                (false, true) => "There are no context lines left at the top of this hunk",
                (false, false) => "There are no context lines left at the bottom of this hunk",
            };
            self.set_status(msg, Level::Info);
            return;
        }
        let path = self.section_files(sec)[i].path().to_owned();
        self.detail.anchor = None;
        self.rebuild_doc();
        self.restore_spot(sec, &path, change, spot);
        self.ensure_doc_visible();
    }

    /// Show the hunks of the file at the cursor as git gives them.
    pub fn reset_hunk_edges(&mut self) {
        let Some((sec, i)) = self.detail.rows.get(self.detail.cursor).and_then(Row::file) else {
            return;
        };
        let spot = self.cursor_spot();
        let Some(file) = self.section_files(sec).get(i).cloned() else {
            return;
        };
        let path = file.path().to_owned();
        let Some(shown) = self.detail.shown_lines.remove(&(sec, path.clone())) else {
            return;
        };
        if let Some((base, view)) = shown.built
            && Arc::ptr_eq(&view, &file)
            && let Some(f) = self.files_mut(sec).and_then(|f| f.get_mut(i))
        {
            *f = base;
        }
        self.forget_colors(sec, i);
        self.detail.anchor = None;
        self.rebuild_doc();
        if let Some((sec, _, _, change, spot)) = spot {
            self.restore_spot(sec, &path, change, spot);
        }
        self.ensure_doc_visible();
    }

    /// Whether the user moved the edges of any hunk of the file at the
    /// cursor.
    pub fn hunk_edges_moved(&self) -> bool {
        let Some((sec, i)) = self.detail.rows.get(self.detail.cursor).and_then(Row::file) else {
            return false;
        };
        self.section_files(sec).get(i).is_some_and(|f| {
            self.detail
                .shown_lines
                .contains_key(&(sec, f.path().to_owned()))
        })
    }

    /// Start dragging an edge of the hunk on row `row`, if it can move.
    pub(super) fn start_edge_drag(&mut self, row: usize, y: u16) {
        let Some(r) = self.detail.rows.get(row) else {
            return;
        };
        let top = match r {
            Row::Hunk { .. } => true,
            Row::HunkEnd { .. } => false,
            _ => return,
        };
        let Some((sec, i, hunk)) = r.hunk() else {
            return;
        };
        if side_of(sec).is_none() {
            return;
        }
        let Some(file) = self.section_files(sec).get(i) else {
            return;
        };
        let Some(change) = first_change(file, hunk) else {
            return;
        };
        self.drag = Some(Drag::HunkEdge {
            sec,
            path: file.path().to_owned(),
            change,
            top,
            y,
        });
    }

    /// Follow the mouse while it drags an edge of a hunk. Each row the
    /// mouse moves shows or hides one line. When the top edge moves, the
    /// pane scrolls along, so the hunk's lines stay where they are on the
    /// screen and the header follows the mouse. The drag ends when the
    /// hunk meets another one, because the edge it held is gone.
    pub(super) fn drag_edge(&mut self, y: u16) {
        let Some(Drag::HunkEdge {
            sec,
            path,
            change,
            top,
            y: from,
        }) = self.drag.clone()
        else {
            return;
        };
        let steps = i32::from(y) - i32::from(from);
        if steps == 0 {
            return;
        }
        let out = if top { steps < 0 } else { steps > 0 };
        let Some((i, file)) = self
            .section_files(sec)
            .iter()
            .enumerate()
            .find(|(_, f)| f.path() == path)
            .map(|(i, f)| (i, f.clone()))
        else {
            self.drag = None;
            return;
        };
        let Some(hunk) = hunk_with(&file, change) else {
            self.drag = None;
            return;
        };
        let count = file.hunks.len();
        let mut moved = 0;
        let mut merged = false;
        for _ in 0..steps.unsigned_abs() {
            if !self.move_hunk_edge(sec, i, hunk, top, out) {
                break;
            }
            moved += 1;
            if top {
                self.detail.scroll = if out {
                    self.detail.scroll + 1
                } else {
                    self.detail.scroll.saturating_sub(1)
                };
            }
            if self.section_files(sec).get(i).map(|f| f.hunks.len()) != Some(count) {
                merged = true;
                break;
            }
        }
        if moved == 0 {
            return;
        }
        self.drag = if merged {
            None
        } else {
            Some(Drag::HunkEdge {
                sec,
                path: path.clone(),
                change,
                top,
                y,
            })
        };
        self.detail.anchor = None;
        self.rebuild_doc();
        let spot = if top { Spot::Header } else { Spot::End };
        self.restore_spot(sec, &path, change, spot);
    }
}
