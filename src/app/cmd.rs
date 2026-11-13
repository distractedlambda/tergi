//! Commands and the actions that trigger them.
//!
//! Each pane describes its actions as a list of `ActionDef`s built from the
//! current selection. The same list drives key handling, the right-click
//! menu, the `?` help menu, the hint bar and the command palette.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gix::ObjectId;
use tokio::sync::oneshot;

use super::doc::{Row, Section, Source};
use super::planner::{PlanAction, Planner};
use super::{After, App, Focus, Level, Suspend};
use super::input::Target;
use crate::git::commit::FileTexts;
use crate::git::diff::FileDiff;
use crate::git::ops::{self, ApplyMode, ResetMode};
use crate::git::rebase::{self, Action, Outcome, RepoState};
use crate::git::refs::Refs;
use crate::ui::textarea::TextArea;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    Section(Section),
    File(Section, usize),
    Hunk(Section, usize, usize),
    Lines(Section, usize, Vec<(usize, usize)>),
}

impl Unit {
    pub fn section(&self) -> Section {
        match self {
            Unit::Section(s) | Unit::File(s, _) | Unit::Hunk(s, ..) | Unit::Lines(s, ..) => *s,
        }
    }

    fn noun(&self) -> &'static str {
        match self {
            Unit::Section(_) => "all",
            Unit::File(..) => "file",
            Unit::Hunk(..) => "hunk",
            Unit::Lines(_, _, l) if l.len() == 1 => "line",
            Unit::Lines(..) => "lines",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullMode {
    Default,
    FfOnly,
    Rebase,
    Merge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Quit,
    Suspend,
    Refresh,
    Help,
    Palette,
    CommandLog,
    ContextMenu,
    ToggleSidebar,
    ToggleSplit,
    ChooseTheme,
    SetTheme(&'static str),
    FocusNext,
    FocusPrev,
    Focus(Focus),
    ShowWorkdir,
    Confirmed(Box<Cmd>),
    /// Close the top dialog. Used by Cancel and OK buttons.
    CloseOverlay,
    SubmitInput,

    Fetch,
    Pull(PullMode),
    PullMenu,
    Push { force: bool },
    PushMenu,

    Reword(ObjectId),
    Squash(ObjectId),
    Fixup(ObjectId),
    Drop(ObjectId),
    Edit(ObjectId),
    Move(ObjectId, bool),
    Planner(ObjectId),
    CheckoutCommit(ObjectId),
    NewBranch(String),
    NewTag(ObjectId),
    CherryPick(ObjectId),
    Revert(ObjectId),
    ResetMenu(ObjectId),
    Reset(ObjectId, ResetMode),
    CopyHash(ObjectId),
    AmendInto(ObjectId),
    JumpTo(ObjectId),

    Stage(Unit),
    Unstage(Unit),
    Discard(Unit),
    StageAll,
    UnstageAll,
    Commit,
    CommitAll,
    ToggleAmend,
    FocusMessage,
    StashPush,
    ToggleFold,
    FoldAll,
    LoadFull,
    OpenInEditor(String),
    TakeSide(String, bool),
    MarkResolved(String),
    StartSelect,
    NextHunk(bool),
    NextFile(bool),

    Checkout(String),
    CheckoutRemote(String),
    RenameBranch(String),
    DeleteBranch(String, bool),
    DeleteRemoteBranch(String, String),
    SetUpstream(String),
    Merge(String),
    RebaseOnto(String),
    DeleteTag(String),
    PushTag(String),
    StashApply(ObjectId, bool),
    StashDrop(ObjectId),
    ToggleSideSection(String),

    ContinueOp,
    AbortOp,
    SkipOp,

    PlanSet(PlanAction),
    PlanMove(bool),
    PlanStart,
    PlanCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    pub const fn ch(c: char) -> Key {
        Key { code: KeyCode::Char(c), ctrl: false, alt: false }
    }
    pub const fn ctrl(c: char) -> Key {
        Key { code: KeyCode::Char(c), ctrl: true, alt: false }
    }
    pub const fn alt(c: char) -> Key {
        Key { code: KeyCode::Char(c), ctrl: false, alt: true }
    }
    pub const fn code(code: KeyCode) -> Key {
        Key { code, ctrl: false, alt: false }
    }

    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        let code = match ev.code {
            // Some terminals report Shift+Tab as Tab with Shift.
            KeyCode::Tab if ev.modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            c => c,
        };
        code == self.code && ctrl == self.ctrl && alt == self.alt
    }

    pub fn label(&self) -> String {
        let base = match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "shift+tab".into(),
            KeyCode::Up => "↑".into(),
            KeyCode::Down => "↓".into(),
            KeyCode::F(n) => format!("F{n}"),
            other => format!("{other:?}").to_lowercase(),
        };
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl+");
        }
        if self.alt {
            s.push_str("alt+");
        }
        s.push_str(&base);
        s
    }
}

#[derive(Debug, Clone)]
pub struct ActionDef {
    pub key: Option<Key>,
    pub label: String,
    pub cmd: Cmd,
    pub enabled: bool,
    /// Show in the right-click menu.
    pub menu: bool,
    /// Show in the hint bar at the bottom.
    pub hint: bool,
}

fn act(key: Option<Key>, label: impl Into<String>, cmd: Cmd) -> ActionDef {
    ActionDef {
        key,
        label: label.into(),
        cmd,
        enabled: true,
        menu: true,
        hint: false,
    }
}

impl ActionDef {
    fn hint(mut self) -> Self {
        self.hint = true;
        self
    }
    fn no_menu(mut self) -> Self {
        self.menu = false;
        self
    }
    fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

pub struct Menu {
    pub title: Option<String>,
    pub items: Vec<ActionDef>,
    pub selected: usize,
    pub anchor: (u16, u16),
    pub scroll: usize,
    /// When set, moving the selection runs the selected item's command
    /// right away, and closing the menu without choosing runs this one to
    /// undo that.
    pub preview: Option<Cmd>,
}

pub enum Purpose {
    NewBranch { at: String },
    RenameBranch(String),
    NewTag(ObjectId),
    Reword(ObjectId),
    Squash(ObjectId),
    StashPush,
    SetUpstream(String),
    PlannerMessage(usize),
    AskPass(oneshot::Sender<Option<String>>),
}

pub struct InputBox {
    pub title: String,
    pub hint: String,
    pub text: TextArea,
    pub purpose: Purpose,
}

pub enum Overlay {
    Menu(Menu),
    Confirm {
        title: String,
        body: String,
        yes: Cmd,
        yes_label: String,
    },
    Input(InputBox),
    Palette {
        input: TextArea,
        items: Vec<ActionDef>,
        selected: usize,
    },
    Log {
        scroll: usize,
    },
    Message {
        title: String,
        body: String,
        scroll: usize,
        error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideRow {
    Header { key: String, title: String, count: usize },
    Branch(usize),
    Remote(String),
    RemoteBranch(usize),
    Tag(usize),
    Stash(usize),
    Empty(String),
}

pub fn sidebar_rows(refs: &Refs, collapsed: &HashSet<String>) -> Vec<SideRow> {
    let mut rows = Vec::new();
    let header = |rows: &mut Vec<SideRow>, key: &str, title: &str, count: usize| {
        rows.push(SideRow::Header {
            key: key.into(),
            title: title.into(),
            count,
        });
        !collapsed.contains(key)
    };
    if header(&mut rows, "branches", "Branches", refs.branches.len()) {
        if refs.branches.is_empty() {
            rows.push(SideRow::Empty("No branches yet".into()));
        }
        rows.extend((0..refs.branches.len()).map(SideRow::Branch));
    }
    if header(&mut rows, "remotes", "Remotes", refs.remotes.len()) {
        for r in &refs.remotes {
            rows.push(SideRow::Remote(r.clone()));
            if !collapsed.contains(&format!("remote:{r}")) {
                rows.extend(
                    refs.remote_branches
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| &b.remote == r)
                        .map(|(i, _)| SideRow::RemoteBranch(i)),
                );
            }
        }
    }
    if header(&mut rows, "tags", "Tags", refs.tags.len()) {
        let mut idx: Vec<usize> = (0..refs.tags.len()).collect();
        // Newest-looking versions first.
        idx.reverse();
        rows.extend(idx.into_iter().map(SideRow::Tag));
    }
    if header(&mut rows, "stashes", "Stashes", refs.stashes.len()) {
        rows.extend((0..refs.stashes.len()).map(SideRow::Stash));
    }
    rows
}

/// Which list of actions applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Sidebar,
    History,
    Detail,
    Message,
    Planner,
}

impl App {
    pub fn context(&self) -> Context {
        if self.planner.is_some() {
            return Context::Planner;
        }
        match self.focus {
            Focus::Sidebar => Context::Sidebar,
            Focus::History => Context::History,
            Focus::Detail => Context::Detail,
            Focus::Message => Context::Message,
        }
    }

