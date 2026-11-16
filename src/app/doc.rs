//! The detail pane: a commit's header and diff, or the staging view, laid
//! out as a flat list of rows so it can be scrolled and drawn quickly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gix::ObjectId;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::cmd::{Cmd, Unit};
use crate::config::Config;
use crate::git::commit::{CommitDetail, FileTexts};
use crate::git::diff::{FileDiff, LineKind};
use crate::git::refs::{Label, LabelKind};
use crate::git::words::{Marks, hunk_marks};
use crate::git::workdir::Workdir;
use crate::highlight::FileHighlight;
use crate::theme::Theme;
use crate::util::{Generation, format_date};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    None,
    Workdir,
    Commit(ObjectId),
    Stash(ObjectId),
}

impl Source {
    /// The commit this source shows, if it shows one.
    pub fn commit_id(&self) -> Option<ObjectId> {
        match self {
            Source::Commit(id) | Source::Stash(id) => Some(*id),
            Source::None | Source::Workdir => None,
        }
    }
}

/// A line selection: the section, the file index, and `(hunk, line)` pairs.
pub type LineSelection = (Section, usize, Vec<(usize, usize)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Commit = 0,
    Conflicted = 1,
    Untracked = 2,
    Unstaged = 3,
    Staged = 4,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Commit => "Files",
            Section::Conflicted => "Merge conflicts",
            Section::Untracked => "Untracked files",
            Section::Unstaged => "Unstaged changes",
            Section::Staged => "Staged changes",
        }
    }

    pub fn from_u8(n: u8) -> Section {
        match n {
            1 => Section::Conflicted,
            2 => Section::Untracked,
            3 => Section::Unstaged,
            4 => Section::Staged,
            _ => Section::Commit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Meta(usize),
    Blank,
    Section(Section),
    File {
        sec: Section,
        file: usize,
    },
    Notice {
        sec: Section,
        file: usize,
        text: String,
    },
    Hunk {
        sec: Section,
        file: usize,
        hunk: usize,
    },
    Line {
        sec: Section,
        file: usize,
        hunk: usize,
        line: usize,
    },
    Split {
        sec: Section,
        file: usize,
        hunk: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
    Empty(String),
}

impl Row {
    pub fn file(&self) -> Option<(Section, usize)> {
        match *self {
            Row::File { sec, file }
            | Row::Notice { sec, file, .. }
            | Row::Hunk { sec, file, .. }
            | Row::Line { sec, file, .. }
            | Row::Split { sec, file, .. } => Some((sec, file)),
            _ => None,
        }
    }

    pub fn section(&self) -> Option<Section> {
        match *self {
            Row::Section(s) => Some(s),
            _ => self.file().map(|f| f.0),
        }
    }
}

pub struct MetaLine {
    pub line: Line<'static>,
    pub click: Option<Cmd>,
}

/// Identifies a row across rebuilds, so the cursor stays put.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKey {
    Meta(usize),
    Section(Section),
    File(Section, String),
    Notice(Section, String),
    Hunk(Section, String, usize),
    Line(Section, String, usize, usize),
    Other,
}

pub struct Detail {
    pub source: Source,
    pub commit: Option<Arc<CommitDetail>>,
    pub loading: bool,
    /// Moves on whenever the files shown change, so stale highlight results
    /// can be dropped. The highlighter shares it.
    pub generation: Generation,
    /// The id of the latest commit load request.
    pub request: u64,
    pub rows: Vec<Row>,
    pub meta: Vec<MetaLine>,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub scroll: usize,
    pub hscroll: usize,
    pub collapsed: HashSet<(Section, String)>,
    pub collapsed_sections: HashSet<Section>,
    seen: HashSet<(Section, String)>,
    pub highlights: HashMap<(u8, usize), Arc<FileHighlight>>,
    pub requested: HashSet<(u8, usize)>,
    pub marks: HashMap<(Section, usize), Arc<Vec<Marks>>>,
    /// Width of the line number columns.
    pub gutter: usize,
    /// File paths per section as of the last rebuild, for keeping the
    /// cursor in place.
    old_paths: HashMap<Section, Vec<String>>,
}

impl Default for Detail {
    fn default() -> Self {
        Detail {
            source: Source::None,
            commit: None,
            loading: false,
            generation: Generation::default(),
            request: 0,
            rows: Vec::new(),
            meta: Vec::new(),
            cursor: 0,
            anchor: None,
            scroll: 0,
            hscroll: 0,
            collapsed: HashSet::new(),
            collapsed_sections: HashSet::new(),
            seen: HashSet::new(),
            highlights: HashMap::new(),
            requested: HashSet::new(),
            marks: HashMap::new(),
            gutter: 4,
            old_paths: HashMap::new(),
        }
    }
}

pub fn section_files<'a>(
    workdir: Option<&'a Workdir>,
    commit: Option<&'a CommitDetail>,
    sec: Section,
) -> &'a [Arc<FileDiff>] {
    match sec {
        Section::Commit => commit.map_or(&[], |c| &c.files),
        Section::Conflicted => workdir.map_or(&[], |w| &w.conflicted),
        Section::Untracked => workdir.map_or(&[], |w| &w.untracked),
        Section::Unstaged => workdir.map_or(&[], |w| &w.unstaged),
        Section::Staged => workdir.map_or(&[], |w| &w.staged),
    }
}

