//! Moving the edges of hunks in the detail pane, which shows more or fewer
//! unchanged lines around them. `git::context` works out what the pane
//! shows. This file keeps the user's choices and handles the keys and the
//! mouse. The diffs themselves stay as git gave them, so staging works on
//! git's hunks.

use super::doc::{Section, section_files, section_texts};
use super::{App, Drag, Level};
use crate::git::context::{self, Side, View};

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

impl App {
    /// Forget the choices for files that left their section. A change that
    /// comes back later starts out as git shows it.
    pub(super) fn prune_shown_lines(&mut self) {
        let workdir = self.workdir.as_ref();
        let commit = self.detail.commit.as_deref();
        self.detail.shown_lines.retain(|(sec, path), _| {
            let loaded = match sec {
                Section::Commit => commit.is_some(),
                _ => workdir.is_some(),
            };
            !loaded
                || section_files(workdir, commit, *sec)
                    .iter()
                    .any(|f| f.path() == path)
        });
    }

    /// Drop the syntax colors of file `i` in `sec`. The colors of the
    /// lines that the texts gave are kept by their place in the file's
    /// view, so they would land on other lines.
    fn forget_highlight(&mut self, sec: Section, i: usize) {
        let key = (sec as u8, i);
        let done = self.detail.highlights.remove(&key).is_some();
        if !done && self.detail.requested.contains(&key) {
            // A job for the old view may still be running, and its result
            // would look current.
            self.detail.reset_highlights();
        }
        self.detail.requested.remove(&key);
    }

    /// Move one edge of the hunk that the pane shows with git's hunk `hunk`
    /// of file `i` in it, by one line. `top` picks the edge, and `out`
    /// moves it away from the middle of the hunk. Returns `None` if the
    /// pane would show the same thing. Otherwise it says whether the hunk
    /// joined another one. The caller rebuilds the pane.
    pub fn move_hunk_edge(
        &mut self,
        sec: Section,
        i: usize,
        hunk: usize,
        top: bool,
        out: bool,
    ) -> Option<bool> {
        let side = side_of(sec)?;
        let file = self.section_files(sec).get(i)?.clone();
        let texts = section_texts(self.workdir.as_ref(), self.detail.commit.as_deref(), sec)
            .and_then(|t| t.get(i))
            .cloned()
            .unwrap_or_default();
        let key = (sec, file.path().to_owned());
        let mut choices = self
            .detail
            .shown_lines
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let before = context::view(&file.hunks, &texts, side, &choices);
        let group = before.group_of(hunk)?;
        let (n, show) = context::edge_line(&before, &file.hunks, group, side, top, out)?;
        choices.insert(n, show);
        let after = context::view(&file.hunks, &texts, side, &choices);
        // The line isn't there, or the texts don't have it.
        if after == before {
            return None;
        }
        let joined = after.group_of(hunk).map(|g| &g.hunks) != Some(&group.hunks);
        if after == View::plain(&file.hunks) {
            self.detail.shown_lines.remove(&key);
        } else {
            self.detail.shown_lines.insert(key, choices);
        }
        self.forget_highlight(sec, i);
        Some(joined)
    }

    /// Move an edge of the hunk at the cursor, for the keys.
    pub fn move_edge_at_cursor(&mut self, top: bool, out: bool) {
        let Some((sec, i, hunk)) = self.detail.hunk_at(self.detail.cursor) else {
            return;
        };
        if self.move_hunk_edge(sec, i, hunk, top, out).is_none() {
            let msg = match (out, top) {
                (true, true) => "There are no more lines above this hunk",
                (true, false) => "There are no more lines below this hunk",
                (false, true) => "There are no context lines left at the top of this hunk",
                (false, false) => "There are no context lines left at the bottom of this hunk",
            };
            self.set_status(msg, Level::Info);
            return;
        }
        self.detail.anchor = None;
        self.rebuild_doc();
        self.ensure_doc_visible();
    }

    /// Show the hunks of the file at the cursor as git gives them.
    pub fn reset_hunk_edges(&mut self) {
        let Some((sec, i)) = self
            .detail
            .rows
            .get(self.detail.cursor)
            .and_then(|r| r.file())
        else {
            return;
        };
        let Some(path) = self.section_files(sec).get(i).map(|f| f.path().to_owned()) else {
            return;
        };
        if self.detail.shown_lines.remove(&(sec, path)).is_none() {
            return;
        }
        self.forget_highlight(sec, i);
        self.detail.anchor = None;
        self.rebuild_doc();
        self.ensure_doc_visible();
    }

    /// Whether the user moved the edges of any hunk of the file at the
    /// cursor.
    pub fn hunk_edges_moved(&self) -> bool {
        let Some((sec, i)) = self
            .detail
            .rows
            .get(self.detail.cursor)
            .and_then(|r| r.file())
        else {
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
        let top = match self.detail.rows.get(row) {
            Some(super::doc::Row::Hunk { .. }) => true,
            Some(super::doc::Row::HunkEnd { .. }) => false,
            _ => return,
        };
        let Some((sec, i, hunk)) = self.detail.hunk_at(row) else {
            return;
        };
        if side_of(sec).is_none() {
            return;
        }
        let Some(file) = self.section_files(sec).get(i) else {
            return;
        };
        self.drag = Some(Drag::HunkEdge {
            sec,
            path: file.path().to_owned(),
            hunk,
            top,
            y,
        });
    }

    /// Follow the mouse while it drags an edge of a hunk. Each row the
    /// mouse moves shows or hides one line. When the top edge moves, the
    /// pane scrolls along, so the hunk's lines stay where they are on the
    /// screen and the header follows the mouse. The drag ends when the
    /// hunk joins another one, because the edge it held is gone.
    pub(super) fn drag_edge(&mut self, y: u16) {
        let Some(Drag::HunkEdge {
            sec,
            path,
            hunk,
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
        let Some(i) = self
            .section_files(sec)
            .iter()
            .position(|f| f.path() == path)
        else {
            self.drag = None;
            return;
        };
        let mut moved = 0;
        let mut joined = false;
        for _ in 0..steps.unsigned_abs() {
            let Some(j) = self.move_hunk_edge(sec, i, hunk, top, out) else {
                break;
            };
            moved += 1;
            if top {
                self.detail.scroll = if out {
                    self.detail.scroll + 1
                } else {
                    self.detail.scroll.saturating_sub(1)
                };
            }
            if j {
                joined = true;
                break;
            }
        }
        if moved == 0 {
            return;
        }
        self.drag = if joined {
            None
        } else {
            Some(Drag::HunkEdge {
                sec,
                path,
                hunk,
                top,
                y,
            })
        };
        self.detail.anchor = None;
        self.rebuild_doc();
    }
}