    pub fn global_actions(&self) -> Vec<ActionDef> {
        let mut v = vec![
            act(Some(Key::ch('?')), "Keyboard shortcuts", Cmd::Help).hint().no_menu(),
            act(Some(Key::ctrl('p')), "Command palette", Cmd::Palette).no_menu(),
            act(Some(Key::ch('F')), "Fetch", Cmd::Fetch).no_menu(),
            act(Some(Key::ch('p')), "Pull", Cmd::PullMenu).no_menu(),
            act(Some(Key::ch('P')), "Push", Cmd::PushMenu).no_menu(),
            act(Some(Key::ch('S')), "Stash changes", Cmd::StashPush).no_menu(),
            act(Some(Key::ch('t')), "Toggle side-by-side diff", Cmd::ToggleSplit).no_menu(),
            act(None, "Choose theme…", Cmd::ChooseTheme).no_menu(),
            act(Some(Key::ch('w')), "Show working directory", Cmd::ShowWorkdir).no_menu(),
            act(Some(Key::ch('@')), "Command log", Cmd::CommandLog).no_menu(),
            act(Some(Key::ctrl('b')), "Toggle sidebar", Cmd::ToggleSidebar).no_menu(),
            act(Some(Key::ctrl('r')), "Refresh", Cmd::Refresh).no_menu(),
            act(Some(Key::code(KeyCode::Tab)), "Next pane", Cmd::FocusNext).no_menu(),
            act(Some(Key::code(KeyCode::BackTab)), "Previous pane", Cmd::FocusPrev).no_menu(),
            act(Some(Key::ch('1')), "Focus sidebar", Cmd::Focus(Focus::Sidebar)).no_menu(),
            act(Some(Key::ch('2')), "Focus history", Cmd::Focus(Focus::History)).no_menu(),
            act(Some(Key::ch('3')), "Focus details", Cmd::Focus(Focus::Detail)).no_menu(),
            act(Some(Key::ch('.')), "Open context menu", Cmd::ContextMenu).no_menu(),
            act(Some(Key::ctrl('z')), "Suspend", Cmd::Suspend).no_menu(),
            act(Some(Key::ch('q')), "Quit", Cmd::Quit).no_menu(),
        ];
        if self.repo_state != RepoState::Clean {
            let skip = !matches!(self.repo_state, RepoState::Merge);
            v.insert(0, act(Some(Key::alt('c')), "Continue", Cmd::ContinueOp).hint().no_menu());
            if skip {
                v.insert(1, act(Some(Key::alt('s')), "Skip commit", Cmd::SkipOp).no_menu());
            }
            v.insert(1, act(Some(Key::alt('a')), "Abort", Cmd::AbortOp).hint().no_menu());
        }
        v
    }

    pub fn actions(&self, ctx: Context) -> Vec<ActionDef> {
        match ctx {
            Context::Sidebar => self.sidebar_actions(),
            Context::History => self.history_actions(),
            Context::Detail => self.detail_actions(),
            Context::Message => vec![
                act(Some(Key { code: KeyCode::Enter, ctrl: true, alt: false }), "Commit", Cmd::Commit).hint(),
                act(Some(Key::alt('a')), "Toggle amend", Cmd::ToggleAmend).hint(),
                act(Some(Key::code(KeyCode::Esc)), "Leave message box", Cmd::Focus(Focus::Detail)).hint(),
            ],
            Context::Planner => vec![
                act(Some(Key::ch('p')), "Pick", Cmd::PlanSet(PlanAction::Pick)).hint(),
                act(Some(Key::ch('r')), "Reword", Cmd::PlanSet(PlanAction::Reword)).hint(),
                act(Some(Key::ch('s')), "Squash into older", Cmd::PlanSet(PlanAction::Squash)).hint(),
                act(Some(Key::ch('f')), "Fixup into older", Cmd::PlanSet(PlanAction::Fixup)).hint(),
                act(Some(Key::ch('d')), "Drop", Cmd::PlanSet(PlanAction::Drop)).hint(),
                act(Some(Key::ch('e')), "Edit (stop here)", Cmd::PlanSet(PlanAction::Edit)).hint(),
                act(Some(Key::ctrl('k')), "Move up (newer)", Cmd::PlanMove(true)).hint(),
                act(Some(Key::ctrl('j')), "Move down (older)", Cmd::PlanMove(false)).hint(),
                act(Some(Key::code(KeyCode::Enter)), "Start rebase", Cmd::PlanStart).hint().no_menu(),
                act(Some(Key::code(KeyCode::Esc)), "Cancel", Cmd::PlanCancel).hint().no_menu(),
            ],
        }
    }