pub fn section_texts<'a>(
    workdir: Option<&'a Workdir>,
    commit: Option<&'a CommitDetail>,
    sec: Section,
) -> Option<&'a [FileTexts]> {
    match sec {
        Section::Commit => commit.map(|c| c.texts.as_slice()),
        Section::Untracked => workdir.map(|w| w.untracked_texts.as_slice()),
        Section::Unstaged => workdir.map(|w| w.unstaged_texts.as_slice()),
        Section::Staged => workdir.map(|w| w.staged_texts.as_slice()),
        Section::Conflicted => None,
    }
}

impl Detail {
    pub fn set_source(&mut self, source: Source) {
        self.source = source;
        self.commit = None;
        self.cursor = 0;
        self.anchor = None;
        self.scroll = 0;
        self.hscroll = 0;
        // Folds are remembered by path. That's right for the working
        // directory, where the same files stay around. In commits, a file
        // that was folded in one commit would stay folded in every other
        // commit that touches it.
        self.collapsed.retain(|(sec, _)| *sec != Section::Commit);
        self.seen.retain(|(sec, _)| *sec != Section::Commit);
        self.reset_for_reload();
    }

    pub fn set_commit(&mut self, c: Arc<CommitDetail>) {
        self.commit = Some(c);
        self.reset_for_reload();
    }

    /// The files changed, so cached per-file data is no longer valid.
    pub fn reset_for_reload(&mut self) {
        self.reset_highlights();
        self.marks.clear();
        self.anchor = None;
    }

    /// Drop the syntax highlighting, so that drawing asks for it again.
    pub fn reset_highlights(&mut self) {
        self.generation.next();
        self.highlights.clear();
        self.requested.clear();
    }

    pub fn rebuild(
        &mut self,
        workdir: Option<&Workdir>,
        labels: &HashMap<ObjectId, Vec<Label>>,
        theme: &Theme,
        split: bool,
        cfg: &Config,
    ) {
        let commit = self.commit.clone();
        let paths: HashMap<Section, Vec<String>> = [
            Section::Commit,
            Section::Conflicted,
            Section::Untracked,
            Section::Unstaged,
            Section::Staged,
        ]
        .into_iter()
        .map(|sec| {
            let v = section_files(workdir, commit.as_deref(), sec)
                .iter()
                .map(|f| f.path().to_string())
                .collect();
            (sec, v)
        })
        .collect();
        // Rows refer to files by index, and the indexes may change when the
        // data reloads. So look up the cursor's file by its path as of the
        // last rebuild.
        let old_key = row_key(&self.rows, self.cursor, &self.old_paths);
        let old_cursor = self.cursor;
        let old_anchor = self.anchor.map(|a| {
            let key = row_key(&self.rows, a, &self.old_paths);
            (a, key, self.rows.get(a).cloned())
        });
        self.rows.clear();
        self.meta.clear();

        match &self.source {
            Source::None => {}
            Source::Commit(_) | Source::Stash(..) => match &commit {
                Some(c) => {
                    self.build_meta(c, labels, theme, cfg);
                    self.build_files(Section::Commit, &c.files, split, false);
                }
                None => self.rows.push(Row::Empty(if self.loading {
                    "Loading…".into()
                } else {
                    String::new()
                })),
            },
            Source::Workdir => match workdir {
                Some(w) => {
                    let mut any = false;
                    for (sec, files) in [
                        (Section::Conflicted, &w.conflicted),
                        (Section::Untracked, &w.untracked),
                        (Section::Unstaged, &w.unstaged),
                        (Section::Staged, &w.staged),
                    ] {
                        if files.is_empty() {
                            continue;
                        }
                        any = true;
                        self.rows.push(Row::Section(sec));
                        if !self.collapsed_sections.contains(&sec) {
                            self.build_files(sec, files, split, true);
                        }
                        self.rows.push(Row::Blank);
                    }
                    if !any {
                        self.rows.push(Row::Empty(
                            "Nothing to commit. The working directory is clean.".into(),
                        ));
                    }
                }
                None => self.rows.push(Row::Empty("Loading…".into())),
            },
        }

        let max_no = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Hunk { sec, file, hunk } => section_files(workdir, commit.as_deref(), *sec)
                    .get(*file)
                    .and_then(|f| f.hunks.get(*hunk))
                    .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        self.gutter = max_no.to_string().len().max(3);

        // Put the cursor back where it was.
        let find = |key: &RowKey, rows: &[Row]| -> Option<usize> {
            (0..rows.len()).find(|&i| {
                row_key(rows, i, &paths) == *key || left_key(rows, i, &paths).as_ref() == Some(key)
            })
        };
        // Blank and placeholder rows all share one key, so looking one up
        // would just find the first blank row.
        let mut pos = if old_key == RowKey::Other {
            None
        } else {
            find(&old_key, &self.rows)
        };
        if pos.is_none() {
            let fallback = match &old_key {
                RowKey::Line(s, p, h, _) => Some(RowKey::Hunk(*s, p.clone(), *h)),
                _ => None,
            };
            if let Some(k) = fallback {
                pos = find(&k, &self.rows);
                if pos.is_none()
                    && let RowKey::Hunk(s, p, h) = &k
                    && *h > 0
                {
                    pos = find(&RowKey::Hunk(*s, p.clone(), h - 1), &self.rows);
                }
            }
        }
        if pos.is_none()
            && let RowKey::Line(s, p, ..)
            | RowKey::Hunk(s, p, _)
            | RowKey::Notice(s, p)
            | RowKey::File(s, p) = &old_key
        {
            pos = find(&RowKey::File(*s, p.clone()), &self.rows);
        }
        // The other end of the line selection has to stay on its row too,
        // or the selection would cover lines the user didn't pick. If that
        // row is gone, drop the selection.
        self.anchor = match old_anchor {
            Some((a, RowKey::Other, row)) => Some(a).filter(|&a| self.rows.get(a) == row.as_ref()),
            Some((_, key, _)) => find(&key, &self.rows),
            None => None,
        };
        self.old_paths = paths;
        self.cursor = pos
            .unwrap_or(old_cursor)
            .min(self.rows.len().saturating_sub(1));
    }

