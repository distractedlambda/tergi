//! Keyboard and mouse handling.

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use super::cmd::{ActionDef, Cmd, Overlay, SideKey, SideRow, Via};
use super::doc::{Depth, Row, Source};
use super::planner::PlanAction;
use super::{App, Drag, Focus, HISTORY_ROW_HEIGHT, Splitter};
use crate::ui::textarea::Outcome;

/// Something on screen that reacts to the mouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Splitter(Splitter),
    SidebarRow(usize),
    HistoryRow(usize),
    DocRow(usize),
    Button(Cmd),
    Message,
    /// The commit message editor in the detail pane.
    Edit,
    MenuItem(usize),
    PaletteItem(usize),
    PlannerRow(usize),
    PlannerAction(usize),
    InputText,
    /// The inside of a dialog. Clicks here do nothing, but don't close it.
    Overlay,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How long a scroll on one axis blocks scrolling on the other. See
/// `scroll_axis_free`.
pub const SCROLL_LOCK: Duration = Duration::from_millis(100);

impl App {
    pub fn handle_input(&mut self, ev: Event) {
        self.by_mouse = matches!(ev, Event::Mouse(_));
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.handle_key(k),
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Paste(s) => self.handle_paste(&s),
            Event::FocusGained => self.refresh(super::REFRESH_ALL),
            Event::Resize(w, h) => self.size = (w, h),
            _ => {}
        }
    }

    fn handle_paste(&mut self, s: &str) {
        match self.overlays.last_mut() {
            Some(Overlay::Input(b)) => b.text.insert_str(s),
            Some(Overlay::Palette {
                input, selected, ..
            }) => {
                input.insert_str(s);
                *selected = 0;
            }
            Some(_) => {}
            None => match &mut self.edit {
                Some(e) => e.input.text.insert_str(s),
                None if self.focus == Focus::Message => self.message.insert_str(s),
                None => {}
            },
        }
    }

    fn handle_key(&mut self, k: KeyEvent) {
        if !self.overlays.is_empty() {
            self.overlay_key(k);
            return;
        }
        if let Some(e) = &mut self.edit {
            match e.input.text.handle_key(k) {
                Outcome::Submit => self.submit_edit(),
                Outcome::Cancel => self.cancel_edit(),
                Outcome::Handled | Outcome::Ignored => {}
            }
            return;
        }
        if self.planner.is_some() {
            if self.nav_planner(k) {
                return;
            }
            if let Some(a) = find_action(&self.actions(super::cmd::Context::Planner), &k) {
                self.run_cmd(a);
                return;
            }
            if let Some(a) = find_action(&self.global_actions(), &k)
                && matches!(
                    a,
                    Cmd::Help | Cmd::Quit | Cmd::Palette | Cmd::CommandLog | Cmd::ContextMenu
                )
            {
                self.run_cmd(a);
            }
            return;
        }
        if self.focus == Focus::Message {
            self.message_key(k);
            return;
        }
        if self.nav_key(k) {
            return;
        }
        let ctx = self.context();
        if let Some(a) = find_action(&self.actions(ctx), &k) {
            self.run_cmd(a);
            return;
        }
        if let Some(a) = find_action(&self.global_actions(), &k) {
            self.run_cmd(a);
            return;
        }
        if k.code == KeyCode::F(10) && k.modifiers.contains(KeyModifiers::SHIFT)
            || k.code == KeyCode::Menu
        {
            self.open_context_menu(None);
        }
    }

    fn message_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Tab => return self.run_cmd(Cmd::FocusNext),
            KeyCode::BackTab => return self.run_cmd(Cmd::FocusPrev),
            KeyCode::Char('a') if k.modifiers == KeyModifiers::ALT => return self.toggle_amend(),
            KeyCode::Char('c') if k.modifiers == KeyModifiers::CONTROL => {
                return self.focus = Focus::Detail;
            }
            _ => {}
        }
        match self.message.handle_key(k) {
            Outcome::Submit => self.run_cmd(Cmd::Commit),
            Outcome::Cancel => self.focus = Focus::Detail,
            Outcome::Handled | Outcome::Ignored => {}
        }
    }

    /// Movement keys for the focused list. Returns true if the key was used.
    fn nav_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = |h: u16| usize::from(h.max(2)) - 1;
        #[derive(Clone, Copy)]
        enum Move {
            By(isize),
            Top,
            Bottom,
        }
        let mv = match k.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => Some(Move::By(1)),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => Some(Move::By(-1)),
            KeyCode::Char('g') | KeyCode::Home if !ctrl => Some(Move::Top),
            KeyCode::Char('G') | KeyCode::End => Some(Move::Bottom),
            KeyCode::PageDown => Some(Move::By(1000)),
            KeyCode::PageUp => Some(Move::By(-1000)),
            KeyCode::Char('d') if ctrl => Some(Move::By(500)),
            KeyCode::Char('u') if ctrl => Some(Move::By(-500)),
            _ => None,
        };
        if let Some(mv) = mv
            && matches!(self.focus, Focus::Detail | Focus::Message)
        {
            let p = page(self.areas.doc.height) as isize;
            let cur = self.detail.cursor as isize;
            let (lo, hi) = self.detail.level_range();
            let extend = k.modifiers.contains(KeyModifiers::SHIFT);
            let target = match mv {
                Move::By(1) if self.detail.depth() == Depth::Hunks => {
                    return self.step_hunk(true);
                }
                Move::By(-1) if self.detail.depth() == Depth::Hunks => {
                    return self.step_hunk(false);
                }
                Move::Top => self.detail.level_target(lo as isize, true),
                Move::Bottom => self.detail.level_target(hi as isize, false),
                Move::By(1000) => self.detail.level_target(cur + p, true),
                Move::By(-1000) => self.detail.level_target(cur - p, false),
                Move::By(500) => self.detail.level_target(cur + p / 2, true),
                Move::By(-500) => self.detail.level_target(cur - p / 2, false),
                Move::By(d) => self.detail.level_target(cur + d, d > 0),
            };
            self.detail_move_to(target, extend && self.detail.depth() == Depth::Lines);
            return true;
        }
        if let Some(mv) = mv {
            let (p, len, cur) = match self.focus {
                Focus::Sidebar => (
                    page(self.areas.sidebar.height),
                    self.sidebar.rows.len(),
                    self.sidebar.selected,
                ),
                Focus::History => (
                    page(self.history_page() as u16),
                    self.history.row_count(),
                    self.history.current_row(),
                ),
                _ => (
                    page(self.areas.doc.height),
                    self.detail.rows.len(),
                    self.detail.cursor,
                ),
            };
            let p = p as isize;
            let target = match mv {
                Move::Top => 0,
                Move::Bottom => len.saturating_sub(1),
                Move::By(1000) => cur.saturating_add(p as usize),
                Move::By(-1000) => cur.saturating_sub(p as usize),
                Move::By(500) => cur.saturating_add((p / 2) as usize),
                Move::By(-500) => cur.saturating_sub((p / 2) as usize),
                Move::By(d) => (cur as isize + d).max(0) as usize,
            };
            let target = target.min(len.saturating_sub(1));
            match self.focus {
                Focus::Sidebar => self.sidebar_select(target),
                Focus::History => {
                    // Shift with a letter, such as `G`, is just the letter.
                    if k.modifiers.contains(KeyModifiers::SHIFT)
                        && !matches!(k.code, KeyCode::Char(_))
                    {
                        self.start_marking();
                    }
                    self.select_history(target);
                }
                _ => self.detail_move_to(target, k.modifiers.contains(KeyModifiers::SHIFT)),
            }
            return true;
        }
        match (self.focus, k.code) {
            (Focus::History, KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right) => {
                self.focus = Focus::Detail;
                true
            }
            (Focus::History, KeyCode::Char('h') | KeyCode::Left) if self.sidebar_shown() => {
                self.focus = Focus::Sidebar;
                true
            }
            (Focus::History, KeyCode::Esc) if self.history.anchor.is_some() => {
                self.stop_marking();
                true
            }
            (Focus::Sidebar, KeyCode::Char('l') | KeyCode::Right) => {
                self.focus = Focus::History;
                true
            }
            (Focus::Sidebar, KeyCode::Enter) => {
                match self.sidebar.rows.get(self.sidebar.selected).cloned() {
                    Some(SideRow::Header { key, .. }) => {
                        self.run_cmd(Cmd::ToggleSideSection(SideKey::Header(key)));
                    }
                    Some(SideRow::Remote(r)) => {
                        self.run_cmd(Cmd::ToggleSideSection(SideKey::Remote(r)));
                    }
                    _ => self.focus = Focus::History,
                }
                true
            }
            (Focus::Detail, KeyCode::Left) => {
                self.detail
                    .hscroll_by(self.detail.cursor, -8, self.areas.doc.width);
                true
            }
            (Focus::Detail, KeyCode::Right) => {
                self.detail
                    .hscroll_by(self.detail.cursor, 8, self.areas.doc.width);
                true
            }
            // Esc finishes a line selection, then backs out a level. At the
            // file level it falls through and opens the git menu.
            (Focus::Detail, KeyCode::Esc) if self.detail.anchor.is_some() => {
                self.detail.anchor = None;
                true
            }
            (Focus::Detail, KeyCode::Esc) if self.detail.depth() != Depth::Files => {
                self.ascend();
                true
            }
            (Focus::Detail, KeyCode::Enter) => {
                self.descend(true);
                true
            }
            _ => false,
        }
    }

    fn activate_doc_row(&mut self, i: usize) {
        match self.detail.rows.get(i).cloned() {
            Some(Row::File { .. } | Row::Section(_)) => self.run_cmd(Cmd::ToggleFold),
            Some(Row::Notice { .. }) => self.run_cmd(Cmd::LoadFull),
            Some(Row::Meta(m)) => {
                if let Some(c) = self.detail.meta.get(m).and_then(|m| m.click.clone()) {
                    self.run_cmd(c);
                }
            }
            _ => {}
        }
    }

    /// Move to the next or previous hunk, in any file. Folded files on the
    /// way are unfolded, so that their hunks can be reached.
    fn step_hunk(&mut self, forward: bool) -> bool {
        self.detail.anchor = None;
        loop {
            let d = &self.detail;
            let at = |i: usize| match d.rows[i] {
                Row::Hunk { .. } | Row::Notice { .. } => Some(Ok(i)),
                Row::File { sec, file } => self
                    .section_files(sec)
                    .get(file)
                    .map(|f| (sec, f.path().to_owned()))
                    .filter(|key| d.collapsed.contains(key))
                    .map(Err),
                _ => None,
            };
            let found = if forward {
                (d.cursor + 1..d.rows.len()).find_map(at)
            } else {
                (0..d.cursor).rev().find_map(at)
            };
            match found {
                Some(Ok(i)) => {
                    self.detail.cursor = i;
                    break;
                }
                // The rebuild keeps the cursor on its row, so the next
                // search starts from the same place and finds the hunks
                // of the file that just opened.
                Some(Err((sec, path))) => {
                    self.set_folded([(sec, path, false)]);
                    self.rebuild_doc();
                }
                None => break,
            }
        }
        self.ensure_doc_visible();
        true
    }

    /// Go one level into the detail pane. `enter` is for the Enter key,
    /// which also follows links in the commit details and loads diffs that
    /// were too large to show.
    pub fn descend(&mut self, enter: bool) {
        let i = self.detail.cursor;
        match self.detail.rows.get(i).cloned() {
            Some(Row::Meta(m)) if enter => {
                if let Some(c) = self.detail.meta.get(m).and_then(|m| m.click.clone()) {
                    self.run_cmd(c);
                }
            }
            Some(Row::Notice { .. }) if enter => self.run_cmd(Cmd::LoadFull),
            Some(Row::Section(s)) => {
                if self.detail.collapsed_sections.remove(&s) {
                    self.rebuild_doc();
                }
                self.step_hunk(true);
            }
            Some(Row::File { sec, file }) => {
                if let Some(path) = self
                    .section_files(sec)
                    .get(file)
                    .map(|f| f.path().to_owned())
                    && self.set_folded([(sec, path, false)])
                {
                    self.rebuild_doc();
                }
                if let Some(j) = self.detail.first_inside(self.detail.cursor, Depth::Hunks) {
                    self.detail.cursor = j;
                }
            }
            Some(Row::Hunk { .. }) => {
                if let Some(j) = self.detail.first_inside(i, Depth::Lines) {
                    self.detail.cursor = j;
                    let (_, hi) = self.detail.level_range();
                    if let Some(c) = (j..=hi).find(|&r| self.changed_row(r)) {
                        self.detail.cursor = c;
                    }
                }
            }
            _ => {}
        }
        self.ensure_doc_visible();
    }

    /// Go one level out of the detail pane, or back to the history from
    /// the files.
    pub fn ascend(&mut self) {
        self.detail.anchor = None;
        if self.detail.depth() == Depth::Files {
            self.focus = Focus::History;
        } else if let Some(i) = self.detail.parent_row() {
            self.detail.cursor = i;
            self.ensure_doc_visible();
        }
    }

    /// Whether row `i` shows an added or removed line.
    fn changed_row(&self, i: usize) -> bool {
        let line = |sec: super::doc::Section, file: usize, hunk: usize, line: Option<usize>| {
            line.is_some_and(|l| {
                self.section_files(sec)
                    .get(file)
                    .and_then(|f| f.hunks.get(hunk))
                    .and_then(|h| h.lines.get(l))
                    .is_some_and(|l| l.kind != crate::git::diff::LineKind::Context)
            })
        };
        match self.detail.rows.get(i) {
            Some(&Row::Line {
                sec,
                file,
                hunk,
                line: l,
            }) => line(sec, file, hunk, Some(l)),
            Some(&Row::Split {
                sec,
                file,
                hunk,
                left,
                right,
            }) => line(sec, file, hunk, left) || line(sec, file, hunk, right),
            _ => false,
        }
    }

    pub fn detail_move_to(&mut self, row: usize, extend: bool) {
        if extend && self.detail.anchor.is_none() {
            self.detail.anchor = Some(self.detail.cursor);
        }
        self.detail.cursor = row.min(self.detail.rows.len().saturating_sub(1));
        self.ensure_doc_visible();
    }

    /// Scroll the detail pane as little as it takes to show the cursor's
    /// row. On a file's header, that's the whole file, and when the file
    /// doesn't fit, the header goes at the top. Keyboard moves call this.
    /// A click on a row doesn't, so it never scrolls what's under the
    /// mouse.
    pub fn ensure_doc_visible(&mut self) {
        let h = usize::from(self.areas.doc.height.max(1));
        let c = self.detail.cursor;
        let rows = &self.detail.rows;
        let last = match rows.get(c) {
            Some(&Row::File { sec, file }) => (c + 1..rows.len())
                .take_while(|&i| rows[i].file() == Some((sec, file)))
                .last()
                .unwrap_or(c),
            _ => c,
        };
        if c < self.detail.scroll || last - c >= h {
            self.detail.scroll = c;
        } else if last >= self.detail.scroll + h {
            self.detail.scroll = last + 1 - h;
        }
    }

    pub fn sidebar_select(&mut self, i: usize) {
        self.sidebar.selected = i.min(self.sidebar.rows.len().saturating_sub(1));
        self.ensure_sidebar_visible();
        // Show what the selected ref points at.
        let target = match self.sidebar.rows.get(self.sidebar.selected) {
            Some(SideRow::Branch(i)) => self.refs.branches.get(*i).map(|b| b.target),
            Some(SideRow::RemoteBranch(i)) => self.refs.remote_branches.get(*i).map(|b| b.target),
            Some(SideRow::Tag(i)) => self.refs.tags.get(*i).map(|t| t.target),
            Some(SideRow::Stash(i)) => {
                if let Some(id) = self.refs.stashes.get(*i).map(|s| s.id) {
                    self.show_source(Source::Stash(id));
                }
                None
            }
            _ => None,
        };
        if let Some(id) = target {
            self.jump_to(id);
        }
    }

    pub fn ensure_sidebar_visible(&mut self) {
        let h = usize::from(self.areas.sidebar.height.max(1));
        if self.sidebar.selected < self.sidebar.scroll {
            self.sidebar.scroll = self.sidebar.selected;
        } else if self.sidebar.selected >= self.sidebar.scroll + h {
            self.sidebar.scroll = self.sidebar.selected + 1 - h;
        }
    }

    fn nav_planner(&mut self, k: KeyEvent) -> bool {
        let Some(p) = &mut self.planner else {
            return false;
        };
        let n = p.rows.len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => {
                p.selected = (p.selected + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up if !ctrl => p.selected = p.selected.saturating_sub(1),
            KeyCode::Char('J') => p.move_selected(false),
            KeyCode::Char('K') => p.move_selected(true),
            KeyCode::Char('g') | KeyCode::Home => p.selected = 0,
            KeyCode::Char('G') | KeyCode::End => p.selected = n.saturating_sub(1),
            _ => return false,
        }
        true
    }

    fn overlay_key(&mut self, k: KeyEvent) {
        let Some(top) = self.overlays.last_mut() else {
            return;
        };
        // Ctrl+K and Ctrl+J are menu items in the history, so they don't
        // move through menus.
        let plain = !k.modifiers.contains(KeyModifiers::CONTROL);
        let up = matches!(k.code, KeyCode::Up)
            || (plain && k.code == KeyCode::Char('k'))
            || (k.code == KeyCode::Char('p') && k.modifiers == KeyModifiers::CONTROL);
        let down = matches!(k.code, KeyCode::Down)
            || (plain && k.code == KeyCode::Char('j'))
            || (k.code == KeyCode::Char('n') && k.modifiers == KeyModifiers::CONTROL);
        match top {
            Overlay::Menu(m) => {
                let n = m.items.len();
                if k.code == KeyCode::Esc || k.code == KeyCode::Char('q') {
                    self.cancel_menu();
                } else if up || down {
                    let mut i = m.selected;
                    for _ in 0..n {
                        i = if up { (i + n - 1) % n } else { (i + 1) % n };
                        if m.items[i].enabled {
                            break;
                        }
                    }
                    m.selected = i;
                    if m.preview.is_some()
                        && let Some(a) = m.items.get(i).filter(|a| a.enabled)
                    {
                        let cmd = a.cmd.preview();
                        self.run_cmd(cmd);
                    }
                } else if k.code == KeyCode::Enter {
                    let item = m.items.get(m.selected).cloned();
                    self.overlays.pop();
                    if let Some(a) = item.filter(|a| a.enabled) {
                        self.run_cmd(a.cmd);
                    }
                } else if let Some(a) = m
                    .items
                    .iter()
                    .filter(|_| !m.help)
                    .find(|a| a.enabled && a.key.is_some_and(|key| key.matches(&k)))
                {
                    let cmd = a.cmd.clone();
                    self.overlays.pop();
                    self.run_cmd(cmd);
                }
            }
            Overlay::Confirm { yes, .. } => match k.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let cmd = yes.clone();
                    self.overlays.pop();
                    self.run_cmd(cmd);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.overlays.pop();
                }
                _ => {}
            },
            Overlay::Input(b) => match b.text.handle_key(k) {
                Outcome::Submit => {
                    if let Some(Overlay::Input(b)) = self.overlays.pop() {
                        self.submit_input(b);
                    }
                }
                Outcome::Cancel => {
                    if let Some(Overlay::Input(b)) = self.overlays.pop() {
                        self.cancel_input(&b);
                    }
                }
                _ => {}
            },
            Overlay::Palette {
                input,
                items,
                selected,
            } => {
                let filtered = palette_filter(items, &input.text());
                if k.code == KeyCode::Esc {
                    self.overlays.pop();
                } else if k.code == KeyCode::Enter {
                    let cmd = filtered.get(*selected).map(|&i| items[i].cmd.clone());
                    self.overlays.pop();
                    if let Some(c) = cmd {
                        self.run_cmd(c);
                    }
                } else if matches!(k.code, KeyCode::Up)
                    || (k.code == KeyCode::Char('p') && k.modifiers == KeyModifiers::CONTROL)
                {
                    *selected = selected.saturating_sub(1);
                } else if matches!(k.code, KeyCode::Down)
                    || (k.code == KeyCode::Char('n') && k.modifiers == KeyModifiers::CONTROL)
                {
                    *selected = (*selected + 1).min(filtered.len().saturating_sub(1));
                } else if input.handle_key(k) == Outcome::Handled {
                    *selected = 0;
                }
            }
            Overlay::Log { scroll } | Overlay::Message { scroll, .. } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.overlays.pop();
                }
                _ if up => *scroll = scroll.saturating_sub(1),
                _ if down => *scroll = scroll.saturating_add(1),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                _ => {}
            },
        }
    }

    /// Close the top menu without choosing anything from it.
    fn cancel_menu(&mut self) {
        if let Some(Overlay::Menu(m)) = self.overlays.pop()
            && let Some(undo) = m.preview
        {
            self.run_cmd(undo);
        }
    }

    /// The mouse pointer shape for whatever is under the mouse.
    pub fn pointer_shape(&self) -> &'static str {
        match self.target_at(self.mouse.0, self.mouse.1) {
            Some(Target::Splitter(_)) => "ew-resize",
            Some(
                Target::Button(_)
                | Target::MenuItem(_)
                | Target::PaletteItem(_)
                | Target::PlannerAction(_),
            ) => "pointer",
            Some(Target::InputText | Target::Message | Target::Edit) => "text",
            _ => "default",
        }
    }

    fn target_at(&self, x: u16, y: u16) -> Option<Target> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, t)| t.clone())
    }

    fn handle_mouse(&mut self, m: MouseEvent) {
        self.mouse = (m.column, m.row);
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self
                    .last_click
                    .is_some_and(|(t, lx, ly)| t.elapsed() < DOUBLE_CLICK && lx == x && ly == y);
                self.last_click = Some((Instant::now(), x, y));
                let target = self.target_at(x, y);
                self.click(
                    target,
                    x,
                    y,
                    double,
                    m.modifiers.contains(KeyModifiers::SHIFT),
                );
            }
            MouseEventKind::Down(MouseButton::Right) => {
                let target = self.target_at(x, y);
                self.right_click(target.as_ref(), x, y);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(x, y),
            MouseEventKind::Up(_) => self.drag = None,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                let sideways = m.modifiers.contains(KeyModifiers::SHIFT);
                if !self.scroll_axis_free(sideways) {
                    return;
                }
                if sideways {
                    self.hscroll_at(x, y, down);
                } else {
                    self.scroll_at(x, y, if down { 1 } else { -1 });
                }
            }
            MouseEventKind::ScrollRight | MouseEventKind::ScrollLeft
                if self.scroll_axis_free(true) =>
            {
                self.hscroll_at(x, y, m.kind == MouseEventKind::ScrollRight);
            }
            _ => {}
        }
    }

    /// Whether a scroll step on one axis should move anything. A diagonal
    /// swipe on a trackpad sends steps on both axes, but the user almost
    /// always means to scroll only one way. So we drop steps on one axis
    /// while the other axis has scrolled within `SCROLL_LOCK`. Dropped
    /// steps don't restart the clock. That lets the other axis take over
    /// soon after the first one stops.
    fn scroll_axis_free(&mut self, sideways: bool) -> bool {
        let now = Instant::now();
        if let Some((axis, t)) = self.last_scroll
            && axis != sideways
            && now.duration_since(t) < SCROLL_LOCK
        {
            return false;
        }
        self.last_scroll = Some((sideways, now));
        true
    }

    fn hscroll_at(&mut self, x: u16, y: u16, right: bool) {
        if let Some(Target::DocRow(i)) = self.target_at(x, y) {
            self.detail
                .hscroll_by(i, if right { 1 } else { -1 }, self.areas.doc.width);
        }
    }

    fn click(&mut self, target: Option<Target>, x: u16, y: u16, double: bool, shift: bool) {
        // Clicking outside a menu closes it. Other dialogs stay open. The hit
        // map only holds the top overlay's targets, so any hit is inside it.
        let Some(target) = target else {
            match self.overlays.last() {
                Some(Overlay::Menu(_)) => self.cancel_menu(),
                Some(Overlay::Palette { .. } | Overlay::Log { .. }) => {
                    self.overlays.pop();
                }
                _ => {}
            }
            return;
        };
        // While a commit message is being edited, the rest of the screen
        // only shows what it is about. Clicks there do nothing, as they do
        // around a dialog.
        if self.overlays.is_empty()
            && self.edit.is_some()
            && !matches!(
                target,
                Target::Edit
                    | Target::Splitter(_)
                    | Target::Button(Cmd::SubmitInput | Cmd::CloseOverlay)
            )
        {
            return;
        }
        match target {
            Target::Splitter(s) => self.drag = Some(Drag::Splitter(s)),
            Target::SidebarRow(i) => {
                self.focus = Focus::Sidebar;
                self.sidebar_select(i);
                if double {
                    match self.sidebar.rows.get(i).cloned() {
                        Some(SideRow::Branch(b)) => {
                            if let Some(br) = self.refs.branches.get(b)
                                && !br.is_head
                            {
                                let name = br.name.clone();
                                self.run_cmd(Cmd::Checkout(name));
                            }
                        }
                        Some(SideRow::RemoteBranch(b)) => {
                            if let Some(rb) = self.refs.remote_branches.get(b) {
                                self.run_cmd(Cmd::CheckoutRemote(
                                    rb.remote.clone(),
                                    rb.name.clone(),
                                ));
                            }
                        }
                        Some(SideRow::Stash(s)) => {
                            if let Some(st) = self.refs.stashes.get(s) {
                                self.run_cmd(Cmd::StashApply(st.id, false));
                            }
                        }
                        _ => {}
                    }
                } else if let Some(SideRow::Header { key, .. }) = self.sidebar.rows.get(i).cloned()
                {
                    self.run_cmd(Cmd::ToggleSideSection(SideKey::Header(key)));
                } else if let Some(SideRow::Remote(r)) = self.sidebar.rows.get(i).cloned() {
                    self.run_cmd(Cmd::ToggleSideSection(SideKey::Remote(r)));
                }
            }
            Target::HistoryRow(i) => {
                self.focus = Focus::History;
                if shift {
                    self.start_marking();
                } else {
                    self.history.anchor = None;
                }
                self.select_history(i);
                if double {
                    self.focus = Focus::Detail;
                }
            }
            // The click was looked up in the last frame, and the row may
            // be gone since then.
            Target::DocRow(i) if i >= self.detail.rows.len() => {}
            Target::DocRow(i) => {
                self.focus = Focus::Detail;
                let row = self.detail.rows.get(i).cloned();
                match row {
                    Some(Row::Line { .. } | Row::Split { .. } | Row::Extra { .. }) => {
                        let staging = self.detail.source == Source::Workdir;
                        if shift && self.detail.anchor.is_some() {
                            self.detail.cursor = i;
                        } else {
                            self.detail.cursor = i;
                            self.detail.anchor = if staging { Some(i) } else { None };
                        }
                        if staging {
                            self.drag = Some(Drag::DiffSelect);
                        }
                    }
                    Some(
                        Row::File { .. } | Row::Section(_) | Row::Notice { .. } | Row::Meta(_),
                    ) => {
                        self.detail.cursor = i;
                        self.detail.anchor = None;
                        self.activate_doc_row(i);
                    }
                    // The header and the bottom edge of a hunk can be
                    // dragged to show more or fewer lines.
                    Some(Row::Hunk { .. } | Row::HunkEnd { .. }) => {
                        self.detail.cursor = i;
                        self.detail.anchor = None;
                        self.start_edge_drag(i, y);
                    }
                    _ => {
                        self.detail.cursor = i;
                        self.detail.anchor = None;
                    }
                }
            }
            Target::Button(Cmd::CloseOverlay) if self.overlays.is_empty() => self.cancel_edit(),
            Target::Button(Cmd::SubmitInput) if self.overlays.is_empty() => self.submit_edit(),
            Target::Button(Cmd::CloseOverlay) => {
                if let Some(Overlay::Input(b)) = self.overlays.pop() {
                    self.cancel_input(&b);
                }
            }
            Target::Button(Cmd::SubmitInput) => {
                if let Some(Overlay::Input(b)) = self.overlays.pop() {
                    self.submit_input(b);
                }
            }
            Target::Button(cmd) => {
                if let Some(Overlay::Confirm { .. }) = self.overlays.last() {
                    self.overlays.pop();
                }
                self.run_cmd(cmd);
            }
            Target::Message => {
                if self.detail.source != Source::Workdir {
                    self.select_history(0);
                }
                self.focus = Focus::Message;
                let a = self.areas.message;
                self.message
                    .click(x.saturating_sub(a.x), y.saturating_sub(a.y));
            }
            Target::Edit => {
                let a = self.areas.message;
                if let Some(e) = &mut self.edit {
                    e.input
                        .text
                        .click(x.saturating_sub(a.x), y.saturating_sub(a.y));
                }
            }
            Target::MenuItem(i) => {
                if let Some(Overlay::Menu(m)) = self.overlays.last() {
                    let item = m.items.get(i).cloned();
                    if let Some(a) = item.filter(|a| a.enabled) {
                        self.overlays.pop();
                        self.run_cmd(a.cmd);
                    }
                }
            }
            Target::PaletteItem(i) => {
                if let Some(Overlay::Palette { items, .. }) = self.overlays.last() {
                    let cmd = items.get(i).map(|a| a.cmd.clone());
                    self.overlays.pop();
                    if let Some(c) = cmd {
                        self.run_cmd(c);
                    }
                }
            }
            Target::PlannerRow(i) => {
                if let Some(p) = &mut self.planner {
                    p.selected = i;
                }
                self.drag = Some(Drag::Planner(i));
            }
            Target::PlannerAction(i) => {
                if let Some(p) = &mut self.planner {
                    p.selected = i;
                }
                self.open_plan_action_menu(x, y);
            }
            Target::InputText => {
                let a = self.areas.input;
                if let Some(Overlay::Input(b)) = self.overlays.last_mut() {
                    b.text.click(x.saturating_sub(a.x), y.saturating_sub(a.y));
                }
            }
            Target::Overlay => {}
        }
    }

    /// Select the row under the mouse and open its menu. Unlike a left
    /// click, this doesn't fold anything, follow links or press buttons.
    fn right_click(&mut self, target: Option<&Target>, x: u16, y: u16) {
        if !self.overlays.is_empty() {
            // Outside the top dialog, both buttons do the same.
            if target.is_none() {
                self.click(None, x, y, false, false);
            }
            return;
        }
        if self.edit.is_some() {
            return;
        }
        match target {
            Some(&(Target::PlannerRow(i) | Target::PlannerAction(i))) => {
                if let Some(p) = &mut self.planner {
                    p.selected = i;
                }
                self.open_plan_action_menu(x, y);
                return;
            }
            // The panes behind the planner have no menu while it's open.
            _ if self.planner.is_some() => return,
            Some(&Target::SidebarRow(i)) => {
                self.focus = Focus::Sidebar;
                self.sidebar_select(i);
            }
            Some(&Target::HistoryRow(i)) => {
                self.focus = Focus::History;
                // A click inside the selected commits is about those commits.
                let marked = self.history.marked_rows();
                if !marked.is_some_and(|(a, b)| (a..=b).contains(&i)) {
                    self.history.anchor = None;
                    self.select_history(i);
                }
            }
            Some(&Target::DocRow(i)) if i >= self.detail.rows.len() => return,
            Some(&Target::DocRow(i)) => {
                self.focus = Focus::Detail;
                // A click inside the selected lines is about those lines.
                let in_selection = self
                    .detail
                    .selection()
                    .is_some_and(|(a, b)| (a..=b).contains(&i));
                if !in_selection {
                    self.detail.cursor = i;
                    self.detail.anchor = None;
                }
            }
            _ => return,
        }
        self.open_context_menu(Some((x, y)));
    }

    fn open_plan_action_menu(&mut self, x: u16, y: u16) {
        let keys = ['p', 'r', 's', 'f', 'd', 'e'];
        let items = PlanAction::ALL
            .iter()
            .zip(keys)
            .map(|(a, k)| ActionDef {
                key: Some(super::cmd::Key::ch(k)),
                label: a.name().to_string(),
                cmd: Cmd::PlanSet(*a),
                enabled: true,
                via: super::cmd::Via::Direct,
                menu: true,
                hint: false,
            })
            .collect();
        self.overlays.push(Overlay::Menu(super::cmd::Menu {
            title: None,
            items,
            selected: 0,
            anchor: Some((x, y)),
            scroll: 0,
            preview: None,
            help: false,
        }));
    }

    fn drag_to(&mut self, x: u16, y: u16) {
        match self.drag.clone() {
            Some(Drag::Splitter(Splitter::Sidebar)) => {
                // `clamp` panics when the upper bound is below the lower one.
                let max = (self.size.0 / 2).max(12);
                // Keep the sidebar's right border under the pointer.
                let w = x.saturating_sub(self.areas.body.x + 1);
                self.sidebar_width = w.clamp(12, max);
            }
            Some(Drag::Splitter(Splitter::History)) => {
                let b = self.areas.body;
                let left = self.areas.sidebar_frame.width;
                let avail = b.width.saturating_sub(left).max(1);
                let rel = x.saturating_sub(b.x + left);
                self.history_frac = (f32::from(rel) / f32::from(avail)).clamp(0.15, 0.85);
            }
            Some(Drag::DiffSelect) => {
                let a = self.areas.doc;
                if a.height == 0 {
                    return;
                }
                // Dragging past the edge scrolls.
                if y < a.y {
                    self.detail.scroll = self.detail.scroll.saturating_sub(1);
                } else if y >= a.y + a.height {
                    self.detail.scroll =
                        (self.detail.scroll + 1).min(self.detail.rows.len().saturating_sub(1));
                }
                let rel = usize::from(y.clamp(a.y, a.y + a.height - 1) - a.y);
                let row = (self.detail.scroll + rel).min(self.detail.rows.len().saturating_sub(1));
                self.detail.cursor = row;
            }
            Some(Drag::HunkEdge { .. }) => self.drag_edge(y),
            Some(Drag::Planner(from)) => {
                if let Some(Target::PlannerRow(to) | Target::PlannerAction(to)) =
                    self.target_at(x, y)
                    && to != from
                    && let Some(p) = &mut self.planner
                {
                    p.move_row(from, to);
                    self.drag = Some(Drag::Planner(to));
                }
            }
            None => {}
        }
    }

    fn scroll_at(&mut self, x: u16, y: u16, delta: isize) {
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let apply = |v: &mut usize, max: usize| {
            *v = ((*v as isize + delta).max(0) as usize).min(max);
        };
        if let Some(top) = self.overlays.last_mut() {
            match top {
                Overlay::Log { scroll } | Overlay::Message { scroll, .. } => {
                    apply(scroll, usize::MAX / 2);
                }
                Overlay::Menu(m) => apply(&mut m.scroll, m.items.len()),
                _ => {}
            }
            return;
        }
        if self.planner.is_some() {
            if let Some(p) = &mut self.planner {
                apply(&mut p.scroll, p.rows.len().saturating_sub(1));
            }
            return;
        }
        if inside(self.areas.sidebar_frame) {
            let max = self
                .sidebar
                .rows
                .len()
                .saturating_sub(usize::from(self.areas.sidebar.height));
            apply(&mut self.sidebar.scroll, max);
        } else if inside(self.areas.history_frame) {
            let max = (self.history.row_count() * usize::from(HISTORY_ROW_HEIGHT))
                .saturating_sub(usize::from(self.areas.history.height));
            apply(&mut self.history.scroll, max);
        } else if inside(self.areas.detail_frame) {
            let max = self
                .detail
                .rows
                .len()
                .saturating_sub(usize::from(self.areas.doc.height / 2));
            apply(&mut self.detail.scroll, max);
        }
    }
}

/// The command for a key pressed in a pane. Only actions whose keys work
/// by themselves count. The others need `.` or Esc first, which open a
/// menu where their keys work.
fn find_action(actions: &[ActionDef], k: &KeyEvent) -> Option<Cmd> {
    actions
        .iter()
        .find(|a| a.enabled && a.via == Via::Direct && a.key.is_some_and(|key| key.matches(k)))
        .map(|a| a.cmd.clone())
}

/// Indexes of palette items matching `query`, best first.
pub fn palette_filter(items: &[ActionDef], query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, a)| crate::util::fuzzy_score(query.trim(), &a.label).map(|s| (s, i)))
        .collect();
    scored.sort();
    scored.into_iter().map(|(_, i)| i).collect()
}