    fn history_actions(&self) -> Vec<ActionDef> {
        let Some(c) = self.history.selected_commit() else {
            return vec![
                act(Some(Key::ch('c')), "Write commit message", Cmd::FocusMessage).hint(),
                act(Some(Key::ch('a')), "Stage all", Cmd::StageAll).hint(),
                act(Some(Key::ch('u')), "Unstage all", Cmd::UnstageAll),
                act(None, "Stash changes", Cmd::StashPush),
            ];
        };
        let id = c.id;
        let has_parent = !c.parents.is_empty();
        let merge = c.parents.len() > 1;
        let staged = self.workdir.as_ref().is_some_and(|w| w.status.has_staged());
        vec![
            act(Some(Key::ch('r')), "Reword", Cmd::Reword(id)).hint().enabled(!merge),
            act(Some(Key::ch('s')), "Squash into parent", Cmd::Squash(id)).hint().enabled(has_parent && !merge),
            act(Some(Key::ch('f')), "Fixup into parent", Cmd::Fixup(id)).enabled(has_parent && !merge),
            act(Some(Key::ch('e')), "Edit commit", Cmd::Edit(id)).enabled(!merge),
            act(Some(Key::ch('d')), "Drop commit", Cmd::Drop(id)).hint().enabled(!merge),
            act(Some(Key::ctrl('k')), "Move up", Cmd::Move(id, true)).enabled(!merge),
            act(Some(Key::ctrl('j')), "Move down", Cmd::Move(id, false)).enabled(has_parent && !merge),
            act(Some(Key::ch('i')), "Edit history from here…", Cmd::Planner(id)).hint().enabled(!merge),
            act(Some(Key::ch('A')), "Amend staged changes into this commit", Cmd::AmendInto(id)).enabled(staged && !merge),
            act(Some(Key::ch('b')), "New branch here…", Cmd::NewBranch(id.to_string())),
            act(Some(Key::ch('T')), "New tag here…", Cmd::NewTag(id)),
            act(Some(Key::ch('c')), "Check out this commit", Cmd::CheckoutCommit(id)),
            act(Some(Key::ch('C')), "Cherry-pick", Cmd::CherryPick(id)),
            act(Some(Key::ch('V')), "Revert", Cmd::Revert(id)),
            act(Some(Key::ch('X')), "Reset current branch to here…", Cmd::ResetMenu(id)),
            act(Some(Key::ch('y')), "Copy commit hash", Cmd::CopyHash(id)),
        ]
    }

    fn detail_actions(&self) -> Vec<ActionDef> {
        let nav = [
            act(Some(Key::ch(']')), "Next file", Cmd::NextFile(true)).no_menu(),
            act(Some(Key::ch('[')), "Previous file", Cmd::NextFile(false)).no_menu(),
            act(Some(Key::ch('}')), "Next hunk", Cmd::NextHunk(true)).no_menu(),
            act(Some(Key::ch('{')), "Previous hunk", Cmd::NextHunk(false)).no_menu(),
            act(Some(Key::ch('z')), "Fold or unfold file", Cmd::ToggleFold),
            act(Some(Key::ch('Z')), "Fold or unfold all files", Cmd::FoldAll),
        ];
        match self.detail.source.clone() {
            Source::Workdir => {
                let mut v = Vec::new();
                let unit = self.detail.unit_at_cursor(|s| self.section_files(s));
                let row = self.detail.rows.get(self.detail.cursor);
                if let Some(u) = &unit {
                    let sec = u.section();
                    let noun = u.noun();
                    match sec {
                        Section::Staged => {
                            v.push(act(Some(Key::ch(' ')), format!("Unstage {noun}"), Cmd::Unstage(u.clone())).hint());
                        }
                        Section::Conflicted => {}
                        _ => {
                            v.push(act(Some(Key::ch(' ')), format!("Stage {noun}"), Cmd::Stage(u.clone())).hint());
                            v.push(act(Some(Key::ch('d')), format!("Discard {noun}"), Cmd::Discard(u.clone())).hint());
                        }
                    }
                }
                if let Some((sec, fi)) = row.and_then(Row::file) {
                    let path = self.section_files(sec).get(fi).map(|f| f.path().to_string());
                    if let Some(p) = path {
                        if sec == Section::Conflicted {
                            v.push(act(Some(Key::ch('O')), "Resolve using ours", Cmd::TakeSide(p.clone(), true)));
                            v.push(act(Some(Key::ch('T')), "Resolve using theirs", Cmd::TakeSide(p.clone(), false)));
                            v.push(act(Some(Key::ch('M')), "Mark as resolved", Cmd::MarkResolved(p.clone())).hint());
                        }
                        v.push(act(Some(Key::ch('o')), "Open in editor", Cmd::OpenInEditor(p)));
                    }
                }
                v.push(act(Some(Key::ch('v')), "Select lines", Cmd::StartSelect).hint().no_menu());
                v.push(act(Some(Key::ch('c')), "Write commit message", Cmd::FocusMessage).hint());
                v.push(act(Some(Key::ch('a')), "Stage all", Cmd::StageAll));
                v.push(act(Some(Key::ch('u')), "Unstage all", Cmd::UnstageAll));
                v.push(act(Some(Key::ch('A')), "Toggle amend", Cmd::ToggleAmend));
                v.extend(nav);
                v
            }
            Source::Commit(id) | Source::Stash(_, id) => {
                let mut v = vec![act(Some(Key::ch('y')), "Copy commit hash", Cmd::CopyHash(id))];
                if let Some((sec, fi)) = self.detail.rows.get(self.detail.cursor).and_then(Row::file)
                    && let Some(f) = self.section_files(sec).get(fi)
                    && f.new_path.is_some()
                {
                    v.push(act(Some(Key::ch('o')), "Open file in editor", Cmd::OpenInEditor(f.path().to_string())));
                }
                v.extend(nav);
                v
            }
            Source::None => nav.to_vec(),
        }
    }

    fn sidebar_actions(&self) -> Vec<ActionDef> {
        let Some(row) = self.sidebar.rows.get(self.sidebar.selected) else {
            return Vec::new();
        };
        let refs = &self.refs;
        let current = refs.head_branch.clone();
        match row {
            SideRow::Header { key, .. } => vec![
                act(Some(Key::code(KeyCode::Enter)), "Fold or unfold", Cmd::ToggleSideSection(key.clone())).hint(),
            ],
            SideRow::Remote(r) => vec![
                act(Some(Key::code(KeyCode::Enter)), "Fold or unfold", Cmd::ToggleSideSection(format!("remote:{r}"))).hint(),
                act(Some(Key::ch('F')), "Fetch", Cmd::Fetch),
            ],
            SideRow::Branch(i) => {
                let b = &refs.branches[*i];
                let is_current = b.is_head;
                let name = b.name.clone();
                let mut v = vec![
                    act(Some(Key::ch(' ')), "Check out", Cmd::Checkout(name.clone())).hint().enabled(!is_current),
                    act(Some(Key::ch('n')), "New branch from here…", Cmd::NewBranch(name.clone())).hint(),
                    act(Some(Key::ch('R')), "Rename…", Cmd::RenameBranch(name.clone())),
                    act(Some(Key::ch('d')), "Delete", Cmd::DeleteBranch(name.clone(), false)).hint().enabled(!is_current),
                    act(Some(Key::ch('D')), "Force delete", Cmd::DeleteBranch(name.clone(), true)).enabled(!is_current),
                    act(Some(Key::ch('u')), "Set upstream…", Cmd::SetUpstream(name.clone())),
                ];
                if let Some(cur) = &current
                    && !is_current
                {
                    v.push(act(Some(Key::ch('M')), format!("Merge into {cur}"), Cmd::Merge(name.clone())));
                    v.push(act(Some(Key::ch('r')), format!("Rebase {cur} onto this"), Cmd::RebaseOnto(name)));
                }
                v
            }
            SideRow::RemoteBranch(i) => {
                let rb = &refs.remote_branches[*i];
                let full = rb.full_name();
                let mut v = vec![
                    act(Some(Key::ch(' ')), "Check out", Cmd::CheckoutRemote(full.clone())).hint(),
                    act(Some(Key::ch('n')), "New branch from here…", Cmd::NewBranch(full.clone())).hint(),
                    act(Some(Key::ch('d')), "Delete on remote", Cmd::DeleteRemoteBranch(rb.remote.clone(), rb.name.clone())),
                ];
                if let Some(cur) = &current {
                    v.push(act(Some(Key::ch('M')), format!("Merge into {cur}"), Cmd::Merge(full.clone())));
                    v.push(act(Some(Key::ch('r')), format!("Rebase {cur} onto this"), Cmd::RebaseOnto(full)));
                }
                v
            }
            SideRow::Tag(i) => {
                let t = &refs.tags[*i];
                vec![
                    act(Some(Key::ch('d')), "Delete tag", Cmd::DeleteTag(t.name.clone())).hint(),
                    act(None, "Push tag", Cmd::PushTag(t.name.clone())),
                    act(Some(Key::ch('n')), "New branch from here…", Cmd::NewBranch(t.name.clone())),
                ]
            }
            SideRow::Stash(i) => {
                let id = refs.stashes[*i].id;
                vec![
                    act(Some(Key::ch(' ')), "Apply", Cmd::StashApply(id, false)).hint(),
                    act(Some(Key::ch('g')), "Pop", Cmd::StashApply(id, true)).hint(),
                    act(Some(Key::ch('d')), "Drop", Cmd::StashDrop(id)).hint(),
                ]
            }
            SideRow::Empty(_) => Vec::new(),
        }
    }