    fn build_meta(
        &mut self,
        c: &CommitDetail,
        labels: &HashMap<ObjectId, Vec<Label>>,
        theme: &Theme,
        cfg: &Config,
    ) {
        let _ = cfg;
        let dim = Style::default().fg(theme.dim);
        let fg = Style::default().fg(theme.fg);
        let mut push = |line: Line<'static>, click: Option<Cmd>| {
            self.meta.push(MetaLine { line, click });
            self.rows.push(Row::Meta(self.meta.len() - 1));
        };
        let mut msg_lines = c.message.trim_end().lines();
        let subject = msg_lines.next().unwrap_or("").to_string();
        push(
            Line::from(Span::styled(subject, fg.add_modifier(Modifier::BOLD))),
            None,
        );
        let body: Vec<String> = msg_lines.map(str::to_string).collect();
        let body_start = body
            .iter()
            .position(|l| !l.trim().is_empty())
            .unwrap_or(body.len());
        if body_start < body.len() {
            push(Line::default(), None);
            for l in &body[body_start..] {
                push(Line::from(Span::styled(l.clone(), fg)), None);
            }
        }
        push(Line::default(), None);
        if let Some(ls) = labels.get(&c.id) {
            let mut spans = vec![Span::styled("Refs       ", dim)];
            for l in ls {
                let bg = match l.kind {
                    LabelKind::Head => theme.badge_head,
                    LabelKind::Local => theme.badge_local,
                    LabelKind::Remote => theme.badge_remote,
                    LabelKind::Tag => theme.badge_tag,
                };
                spans.push(Span::styled(
                    format!(" {} ", l.name),
                    Style::default().bg(bg).fg(theme.badge_fg),
                ));
                spans.push(Span::raw(" "));
            }
            push(Line::from(spans), None);
        }
        let person = |p: &crate::git::commit::Person| {
            format!(
                "{} <{}>  {}",
                p.name,
                p.email,
                format_date(p.time, p.offset)
            )
        };
        push(
            Line::from(vec![
                Span::styled("Author     ", dim),
                Span::styled(person(&c.author), fg),
            ]),
            None,
        );
        if c.committer.name != c.author.name
            || c.committer.email != c.author.email
            || c.committer.time != c.author.time
        {
            push(
                Line::from(vec![
                    Span::styled("Committer  ", dim),
                    Span::styled(person(&c.committer), fg),
                ]),
                None,
            );
        }
        push(
            Line::from(vec![
                Span::styled("Commit     ", dim),
                Span::styled(c.id.to_string(), Style::default().fg(theme.accent)),
            ]),
            Some(Cmd::CopyHash(c.id)),
        );
        for (i, p) in c.parents.iter().enumerate() {
            push(
                Line::from(vec![
                    Span::styled(if i == 0 { "Parents    " } else { "           " }, dim),
                    Span::styled(
                        p.to_string(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]),
                Some(Cmd::JumpTo(*p)),
            );
        }
        let (mut adds, mut dels) = (0, 0);
        for f in &c.files {
            let (a, d) = f.added_removed();
            adds += a;
            dels += d;
        }
        push(
            Line::from(vec![
                Span::styled("Changes    ", dim),
                Span::styled(
                    format!(
                        "{} file{}",
                        c.files.len(),
                        if c.files.len() == 1 { "" } else { "s" }
                    ),
                    fg,
                ),
                Span::raw("  "),
                Span::styled(format!("+{adds}"), Style::default().fg(theme.add_fg)),
                Span::raw(" "),
                Span::styled(format!("−{dels}"), Style::default().fg(theme.del_fg)),
            ]),
            None,
        );
        push(Line::default(), None);
    }

    fn build_files(&mut self, sec: Section, files: &[Arc<FileDiff>], split: bool, staging: bool) {
        let many = files.len() > 40;
        for (fi, f) in files.iter().enumerate() {
            let key = (sec, f.path().to_string());
            if self.seen.insert(key.clone()) && (f.line_count() > 400 || (many && !staging)) {
                self.collapsed.insert(key.clone());
            }
            self.rows.push(Row::File { sec, file: fi });
            if self.collapsed.contains(&key) {
                continue;
            }
            let notice = if f.binary {
                Some("Binary file".to_string())
            } else if f.too_large {
                Some("This diff is large. Press Enter here to show it anyway.".to_string())
            } else if f.hunks.is_empty() {
                match (f.old_mode.as_deref(), f.new_mode.as_deref()) {
                    (Some(a), Some(b)) if a != b => Some(format!("Mode changed from {a} to {b}")),
                    _ if f.status == crate::git::diff::FileStatus::Renamed => {
                        Some("Renamed without changes".to_string())
                    }
                    _ => Some("No content changes".to_string()),
                }
            } else {
                None
            };
            if let Some(text) = notice {
                self.rows.push(Row::Notice {
                    sec,
                    file: fi,
                    text,
                });
                continue;
            }
            self.marks
                .entry((sec, fi))
                .or_insert_with(|| Arc::new(f.hunks.iter().map(hunk_marks).collect()));
            for (hi, h) in f.hunks.iter().enumerate() {
                self.rows.push(Row::Hunk {
                    sec,
                    file: fi,
                    hunk: hi,
                });
                if split {
                    push_split_rows(&mut self.rows, sec, fi, hi, &h.lines);
                } else {
                    for li in 0..h.lines.len() {
                        self.rows.push(Row::Line {
                            sec,
                            file: fi,
                            hunk: hi,
                            line: li,
                        });
                    }
                }
            }
        }
    }

    /// Rows covered by the line selection, if there is one.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        Some((a.min(self.cursor), a.max(self.cursor)))
    }

    /// The changed lines in the selection that belong to the cursor's file.
    pub fn selected_lines<'a>(
        &self,
        files_of: impl Fn(Section) -> &'a [Arc<FileDiff>],
    ) -> Option<LineSelection> {
        let (lo, hi) = self.selection()?;
        let (sec, file) = self.rows.get(self.cursor)?.file()?;
        let f = files_of(sec).get(file)?;
        let changed = |h: usize, l: usize| {
            f.hunks
                .get(h)
                .and_then(|h| h.lines.get(l))
                .is_some_and(|l| l.kind != LineKind::Context)
        };
        let mut out = Vec::new();
        for row in self.rows.get(lo..=hi)? {
            match *row {
                Row::Line {
                    sec: s,
                    file: fl,
                    hunk,
                    line,
                } if s == sec && fl == file => {
                    if changed(hunk, line) {
                        out.push((hunk, line));
                    }
                }
                Row::Split {
                    sec: s,
                    file: fl,
                    hunk,
                    left,
                    right,
                } if s == sec && fl == file => {
                    for l in [left, right].into_iter().flatten() {
                        if changed(hunk, l) && !out.contains(&(hunk, l)) {
                            out.push((hunk, l));
                        }
                    }
                }
                _ => {}
            }
        }
        if out.is_empty() {
            None
        } else {
            Some((sec, file, out))
        }
    }

    /// The piece of the diff that "stage" or "discard" applies to, based on
    /// the cursor and the line selection.
    pub fn unit_at_cursor<'a>(
        &self,
        files_of: impl Fn(Section) -> &'a [Arc<FileDiff>],
    ) -> Option<Unit> {
        let row = self.rows.get(self.cursor)?;
        if self.anchor.is_some()
            && let Some((s, f, lines)) = self.selected_lines(&files_of)
        {
            return Some(Unit::Lines(s, files_of(s).get(f)?.clone(), lines));
        }
        let file = |sec: Section, i: usize| files_of(sec).get(i).cloned();
        Some(match *row {
            Row::Section(s) => Unit::Section(s),
            Row::File { sec, file: i } | Row::Notice { sec, file: i, .. } => {
                Unit::File(sec, file(sec, i)?.path().to_string())
            }
            Row::Hunk { sec, file: i, hunk } => Unit::Hunk(sec, file(sec, i)?, hunk),
            Row::Line {
                sec,
                file: i,
                hunk,
                line,
            } => {
                let f = file(sec, i)?;
                let kind = f.hunks.get(hunk)?.lines.get(line)?.kind;
                if kind == LineKind::Context {
                    Unit::Hunk(sec, f, hunk)
                } else {
                    Unit::Lines(sec, f, vec![(hunk, line)])
                }
            }
            Row::Split {
                sec,
                file: i,
                hunk,
                left,
                right,
            } => {
                let f = file(sec, i)?;
                let h = f.hunks.get(hunk)?;
                let lines: Vec<(usize, usize)> = [left, right]
                    .into_iter()
                    .flatten()
                    .filter(|&l| h.lines.get(l).is_some_and(|x| x.kind != LineKind::Context))
                    .map(|l| (hunk, l))
                    .collect();
                if lines.is_empty() {
                    Unit::Hunk(sec, f, hunk)
                } else {
                    Unit::Lines(sec, f, lines)
                }
            }
            _ => return None,
        })
    }

    /// Move the cursor to the next or previous row matching `pred`.
    pub fn jump(&mut self, forward: bool, pred: impl Fn(&Row) -> bool) {
        let n = self.rows.len();
        if forward {
            if let Some(i) = (self.cursor + 1..n).find(|&i| pred(&self.rows[i])) {
                self.cursor = i;
            }
        } else if let Some(i) = (0..self.cursor).rev().find(|&i| pred(&self.rows[i])) {
            self.cursor = i;
        }
    }
}