    /// Actions for the focused pane followed by the global ones.
    pub fn all_actions(&self) -> Vec<ActionDef> {
        let mut v = self.actions(self.context());
        for g in self.global_actions() {
            // A pane can offer a global command too, often without its key.
            // List it once, with the key.
            match v.iter_mut().find(|p| p.cmd == g.cmd) {
                Some(p) if p.key.is_none() || p.key == g.key => p.key = g.key,
                _ => v.push(g),
            }
        }
        v
    }

    fn confirm_for(&self, cmd: &Cmd) -> Option<(String, String, String)> {
        let s = |a: &str, b: String, c: &str| Some((a.to_string(), b, c.to_string()));
        match cmd {
            Cmd::Discard(u) => {
                let what = match u {
                    Unit::Section(Section::Untracked) => "all untracked files will be deleted".to_string(),
                    Unit::Section(_) => "all unstaged changes will be lost".to_string(),
                    Unit::File(sec, i) => {
                        let p = self.section_files(*sec).get(*i).map(|f| f.path().to_string()).unwrap_or_default();
                        if *sec == Section::Untracked {
                            format!("{p} will be deleted")
                        } else {
                            format!("unstaged changes to {p} will be lost")
                        }
                    }
                    Unit::Hunk(..) => "this hunk's changes will be lost".to_string(),
                    Unit::Lines(..) => "the selected changes will be lost".to_string(),
                };
                s("Discard changes?", format!("This can't be undone: {what}."), "Discard")
            }
            Cmd::Drop(id) => s("Drop commit?", format!("Commit {} will be removed from the branch.", short(id)), "Drop"),
            Cmd::Reset(id, ResetMode::Hard) => s(
                "Hard reset?",
                format!("The branch moves to {} and all uncommitted changes are lost.", short(id)),
                "Reset",
            ),
            Cmd::DeleteBranch(name, force) => s(
                "Delete branch?",
                if *force {
                    format!("{name} isn't merged. Its commits may be lost if nothing else points at them.")
                } else {
                    format!("Delete the branch {name}?")
                },
                "Delete",
            ),
            Cmd::DeleteRemoteBranch(r, b) => s("Delete remote branch?", format!("This deletes {b} on {r} for everyone."), "Delete"),
            Cmd::DeleteTag(t) => s("Delete tag?", format!("Delete the tag {t}?"), "Delete"),
            Cmd::StashDrop(id) => {
                let what = match self.refs.stashes.iter().find(|st| st.id == *id) {
                    Some(st) => format!("\"{}\"", st.message),
                    None => format!("Stash {}", short(id)),
                };
                s("Drop stash?", format!("{what} will be deleted."), "Drop")
            }
            Cmd::AbortOp => s("Abort?", "Everything done since the operation started will be undone.".into(), "Abort"),
            Cmd::Push { force: true } => s("Force push?", "Commits on the remote that you don't have will be lost. tergi uses --force-with-lease, which refuses if the remote changed since you last fetched.".into(), "Force push"),
            Cmd::CommitAll => s("Nothing is staged", "Stage all changes and commit them?".into(), "Stage all and commit"),
            _ => None,
        }
    }