fn path_at(paths: &HashMap<Section, Vec<String>>, sec: Section, file: usize) -> String {
    paths
        .get(&sec)
        .and_then(|v| v.get(file))
        .cloned()
        .unwrap_or_default()
}

fn row_key(rows: &[Row], i: usize, paths: &HashMap<Section, Vec<String>>) -> RowKey {
    let path = |sec: Section, file: usize| path_at(paths, sec, file);
    match rows.get(i) {
        Some(Row::Meta(m)) => RowKey::Meta(*m),
        Some(Row::Section(s)) => RowKey::Section(*s),
        Some(Row::File { sec, file }) => RowKey::File(*sec, path(*sec, *file)),
        Some(Row::Notice { sec, file, .. }) => RowKey::Notice(*sec, path(*sec, *file)),
        Some(Row::Hunk { sec, file, hunk }) => RowKey::Hunk(*sec, path(*sec, *file), *hunk),
        Some(Row::Line {
            sec,
            file,
            hunk,
            line,
        }) => RowKey::Line(*sec, path(*sec, *file), *hunk, *line),
        Some(Row::Split {
            sec,
            file,
            hunk,
            left,
            right,
        }) => RowKey::Line(*sec, path(*sec, *file), *hunk, right.or(*left).unwrap_or(0)),
        _ => RowKey::Other,
    }
}

/// A row of the side-by-side view can show two lines, and `row_key` names
/// the one on the right. This names the one on the left, so that a removed
/// line can still be found after switching from the unified view.
fn left_key(rows: &[Row], i: usize, paths: &HashMap<Section, Vec<String>>) -> Option<RowKey> {
    match rows.get(i) {
        Some(Row::Split {
            sec,
            file,
            hunk,
            left: Some(l),
            right: Some(_),
        }) => Some(RowKey::Line(*sec, path_at(paths, *sec, *file), *hunk, *l)),
        _ => None,
    }
}

fn push_split_rows(
    rows: &mut Vec<Row>,
    sec: Section,
    file: usize,
    hunk: usize,
    lines: &[crate::git::diff::DiffLine],
) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Context {
            rows.push(Row::Split {
                sec,
                file,
                hunk,
                left: Some(i),
                right: Some(i),
            });
            i += 1;
            continue;
        }
        let mut dels = Vec::new();
        let mut adds = Vec::new();
        while i < lines.len() && lines[i].kind == LineKind::Del {
            dels.push(i);
            i += 1;
        }
        while i < lines.len() && lines[i].kind == LineKind::Add {
            adds.push(i);
            i += 1;
        }
        for k in 0..dels.len().max(adds.len()) {
            rows.push(Row::Split {
                sec,
                file,
                hunk,
                left: dels.get(k).copied(),
                right: adds.get(k).copied(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::sample::Index;

    use super::*;
    use crate::git::diff::{DiffLine, FileStatus, Hunk};

    const SECTIONS: [Section; 4] = [
        Section::Conflicted,
        Section::Untracked,
        Section::Unstaged,
        Section::Staged,
    ];

    /// A file that may be in the diff before a reload, after it, or both.
    #[derive(Debug, Clone)]
    struct Spec {
        diff: FileDiff,
        /// What the diff turns into on reload, as when one of its hunks
        /// gets staged.
        changed: Option<FileDiff>,
        before: bool,
        after: bool,
        folded: bool,
    }

    #[derive(Debug, Clone)]
    struct Case {
        /// Show a commit, with its files in `sections[0]`, rather than the
        /// working directory.
        commit: bool,
        split: bool,
        sections: [Vec<Spec>; 4],
        folded_sections: Vec<Section>,
    }

    /// What the pane shows, before or after the reload.
    struct Data {
        workdir: Option<Workdir>,
        commit: Option<Arc<CommitDetail>>,
    }

    impl Data {
        fn files(&self, sec: Section) -> &[Arc<FileDiff>] {
            section_files(self.workdir.as_ref(), self.commit.as_deref(), sec)
        }
    }

    fn hunk() -> impl Strategy<Value = Hunk> {
        let kind = prop_oneof![
            Just(LineKind::Context),
            Just(LineKind::Add),
            Just(LineKind::Del)
        ];
        prop::collection::vec(kind, 1..10).prop_map(|mut kinds| {
            // Git never prints a hunk without a change in it.
            if kinds.iter().all(|&k| k == LineKind::Context) {
                let mid = kinds.len() / 2;
                kinds[mid] = LineKind::Add;
            }
            let (mut old, mut new) = (1, 1);
            let lines: Vec<DiffLine> = kinds
                .into_iter()
                .map(|kind| {
                    let o = (kind != LineKind::Add).then_some(old);
                    let n = (kind != LineKind::Del).then_some(new);
                    old += u32::from(o.is_some());
                    new += u32::from(n.is_some());
                    DiffLine::new(kind, b"x", o, n)
                })
                .collect();
            Hunk {
                old_start: 1,
                old_len: old - 1,
                new_start: 1,
                new_len: new - 1,
                section: String::new(),
                lines,
            }
        })
    }

    /// Files in path order. Some are binary, too large, or have no hunks,
    /// so that they show a notice instead of a diff.
    fn files(max: usize) -> impl Strategy<Value = Vec<Spec>> {
        let body = || (0u8..8, prop::collection::vec(hunk(), 1..4));
        let changed = prop::option::weighted(0.25, body());
        let spec = (body(), changed, any::<bool>(), any::<bool>(), 0u8..5);
        prop::collection::vec(spec, 0..max).prop_map(|specs| {
            specs
                .into_iter()
                .enumerate()
                .map(|(i, (body, changed, before, after, folded))| {
                    let path = format!("dir/file{i:02}");
                    let diff = |(kind, hunks): (u8, Vec<Hunk>)| {
                        let mut diff = FileDiff::new(
                            FileStatus::Modified,
                            Some(path.clone()),
                            Some(path.clone()),
                        );
                        match kind {
                            0 => diff.binary = true,
                            1 => diff.too_large = true,
                            2 => {}
                            _ => diff.hunks = hunks,
                        }
                        diff
                    };
                    Spec {
                        diff: diff(body),
                        changed: changed.map(diff),
                        before,
                        after,
                        folded: folded == 0,
                    }
                })
                .collect()
        })
    }

    fn case() -> impl Strategy<Value = Case> {
        // A commit with more than 40 files starts with all of them folded.
        let commit_files = prop_oneof![4 => files(6), 1 => files(46)];
        let workdir = [files(5), files(5), files(5), files(5)];
        (
            any::<bool>(),
            any::<bool>(),
            commit_files,
            workdir,
            prop::sample::subsequence(SECTIONS.to_vec(), 0..=4),
        )
            .prop_map(|(commit, split, commit_files, workdir, folded_sections)| {
                let mut sections = workdir;
                if commit {
                    sections[0] = commit_files;
                }
                Case {
                    commit,
                    split,
                    sections,
                    folded_sections,
                }
            })
    }

    fn data(case: &Case, after: bool) -> Data {
        let pick = |specs: &[Spec]| -> Vec<Arc<FileDiff>> {
            specs
                .iter()
                .filter(|s| if after { s.after } else { s.before })
                .map(|s| match &s.changed {
                    Some(changed) if after => Arc::new(changed.clone()),
                    _ => Arc::new(s.diff.clone()),
                })
                .collect()
        };
        if case.commit {
            let person = crate::git::commit::Person {
                name: "A".into(),
                email: "a@example.com".into(),
                time: 0,
                offset: 0,
            };
            let files = pick(&case.sections[0]);
            let texts = vec![FileTexts::default(); files.len()];
            let commit = CommitDetail {
                id: ObjectId::null(gix::hash::Kind::Sha1),
                parents: vec![ObjectId::null(gix::hash::Kind::Sha1)],
                message: "Subject\n\nBody\n".into(),
                author: person.clone(),
                committer: person,
                files,
                texts,
            };
            Data {
                workdir: None,
                commit: Some(Arc::new(commit)),
            }
        } else {
            let [conflicted, untracked, unstaged, staged] = &case.sections;
            let workdir = Workdir {
                conflicted: pick(conflicted),
                untracked: pick(untracked),
                unstaged: pick(unstaged),
                staged: pick(staged),
                ..Workdir::default()
            };
            Data {
                workdir: Some(workdir),
                commit: None,
            }
        }
    }

    fn shown_sections(case: &Case) -> Vec<(Section, &[Spec])> {
        if case.commit {
            vec![(Section::Commit, case.sections[0].as_slice())]
        } else {
            SECTIONS
                .into_iter()
                .zip(case.sections.iter().map(Vec::as_slice))
                .collect()
        }
    }

    /// A pane showing the data from before the reload.
    fn detail(case: &Case) -> (Detail, Data) {
        let data = data(case, false);
        let mut d = Detail::default();
        match &data.commit {
            Some(c) => {
                d.set_source(Source::Commit(c.id));
                d.set_commit(c.clone());
            }
            None => d.set_source(Source::Workdir),
        }
        for (sec, specs) in shown_sections(case) {
            for s in specs.iter().filter(|s| s.folded) {
                d.collapsed.insert((sec, s.diff.path().to_string()));
            }
        }
        d.collapsed_sections = case.folded_sections.iter().copied().collect();
        rebuild(&mut d, &data, case.split);
        (d, data)
    }

    fn rebuild(d: &mut Detail, data: &Data, split: bool) {
        d.rebuild(
            data.workdir.as_ref(),
            &HashMap::new(),
            &Theme::default(),
            split,
            &Config::default(),
        );
    }

    /// A row with its file named by path rather than by index, so that rows
    /// can be compared across a reload.
    fn by_path(row: &Row, data: &Data) -> (Option<(Section, String)>, Row) {
        let mut row = row.clone();
        let file = row
            .file()
            .map(|(sec, i)| (sec, data.files(sec)[i].path().to_string()));
        if let Row::File { file, .. }
        | Row::Notice { file, .. }
        | Row::Hunk { file, .. }
        | Row::Line { file, .. }
        | Row::Split { file, .. } = &mut row
        {
            *file = 0;
        }
        (file, row)
    }

    /// Every file that should be shown has a header, and then either a
    /// notice or its hunks. Each hunk's lines follow its header in order,
    /// so each changed line shows up exactly once.
    fn check_rows(d: &Detail, data: &Data, split: bool) -> Result<(), TestCaseError> {
        let mut sections = Vec::new();
        let mut files = Vec::new();
        let mut notices = Vec::new();
        let mut hunks = Vec::new();
        // The left and right columns of each hunk. A unified row is on both.
        let mut columns: HashMap<_, (Vec<usize>, Vec<usize>)> = HashMap::new();
        let mut file_now = None;
        let mut hunk_now = None;
        for row in &d.rows {
            match *row {
                Row::Section(sec) => {
                    sections.push(sec);
                    (file_now, hunk_now) = (None, None);
                }
                Row::File { sec, file } => {
                    files.push((sec, file));
                    (file_now, hunk_now) = (Some((sec, file)), None);
                }
                Row::Notice { sec, file, .. } => {
                    prop_assert_eq!(file_now, Some((sec, file)));
                    notices.push((sec, file));
                }
                Row::Hunk { sec, file, hunk } => {
                    prop_assert_eq!(file_now, Some((sec, file)));
                    hunks.push((sec, file, hunk));
                    hunk_now = Some((sec, file, hunk));
                }
                Row::Line {
                    sec,
                    file,
                    hunk,
                    line,
                } => {
                    prop_assert!(!split);
                    prop_assert_eq!(hunk_now, Some((sec, file, hunk)));
                    let (l, r) = columns.entry((sec, file, hunk)).or_default();
                    l.push(line);
                    r.push(line);
                }
                Row::Split {
                    sec,
                    file,
                    hunk,
                    left,
                    right,
                } => {
                    prop_assert!(split);
                    prop_assert_eq!(hunk_now, Some((sec, file, hunk)));
                    let lines = &data.files(sec)[file].hunks[hunk].lines;
                    let kind = |i: Option<usize>| i.map(|i| lines[i].kind);
                    prop_assert!(left.is_some() || right.is_some());
                    prop_assert_ne!(kind(left), Some(LineKind::Add));
                    prop_assert_ne!(kind(right), Some(LineKind::Del));
                    if kind(left) == Some(LineKind::Context)
                        || kind(right) == Some(LineKind::Context)
                    {
                        prop_assert_eq!(left, right);
                    }
                    let (l, r) = columns.entry((sec, file, hunk)).or_default();
                    l.extend(left);
                    r.extend(right);
                }
                Row::Meta(_) | Row::Blank | Row::Empty(_) => {
                    (file_now, hunk_now) = (None, None);
                }
            }
        }

        let shown: Vec<Section> = match &data.workdir {
            Some(_) => SECTIONS
                .into_iter()
                .filter(|&s| !data.files(s).is_empty())
                .collect(),
            None => vec![Section::Commit],
        };
        if data.workdir.is_some() {
            prop_assert_eq!(&sections, &shown);
        }
        let mut want_files = Vec::new();
        let mut want_notices = Vec::new();
        let mut want_hunks = Vec::new();
        for sec in shown {
            if sec != Section::Commit && d.collapsed_sections.contains(&sec) {
                continue;
            }
            for (fi, f) in data.files(sec).iter().enumerate() {
                want_files.push((sec, fi));
                if d.collapsed.contains(&(sec, f.path().to_string())) {
                    continue;
                }
                if f.binary || f.too_large || f.hunks.is_empty() {
                    want_notices.push((sec, fi));
                    continue;
                }
                for (hi, h) in f.hunks.iter().enumerate() {
                    want_hunks.push((sec, fi, hi));
                    let (left, right) = columns.remove(&(sec, fi, hi)).unwrap_or_default();
                    let side = |skip: LineKind| -> Vec<usize> {
                        (0..h.lines.len())
                            .filter(|&i| split && h.lines[i].kind != skip || !split)
                            .collect()
                    };
                    prop_assert_eq!(
                        left,
                        side(LineKind::Add),
                        "left side of {:?}",
                        (sec, fi, hi)
                    );
                    prop_assert_eq!(
                        right,
                        side(LineKind::Del),
                        "right side of {:?}",
                        (sec, fi, hi)
                    );
                }
            }
        }
        prop_assert_eq!(files, want_files);
        prop_assert_eq!(notices, want_notices);
        prop_assert_eq!(hunks, want_hunks);
        prop_assert!(columns.is_empty(), "lines of hidden hunks: {:?}", columns);
        Ok(())
    }

    /// Selecting a hunk from its header to its last line, in either
    /// direction, or only its lines, selects exactly its changed lines.
    fn check_hunk_selection(d: &mut Detail, data: &Data) -> Result<(), TestCaseError> {
        for start in 0..d.rows.len() {
            let Row::Hunk { sec, file, hunk } = d.rows[start] else {
                continue;
            };
            let end = (start + 1..d.rows.len())
                .take_while(|&i| {
                    matches!(d.rows[i], Row::Line { .. } | Row::Split { .. })
                        && d.rows[i].file() == Some((sec, file))
                })
                .last()
                .unwrap_or(start);
            let want: Vec<(usize, usize)> = data.files(sec)[file].hunks[hunk]
                .changed_lines()
                .map(|l| (hunk, l))
                .collect();
            for (anchor, cursor) in [(start, end), (end, start), (start + 1, end)] {
                d.anchor = Some(anchor);
                d.cursor = cursor;
                let got = d.selected_lines(|s| data.files(s));
                let (s, f, mut lines) = got.ok_or_else(|| {
                    TestCaseError::fail(format!("nothing selected in rows {anchor}..{cursor}"))
                })?;
                prop_assert_eq!((s, f), (sec, file));
                let n = lines.len();
                lines.sort_unstable();
                lines.dedup();
                prop_assert_eq!(lines.len(), n, "a line was selected twice");
                prop_assert_eq!(&lines, &want);
                match d.unit_at_cursor(|s| data.files(s)) {
                    Some(Unit::Lines(s, _, mut lines)) => {
                        prop_assert_eq!(s, sec);
                        lines.sort_unstable();
                        prop_assert_eq!(&lines, &want);
                    }
                    other => {
                        prop_assert!(false, "unit at cursor: {:?}", other.map(|u| u.section()));
                    }
                }
            }
        }
        d.anchor = None;
        Ok(())
    }

    /// Whether two rows show the same thing. A unified row and a row of
    /// the side-by-side view match when they share a line.
    fn same_place(a: &Row, b: &Row) -> bool {
        let lines = |r: &Row| match *r {
            Row::Line {
                sec,
                file,
                hunk,
                line,
            } => Some((sec, file, hunk, vec![line])),
            Row::Split {
                sec,
                file,
                hunk,
                left,
                right,
            } => Some((sec, file, hunk, left.into_iter().chain(right).collect())),
            _ => None,
        };
        match (lines(a), lines(b)) {
            (Some((s, f, h, a)), Some((t, g, i, b))) => {
                (s, f, h) == (t, g, i) && a.iter().any(|l| b.contains(l))
            }
            _ => a == b,
        }
    }

    proptest! {
        #[test]
        fn rows_show_every_line(case in case()) {
            let (mut d, data) = detail(&case);
            check_rows(&d, &data, case.split)?;
            check_hunk_selection(&mut d, &data)?;
        }

        /// Rebuilding with the same data leaves the cursor and the line
        /// selection on the same rows.
        #[test]
        fn rebuild_keeps_cursor(
            case in case(),
            at in prop::collection::vec((any::<Index>(), any::<Option<Index>>()), 1..8),
        ) {
            let (mut d, data) = detail(&case);
            let rows = d.rows.clone();
            for (cursor, anchor) in at {
                let cursor = cursor.index(rows.len());
                let anchor = anchor.map(|a| a.index(rows.len()));
                d.cursor = cursor;
                d.anchor = anchor;
                rebuild(&mut d, &data, case.split);
                prop_assert_eq!(&d.rows, &rows);
                prop_assert_eq!(d.cursor, cursor, "cursor moved from {:?} to {:?}", rows[cursor], rows[d.cursor]);
                prop_assert_eq!(d.anchor, anchor);
            }
        }

        /// Switching between the unified and the side-by-side view keeps the
        /// cursor and the line selection on the same lines.
        #[test]
        fn switching_views_keeps_cursor(
            case in case(),
            cursor in any::<Index>(),
            anchor in any::<Option<Index>>(),
        ) {
            let (mut d, data) = detail(&case);
            let rows = d.rows.clone();
            d.cursor = cursor.index(rows.len());
            d.anchor = anchor.map(|a| a.index(rows.len()));
            let (old_cursor, old_anchor) = (d.cursor, d.anchor);
            rebuild(&mut d, &data, !case.split);
            check_rows(&d, &data, !case.split)?;
            let (old, new) = (&rows[old_cursor], &d.rows[d.cursor]);
            if !matches!(old, Row::Blank | Row::Empty(_)) {
                prop_assert!(same_place(old, new), "cursor moved from {:?} to {:?}", old, new);
            }
            if let Some(a) = old_anchor
                && !matches!(rows[a], Row::Blank | Row::Empty(_))
            {
                let new = d.anchor.map(|b| &d.rows[b]);
                prop_assert!(
                    new.is_some_and(|new| same_place(&rows[a], new)),
                    "anchor moved from {:?} to {:?}", rows[a], new
                );
            }
        }

        /// After files change, go away or appear, the cursor is still on a
        /// row. If its file is still shown, it stays in that file, and on
        /// the same row if the file's diff didn't change.
        #[test]
        fn cursor_follows_its_file(case in case(), at in any::<Index>()) {
            let (mut d, before) = detail(&case);
            d.cursor = at.index(d.rows.len());
            let old = by_path(&d.rows[d.cursor], &before);
            let after = data(&case, true);
            if let Some(c) = &after.commit {
                d.set_commit(c.clone());
            }
            rebuild(&mut d, &after, case.split);
            check_rows(&d, &after, case.split)?;
            prop_assert!(d.cursor < d.rows.len());
            let new = by_path(&d.rows[d.cursor], &after);
            if let (Some((sec, path)), _) = &old {
                let still_shown = d
                    .rows
                    .iter()
                    .any(|r| r.file().is_some_and(|(s, i)| s == *sec && after.files(s)[i].path() == path));
                let specs = &case.sections[if case.commit { 0 } else { *sec as usize - 1 }];
                let changed = specs.iter().any(|s| s.diff.path() == path && s.changed.is_some());
                if still_shown && changed {
                    prop_assert_eq!(&new.0, &old.0);
                } else if still_shown {
                    prop_assert_eq!(&new, &old);
                }
            } else if let Row::Section(sec) = old.1
                && !after.files(sec).is_empty()
            {
                prop_assert_eq!(&new.1, &Row::Section(sec));
            }
        }
    }
}