    pub fn run_cmd(&mut self, cmd: Cmd) {
        let cmd = match cmd {
            Cmd::Confirmed(inner) => *inner,
            other => {
                if let Some((title, body, yes_label)) = self.confirm_for(&other) {
                    self.overlays.push(Overlay::Confirm {
                        title,
                        body,
                        yes: Cmd::Confirmed(Box::new(other)),
                        yes_label,
                    });
                    return;
                }
                other
            }
        };
        let exe = self.repo.exe.clone();
        let has_head = self
            .workdir
            .as_ref()
            .is_none_or(|w| w.status.head_oid.is_some());
        match cmd {
            Cmd::Quit => self.quit = true,
            Cmd::Suspend => self.suspend = Some(Suspend::Stop),
            Cmd::Refresh => {
                self.refresh(super::REFRESH_ALL);
                self.set_status("Refreshed", Level::Info);
            }
            Cmd::Help => {
                let items: Vec<ActionDef> = self.all_actions();
                let (w, h) = self.size;
                self.overlays.push(Overlay::Menu(Menu {
                    title: Some("Keyboard shortcuts".into()),
                    items,
                    selected: 0,
                    anchor: (w / 2, h / 4),
                    scroll: 0,
                    preview: None,
                }));
            }
            Cmd::Palette => {
                let items = self.all_actions().into_iter().filter(|a| a.enabled).collect();
                let mut input = TextArea::new(false);
                input.placeholder = "Type to search commands".into();
                self.overlays.push(Overlay::Palette { input, items, selected: 0 });
            }
            Cmd::CommandLog => self.overlays.push(Overlay::Log { scroll: usize::MAX }),
            Cmd::ContextMenu => self.open_context_menu(None),
            Cmd::ToggleSidebar => {
                self.sidebar_visible = Some(!self.sidebar_shown());
                if !self.sidebar_shown() && self.focus == Focus::Sidebar {
                    self.focus = Focus::History;
                }
            }
            Cmd::ToggleSplit => {
                self.split_diff = !self.split_diff;
                self.rebuild_doc();
            }
            Cmd::ChooseTheme => {
                let current = self.theme.id;
                let items: Vec<ActionDef> = crate::theme::Theme::list().map(|(id, name)| act(None, name, Cmd::SetTheme(id))).collect();
                let selected = items.iter().position(|a| a.cmd == Cmd::SetTheme(current)).unwrap_or(0);
                let (w, h) = self.size;
                self.overlays.push(Overlay::Menu(Menu {
                    title: Some("Theme".into()),
                    items,
                    selected,
                    anchor: (w.saturating_sub(24) / 2, h / 4),
                    scroll: 0,
                    preview: Some(Cmd::SetTheme(current)),
                }));
            }
            Cmd::SetTheme(id) => self.set_theme(id),
            Cmd::FocusNext | Cmd::FocusPrev => {
                let mut order = Vec::new();
                if self.sidebar_shown() {
                    order.push(Focus::Sidebar);
                }
                order.push(Focus::History);
                order.push(Focus::Detail);
                if self.detail.source == Source::Workdir {
                    order.push(Focus::Message);
                }
                let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let n = order.len();
                let j = if cmd == Cmd::FocusNext { (i + 1) % n } else { (i + n - 1) % n };
                self.focus = order[j];
            }
            Cmd::Focus(f) => {
                if f == Focus::Sidebar && !self.sidebar_shown() {
                    self.sidebar_visible = Some(true);
                }
                if f == Focus::Message && self.detail.source != Source::Workdir {
                    self.select_history(0);
                }
                self.focus = f;
            }
            Cmd::ShowWorkdir => {
                self.select_history(0);
            }
            Cmd::FocusMessage => {
                if self.detail.source != Source::Workdir {
                    self.select_history(0);
                }
                self.focus = Focus::Message;
            }
            Cmd::Confirmed(_) | Cmd::CloseOverlay | Cmd::SubmitInput => {}

            Cmd::Fetch => {
                let env = self.repo.remote_env();
                self.spawn_op("Fetch", After::Nothing, move |r, progress| async move {
                    r.git
                        .run_streaming(
                            &["fetch", "--all", "--prune", "--progress"],
                            &crate::git::cli::RunOpts { env, ..Default::default() },
                            |l| progress(l.to_string()),
                        ).await
                        .map(|_| "Fetched".to_string())
                });
            }
            Cmd::PullMenu => {
                let items = vec![
                    act(Some(Key::ch('p')), "Pull (use git config)", Cmd::Pull(PullMode::Default)),
                    act(Some(Key::ch('f')), "Pull, fast-forward only", Cmd::Pull(PullMode::FfOnly)),
                    act(Some(Key::ch('r')), "Pull with rebase", Cmd::Pull(PullMode::Rebase)),
                    act(Some(Key::ch('m')), "Pull with merge", Cmd::Pull(PullMode::Merge)),
                ];
                self.open_menu("Pull", items, Some(&Cmd::PullMenu));
            }
            Cmd::Pull(mode) => {
                let env = self.repo.remote_env();
                self.spawn_op("Pull", After::Nothing, move |r, progress| async move {
                    let mut args = vec!["pull", "--progress"];
                    match mode {
                        PullMode::Default => {}
                        PullMode::FfOnly => args.push("--ff-only"),
                        PullMode::Rebase => args.extend(["--rebase", "--autostash"]),
                        PullMode::Merge => args.extend(["--no-rebase", "--no-edit"]),
                    }
                    r.git
                        .run_streaming(&args, &crate::git::cli::RunOpts { env, ..Default::default() }, |l| {
                            progress(l.to_string());
                        }).await
                        .map(|_| "Pulled".to_string())
                });
            }
            Cmd::PushMenu => {
                let items = vec![
                    act(Some(Key::ch('P')), "Push", Cmd::Push { force: false }),
                    act(Some(Key::ch('f')), "Force push (with lease)", Cmd::Push { force: true }),
                ];
                self.open_menu("Push", items, Some(&Cmd::PushMenu));
            }
            Cmd::Push { force } => {
                let Some(branch) = self.refs.current_branch().cloned() else {
                    self.set_status("Can't push: no branch is checked out", Level::Error);
                    return;
                };
                let remote = if self.refs.remotes.iter().any(|r| r == "origin") {
                    Some("origin".to_string())
                } else {
                    self.refs.remotes.first().cloned()
                };
                let has_upstream = branch.upstream.is_some() && !branch.upstream_gone;
                let Some(remote) = remote else {
                    self.set_status("Can't push: this repository has no remotes", Level::Error);
                    return;
                };
                let env = self.repo.remote_env();
                self.spawn_op("Push", After::Nothing, move |r, progress| async move {
                    let mut args = vec!["push".to_string(), "--progress".to_string()];
                    if force {
                        args.push("--force-with-lease".into());
                    }
                    if !has_upstream {
                        args.extend(["-u".into(), remote, branch.name.clone()]);
                    }
                    r.git
                        .run_streaming(&args, &crate::git::cli::RunOpts { env, ..Default::default() }, |l| {
                            progress(l.to_string());
                        }).await
                        .map(|_| "Pushed".to_string())
                });
            }

            // The dialogs below open once git has answered. If another one
            // opened in the meantime, the user has moved on, so we don't.
            Cmd::Reword(id) => self.spawn_read(
                move |r| async move { ops::commit_message(&r.git, &id.to_string()).await.unwrap_or_default() },
                move |app, msg| {
                    if app.overlays.is_empty() {
                        app.open_input("Reword commit", "Ctrl+Enter to save", true, &msg, Purpose::Reword(id));
                    }
                },
            ),
            Cmd::Squash(id) => self.spawn_read(
                move |r| async move {
                    let this = ops::commit_message(&r.git, &id.to_string()).await.unwrap_or_default();
                    let parent = ops::commit_message(&r.git, &format!("{id}^")).await.unwrap_or_default();
                    format!("{parent}\n\n{this}")
                },
                move |app, msg| {
                    if app.overlays.is_empty() {
                        app.open_input(
                            "Squash into parent",
                            "Message for the combined commit. Ctrl+Enter to save.",
                            true,
                            &msg,
                            Purpose::Squash(id),
                        );
                    }
                },
            ),
            Cmd::Fixup(id) => self.rebase_op("Fixup", move |r| async move { rebase::single(&r.git, &exe, id, Action::Fixup).await }),
            Cmd::Drop(id) => self.rebase_op("Drop commit", move |r| async move { rebase::single(&r.git, &exe, id, Action::Drop).await }),
            Cmd::Edit(id) => self.rebase_op("Edit commit", move |r| async move { rebase::single(&r.git, &exe, id, Action::Edit).await }),
            Cmd::Move(id, newer) => self.rebase_op("Move commit", move |r| async move { rebase::move_commit(&r.git, &exe, id, newer).await }),
            Cmd::Planner(id) => self.spawn_read(
                move |r| async move { rebase::plan_from(&r.git, id).await },
                |app, plan| match plan {
                    Ok(plan) => {
                        if app.planner.is_none() && app.overlays.is_empty() {
                            app.planner = Some(Planner::new(plan));
                        }
                    }
                    Err(e) => app.show_error("Can't edit history here", &e),
                },
            ),
            Cmd::CheckoutCommit(id) => {
                let s = id.to_string();
                self.spawn_op("Checkout", After::Nothing, move |r, _| async move {
                    ops::checkout(&r.git, &s).await.map(|()| "Checked out (detached HEAD)".into())
                });
            }
            Cmd::NewBranch(at) => {
                self.open_input("New branch", &format!("Created at {} and checked out", short_name(&at)), false, "", Purpose::NewBranch { at });
            }
            Cmd::NewTag(id) => self.open_input("New tag", &format!("At {}", short(&id)), false, "", Purpose::NewTag(id)),
            Cmd::CherryPick(id) => {
                let s = id.to_string();
                self.spawn_op("Cherry-pick", After::Nothing, move |r, _| async move {
                    ops::cherry_pick(&r.git, &s).await.map(|()| "Cherry-picked".into())
                });
            }
            Cmd::Revert(id) => {
                let s = id.to_string();
                self.spawn_op("Revert", After::Nothing, move |r, _| async move { ops::revert(&r.git, &s).await.map(|()| "Reverted".into()) });
            }
            Cmd::ResetMenu(id) => {
                let items = vec![
                    act(Some(Key::ch('s')), "Soft: keep changes staged", Cmd::Reset(id, ResetMode::Soft)),
                    act(Some(Key::ch('m')), "Mixed: keep changes unstaged", Cmd::Reset(id, ResetMode::Mixed)),
                    act(Some(Key::ch('h')), "Hard: discard all changes", Cmd::Reset(id, ResetMode::Hard)),
                ];
                self.open_menu(&format!("Reset to {}", short(&id)), items, None);
            }
            Cmd::Reset(id, mode) => {
                let s = id.to_string();
                self.spawn_op("Reset", After::Nothing, move |r, _| async move { ops::reset(&r.git, &s, mode).await.map(|()| "Reset".into()) });
            }
            Cmd::CopyHash(id) => self.copy_to_clipboard(&id.to_string()),
            Cmd::AmendInto(id) => self.rebase_op("Amend into commit", move |r| async move { rebase::fixup_staged_into(&r.git, id).await }),
            Cmd::JumpTo(id) => {
                if !self.jump_to(id) {
                    self.set_status("That commit isn't loaded in the history yet", Level::Info);
                }
            }

            Cmd::Stage(u) if u.section() == Section::Staged => self.run_cmd(Cmd::Unstage(u)),
            Cmd::Stage(u) => self.apply_unit(u, ApplyMode::Stage, has_head),
            Cmd::Unstage(u) => self.apply_unit(u, ApplyMode::Unstage, has_head),
            Cmd::Discard(u) => self.apply_unit(u, ApplyMode::Discard, has_head),
            Cmd::StageAll => self.spawn_op("Stage all", After::Nothing, move |r, _| async move { ops::stage_all(&r.git).await.map(|()| "Staged everything".into()) }),
            Cmd::UnstageAll => self.spawn_op("Unstage all", After::Nothing, move |r, _| async move {
                ops::unstage_all(&r.git, has_head).await.map(|()| "Unstaged everything".into())
            }),
            Cmd::Commit | Cmd::CommitAll => {
                let all = cmd == Cmd::CommitAll;
                let msg = self.message.text();
                if msg.trim().is_empty() {
                    self.set_status("Write a commit message first", Level::Warn);
                    self.focus = Focus::Message;
                    if self.detail.source != Source::Workdir {
                        self.select_history(0);
                    }
                    return;
                }
                let amend = self.amend;
                if all || amend || self.workdir.as_ref().is_some_and(|w| w.status.has_staged()) {
                    self.start_commit(msg, amend, all);
                    return;
                }
                // The status we have may be from before the last stage, since
                // it reloads in the background. So before saying that nothing
                // is staged, ask git. It exits with 1 when something is.
                self.spawn_read(
                    |r| async move { r.git.read(&["diff", "--cached", "--quiet"]).await.is_err() },
                    move |app, staged| {
                        // If the message or amend changed while git answered,
                        // the user did something else in the meantime.
                        if app.amend || app.message.text() != msg {
                            return;
                        }
                        if staged {
                            app.start_commit(msg, false, false);
                        } else if app.workdir.as_ref().is_some_and(|w| !w.status.is_clean()) {
                            app.run_cmd(Cmd::CommitAll);
                        } else {
                            app.set_status("There's nothing to commit", Level::Warn);
                        }
                    },
                );
            }
            Cmd::ToggleAmend => self.toggle_amend(),
            Cmd::StashPush => self.open_input("Stash changes", "Optional message. Untracked files are included.", false, "", Purpose::StashPush),
            Cmd::ToggleFold => self.toggle_fold(),
            Cmd::FoldAll => self.fold_all(),
            Cmd::LoadFull => self.load_full(),
            Cmd::OpenInEditor(p) => self.suspend = Some(Suspend::Edit(self.repo.workdir.join(p))),
            Cmd::TakeSide(p, ours) => self.spawn_op("Resolve", After::Nothing, move |r, _| async move {
                ops::take_side(&r.git, &p, ours).await.map(|()| format!("Resolved {p}"))
            }),
            Cmd::MarkResolved(p) => self.spawn_op("Mark resolved", After::Nothing, move |r, _| async move {
                ops::stage_paths(&r.git, &[&p]).await.map(|()| format!("Marked {p} as resolved"))
            }),
            Cmd::StartSelect => {
                self.detail.anchor = match self.detail.anchor {
                    Some(_) => None,
                    None => Some(self.detail.cursor),
                };
            }
            Cmd::NextHunk(fwd) => {
                self.detail.anchor = None;
                self.detail.jump(fwd, |r| matches!(r, Row::Hunk { .. }));
            }
            Cmd::NextFile(fwd) => {
                self.detail.anchor = None;
                self.detail.jump(fwd, |r| matches!(r, Row::File { .. }));
            }

            Cmd::Checkout(b) => self.spawn_op("Checkout", After::Nothing, move |r, _| async move {
                ops::checkout(&r.git, &b).await.map(|()| format!("Switched to {b}"))
            }),
            Cmd::CheckoutRemote(full) => {
                let local = full.split_once('/').map_or(full.as_str(), |(_, b)| b).to_string();
                let exists = self.refs.branches.iter().any(|b| b.name == local);
                self.spawn_op("Checkout", After::Nothing, move |r, _| async move {
                    ops::checkout_remote(&r.git, &full, exists).await.map(|()| format!("Switched to {local}"))
                });
            }
            Cmd::RenameBranch(b) => self.open_input("Rename branch", &format!("New name for {b}"), false, &b.clone(), Purpose::RenameBranch(b)),
            Cmd::DeleteBranch(b, force) => self.spawn_op("Delete branch", After::Nothing, move |r, _| async move {
                match ops::delete_branch(&r.git, &b, force).await {
                    Ok(()) => Ok(format!("Deleted {b}")),
                    Err(e) if !force && format!("{e:#}").contains("not fully merged") => Err(anyhow::anyhow!(
                        "{b} isn't fully merged. Merge it first, or use Force delete (D) if you're sure.\n\n{e:#}"
                    )),
                    Err(e) => Err(e),
                }
            }),
            Cmd::DeleteRemoteBranch(remote, name) => {
                let env = self.repo.remote_env();
                self.spawn_op("Delete remote branch", After::Nothing, move |r, _| async move {
                    r.git
                        .run_opts(&["push", &remote, "--delete", &name], &crate::git::cli::RunOpts { env, ..Default::default() }).await
                        .map(|_| format!("Deleted {name} on {remote}"))
                });
            }
            Cmd::SetUpstream(b) => {
                let guess = format!("{}/{b}", self.refs.remotes.first().map_or("origin", String::as_str));
                self.open_input("Set upstream", &format!("Remote branch for {b} to track"), false, &guess, Purpose::SetUpstream(b));
            }
            Cmd::Merge(b) => self.spawn_op("Merge", After::Nothing, move |r, _| async move { ops::merge(&r.git, &b).await.map(|()| format!("Merged {b}")) }),
            Cmd::RebaseOnto(b) => self.rebase_op("Rebase", move |r| async move { rebase::rebase_onto(&r.git, &b).await }),
            Cmd::DeleteTag(t) => self.spawn_op("Delete tag", After::Nothing, move |r, _| async move { ops::delete_tag(&r.git, &t).await.map(|()| format!("Deleted tag {t}")) }),
            Cmd::PushTag(t) => {
                let remote = self.refs.remotes.first().cloned().unwrap_or_else(|| "origin".into());
                let env = self.repo.remote_env();
                self.spawn_op("Push tag", After::Nothing, move |r, p| async move {
                    r.git
                        .run_streaming(&["push", "--progress", &remote, &format!("refs/tags/{t}")], &crate::git::cli::RunOpts { env, ..Default::default() }, |l| p(l.to_string())).await
                        .map(|_| format!("Pushed {t}"))
                });
            }
            Cmd::StashApply(id, pop) => self.spawn_op(if pop { "Pop stash" } else { "Apply stash" }, After::Nothing, move |r, _| async move {
                ops::stash_apply(&r.git, id, pop).await.map(|()| if pop { "Popped stash" } else { "Applied stash" }.into())
            }),
            Cmd::StashDrop(id) => self.spawn_op("Drop stash", After::Nothing, move |r, _| async move {
                ops::stash_drop(&r.git, id).await.map(|()| "Dropped stash".into())
            }),
            Cmd::ToggleSideSection(key) => {
                if !self.sidebar.collapsed.remove(&key) {
                    self.sidebar.collapsed.insert(key);
                }
                self.rebuild_sidebar();
            }

            Cmd::ContinueOp => {
                let st = self.repo_state.clone();
                self.rebase_op("Continue", move |r| async move { rebase::continue_op(&r.git, &st).await });
            }
            Cmd::AbortOp => {
                let st = self.repo_state.clone();
                self.spawn_op("Abort", After::Nothing, move |r, _| async move { rebase::abort_op(&r.git, &st).await.map(|()| "Aborted".into()) });
            }
            Cmd::SkipOp => {
                let st = self.repo_state.clone();
                self.rebase_op("Skip", move |r| async move { rebase::skip_op(&r.git, &st).await });
            }

            Cmd::PlanSet(a) => self.planner_set(a),
            Cmd::PlanMove(up) => {
                if let Some(p) = &mut self.planner {
                    p.move_selected(up);
                }
            }
            Cmd::PlanStart => self.planner_start(),
            Cmd::PlanCancel => self.planner = None,
        }
    }

    fn start_commit(&mut self, msg: String, amend: bool, all: bool) {
        self.focus = Focus::Detail;
        self.spawn_op(if amend { "Amend" } else { "Commit" }, After::ClearMessage, move |r, _| async move {
            if all {
                ops::stage_all(&r.git).await?;
            }
            ops::commit(&r.git, &msg, amend).await
        });
    }

    fn rebase_op<F>(&mut self, label: &str, f: impl FnOnce(std::sync::Arc<super::Repo>) -> F + Send + 'static)
    where
        F: Future<Output = anyhow::Result<Outcome>> + Send + 'static,
    {
        self.spawn_op(label, After::Nothing, move |r, _| async move {
            Ok(match f(r).await? {
                Outcome::Done => "Done".to_string(),
                Outcome::Stopped => "Stopped. Make your changes, then Continue (alt+c).".to_string(),
            })
        });
    }

    /// Open a menu below the button for `from` if it's on screen, and at the
    /// mouse pointer otherwise.
    fn open_menu(&mut self, title: &str, items: Vec<ActionDef>, from: Option<&Cmd>) {
        let button = from.and_then(|cmd| {
            self.hits.iter().find_map(|(r, t)| match t {
                Target::Button(c) if c == cmd => Some((r.x, r.y + 1)),
                _ => None,
            })
        });
        let anchor = button.unwrap_or(self.mouse);
        self.overlays.push(Overlay::Menu(Menu {
            title: Some(title.to_string()),
            items,
            selected: 0,
            anchor,
            scroll: 0,
            preview: None,
        }));
    }

    /// Open the right-click menu for the focused pane, at `at` or near the
    /// selected row.
    pub fn open_context_menu(&mut self, at: Option<(u16, u16)>) {
        let items: Vec<ActionDef> = self.actions(self.context()).into_iter().filter(|a| a.menu).collect();
        if items.is_empty() {
            return;
        }
        let anchor = at.unwrap_or_else(|| {
            let a = match self.focus {
                Focus::Sidebar => (self.areas.sidebar, self.sidebar.selected.saturating_sub(self.sidebar.scroll)),
                Focus::History => (self.areas.history, self.history.current_row().saturating_sub(self.history.scroll)),
                _ => (self.areas.doc, self.detail.cursor.saturating_sub(self.detail.scroll)),
            };
            (a.0.x + 4, a.0.y + (a.1 as u16).min(a.0.height.saturating_sub(1)) + 1)
        });
        self.overlays.push(Overlay::Menu(Menu {
            title: None,
            items,
            selected: 0,
            anchor,
            scroll: 0,
            preview: None,
        }));
    }

    fn open_input(&mut self, title: &str, hint: &str, multiline: bool, initial: &str, purpose: Purpose) {
        self.overlays.push(Overlay::Input(InputBox {
            title: title.into(),
            hint: hint.into(),
            text: TextArea::with_text(multiline, initial),
            purpose,
        }));
    }

    pub fn submit_input(&mut self, b: InputBox) {
        let text = b.text.text();
        let exe = self.repo.exe.clone();
        let name = text.trim().to_string();
        match b.purpose {
            Purpose::AskPass(reply) => {
                let _ = reply.send(Some(text));
            }
            _ if name.is_empty() && !matches!(b.purpose, Purpose::StashPush) => {
                self.set_status("Nothing entered", Level::Warn);
                self.cancel_input(b);
            }
            Purpose::NewBranch { at } => self.spawn_op("New branch", After::Nothing, move |r, _| async move {
                ops::create_branch(&r.git, &name, &at, true).await.map(|()| format!("Created and switched to {name}"))
            }),
            Purpose::RenameBranch(old) => self.spawn_op("Rename branch", After::Nothing, move |r, _| async move {
                ops::rename_branch(&r.git, &old, &name).await.map(|()| format!("Renamed {old} to {name}"))
            }),
            Purpose::NewTag(id) => self.spawn_op("New tag", After::Nothing, move |r, _| async move {
                ops::create_tag(&r.git, &name, &id.to_string(), None).await.map(|()| format!("Created tag {name}"))
            }),
            Purpose::SetUpstream(branch) => self.spawn_op("Set upstream", After::Nothing, move |r, _| async move {
                ops::set_upstream(&r.git, &branch, &name).await.map(|()| format!("{branch} now tracks {name}"))
            }),
            Purpose::Reword(id) => self.rebase_op("Reword", move |r| async move { rebase::reword(&r.git, &exe, id, &text).await }),
            Purpose::Squash(id) => self.rebase_op("Squash", move |r| async move { rebase::single(&r.git, &exe, id, Action::Squash(text)).await }),
            Purpose::StashPush => self.spawn_op("Stash", After::Nothing, move |r, _| async move {
                ops::stash_push(&r.git, &name, true, false).await.map(|()| "Stashed changes".into())
            }),
            Purpose::PlannerMessage(row) => {
                if let Some(p) = &mut self.planner
                    && let Some(r) = p.rows.get_mut(row)
                {
                    r.message = Some(text);
                }
            }
        }
    }

    pub fn cancel_input(&mut self, b: InputBox) {
        match b.purpose {
            Purpose::AskPass(reply) => {
                let _ = reply.send(None);
            }
            Purpose::PlannerMessage(row) => {
                // Without a message, reword and squash make no sense.
                if let Some(p) = &mut self.planner
                    && let Some(r) = p.rows.get_mut(row)
                    && r.message.is_none()
                {
                    r.action = PlanAction::Pick;
                }
            }
            _ => {}
        }
    }

    fn apply_unit(&mut self, unit: Unit, mode: ApplyMode, has_head: bool) {
        let sec = unit.section();
        let files: Vec<FileDiff> = self.section_files(sec).to_vec();
        let (label, past) = match mode {
            ApplyMode::Stage => ("Stage", "Staged"),
            ApplyMode::Unstage => ("Unstage", "Unstaged"),
            ApplyMode::Discard => ("Discard", "Discarded"),
        };
        let noun = unit.noun();
        let done = format!("{past} {noun}");
        let paths: Vec<String> = match &unit {
            Unit::Section(_) => files.iter().map(|f| f.path().to_string()).collect(),
            Unit::File(_, i) => files.get(*i).map(|f| vec![f.path().to_string()]).unwrap_or_default(),
            _ => Vec::new(),
        };
        let file = match &unit {
            Unit::Hunk(_, i, _) | Unit::Lines(_, i, _) => files.get(*i).cloned(),
            _ => None,
        };
        self.detail.anchor = None;
        self.spawn_op(label, After::Nothing, move |r, _| async move {
            let git = &r.git;
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            match (&unit, mode) {
                (Unit::Section(Section::Staged), ApplyMode::Unstage) => ops::unstage_all(git, has_head).await?,
                (Unit::Section(_) | Unit::File(..), ApplyMode::Stage) => ops::stage_paths(git, &refs).await?,
                (Unit::Section(_) | Unit::File(..), ApplyMode::Unstage) => ops::unstage_paths(git, &refs, has_head).await?,
                (Unit::Section(s) | Unit::File(s, _), ApplyMode::Discard) => {
                    if *s == Section::Untracked {
                        ops::delete_untracked(git, &refs).await?;
                    } else {
                        ops::discard_paths(git, &refs).await?;
                    }
                }
                (Unit::Hunk(_, _, h), _) => {
                    let f = file.as_ref().ok_or_else(|| anyhow::anyhow!("file is gone"))?;
                    let h = *h;
                    ops::apply_lines(git, f, mode, &|hh, _| hh == h).await?;
                }
                (Unit::Lines(_, _, set), _) => {
                    let f = file.as_ref().ok_or_else(|| anyhow::anyhow!("file is gone"))?;
                    ops::apply_lines(git, f, mode, &|h, l| set.contains(&(h, l))).await?;
                }
            }
            Ok(done)
        });
    }

    fn toggle_fold(&mut self) {
        let row = self.detail.rows.get(self.detail.cursor).cloned();
        match row {
            Some(Row::Section(s)) => {
                if !self.detail.collapsed_sections.remove(&s) {
                    self.detail.collapsed_sections.insert(s);
                }
            }
            Some(r) => {
                if let Some((sec, fi)) = r.file()
                    && let Some(f) = self.section_files(sec).get(fi)
                {
                    let key = (sec, f.path().to_string());
                    if !self.detail.collapsed.remove(&key) {
                        self.detail.collapsed.insert(key);
                    }
                    // Keep the cursor on the file header.
                    if let Some(i) = self.detail.rows.iter().position(|r| *r == Row::File { sec, file: fi }) {
                        self.detail.cursor = i;
                    }
                }
            }
            None => {}
        }
        self.detail.anchor = None;
        self.rebuild_doc();
    }

    fn fold_all(&mut self) {
        let keys: Vec<(Section, String)> = self
            .detail
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::File { sec, file } => self.section_files(*sec).get(*file).map(|f| (*sec, f.path().to_string())),
                _ => None,
            })
            .collect();
        let all_folded = keys.iter().all(|k| self.detail.collapsed.contains(k));
        for k in keys {
            if all_folded {
                self.detail.collapsed.remove(&k);
            } else {
                self.detail.collapsed.insert(k);
            }
        }
        self.rebuild_doc();
    }

    fn load_full(&mut self) {
        let Some((sec, fi)) = self.detail.rows.get(self.detail.cursor).and_then(Row::file) else {
            return;
        };
        let Some(f) = self.section_files(sec).get(fi) else { return };
        if !f.too_large {
            return;
        }
        let ctx = self.cfg.context_lines;
        let path = f.path().to_string();
        match sec {
            Section::Unstaged | Section::Staged => {
                let f = f.clone();
                self.spawn_read(
                    move |r| async move { crate::git::workdir::load_full_diff(&r.git, &f, sec == Section::Staged, ctx).await },
                    move |app, full| match full {
                        Ok(Some(full)) => app.put_full_diff(sec, fi, &path, full, None),
                        Ok(None) => {}
                        Err(e) => app.show_error("Loading the diff failed", &e),
                    },
                );
            }
            Section::Untracked => {
                let p = path.clone();
                self.spawn_read(
                    move |r| async move { crate::git::workdir::load_full_untracked(&r.git, &p, ctx).await },
                    move |app, (full, texts)| app.put_full_diff(sec, fi, &path, full, Some(texts)),
                );
            }
            Section::Commit => {
                let Some(id) = self.detail.commit.as_ref().map(|c| c.id) else { return };
                self.spawn_read(
                    move |r| crate::util::blocking(move || crate::git::commit::load(&r.gix.to_thread_local(), id, ctx, usize::MAX)),
                    move |app, full| {
                        if app.detail.commit.as_ref().is_none_or(|c| c.id != id) {
                            return;
                        }
                        match full {
                            Ok(full) => {
                                app.detail.set_commit(std::sync::Arc::new(full));
                                app.rebuild_doc();
                            }
                            Err(e) => app.show_error("Loading the diff failed", &e),
                        }
                    },
                );
            }
            Section::Conflicted => {}
        }
    }

    /// Show the whole diff of a working directory file in place of the
    /// shortened one. The status may have reloaded while git worked, so
    /// only do it if the same file is still in the same place.
    fn put_full_diff(&mut self, sec: Section, fi: usize, path: &str, full: FileDiff, texts: Option<FileTexts>) {
        let Some(w) = &mut self.workdir else { return };
        let list = match sec {
            Section::Staged => &mut w.staged,
            Section::Unstaged => &mut w.unstaged,
            Section::Untracked => &mut w.untracked,
            Section::Commit | Section::Conflicted => return,
        };
        let Some(slot) = list.get_mut(fi) else { return };
        if slot.path() != path || !slot.too_large {
            return;
        }
        *slot = full;
        if let Some(texts) = texts
            && let Some(t) = w.untracked_texts.get_mut(fi)
        {
            *t = texts;
        }
        if self.detail.source == Source::Workdir {
            self.detail.reset_for_reload();
            self.rebuild_doc();
        }
    }
}

pub fn short(id: &ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}

fn short_name(s: &str) -> String {
    if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s[..7].to_string()
    } else {
        s.to_string()
    }
}
