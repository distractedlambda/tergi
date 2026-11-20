//! Commands and the actions that trigger them.
//!
//! Each pane describes its actions as a list of `ActionDef`s built from the
//! current selection. The same list drives key handling, the right-click
//! menu, the `?` help menu, the hint bar and the command palette.

use std::collections::HashSet;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice as _};

use super::doc::{Row, Section, Source};
use super::input::Target;
use super::planner::{PlanAction, Planner};
use super::{After, App, Focus, HISTORY_ROW_HEIGHT, Level, Suspend};
use crate::config;
use crate::git::commit::FileTexts;
use crate::git::diff::FileDiff;
use crate::git::ops::{self, ApplyMode, ResetMode};
use crate::git::rebase::{self, Action, Outcome, RepoState};
use crate::git::refs::Refs;
use crate::ui::textarea::TextArea;
use crate::util::{fs_path, os_str};

/// Part of the working directory's changes, to stage, unstage or discard.
///
/// A unit can wait in a menu or a confirmation dialog while the status
/// reloads and the file lists change. So it names a file by its path, not
/// by its place in a list. Hunks and lines keep the diff they were chosen
/// from, and the patch is built from that diff. If the file has changed
/// since then where the patch applies, `git apply` refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    Section(Section),
    File(Section, BString),
    /// Files by path. Discarding a whole section turns into this when the
    /// dialog asking about it opens. That way it only discards the files
    /// the dialog names, and not ones that show up while it's open.
    Files(Section, Vec<BString>),
    Hunk(Section, Arc<FileDiff>, usize),
    /// `(hunk, line)` indexes into the diff.
    Lines(Section, Arc<FileDiff>, Vec<(usize, usize)>),
}

impl Unit {
    pub fn section(&self) -> Section {
        match self {
            Unit::Section(s)
            | Unit::File(s, _)
            | Unit::Files(s, _)
            | Unit::Hunk(s, ..)
            | Unit::Lines(s, ..) => *s,
        }
    }

    fn noun(&self) -> &'static str {
        match self {
            Unit::Section(_) => "all",
            Unit::File(..) => "file",
            Unit::Files(_, p) if p.len() == 1 => "file",
            Unit::Files(..) => "files",
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
    /// Switch to a theme and save it.
    SetTheme(&'static str),
    /// Show a theme without saving it.
    PreviewTheme(&'static str),
    /// Go back to the saved theme.
    RevertTheme,
    ChooseContextLines,
    ChooseTabWidth,
    ToggleRelativeDates,
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
    /// Push the branch, which was checked out when the user chose this.
    Push {
        branch: BString,
        force: bool,
    },
    PushMenu,

    Reword(ObjectId),
    Squash(ObjectId),
    Fixup(ObjectId),
    Drop(ObjectId),
    Edit(ObjectId),
    Move(ObjectId, bool),
    Planner(ObjectId),
    CheckoutCommit(ObjectId),
    /// Ask for the name of a new branch at this ref or commit.
    NewBranch(BString),
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
    OpenInEditor(BString),
    TakeSide(BString, bool),
    MarkResolved(BString),
    StartSelect,
    NextHunk(bool),
    NextFile(bool),

    // Ref names needn't be UTF-8, so these keep them as bytes.
    Checkout(BString),
    CheckoutRemote(BString, BString),
    RenameBranch(BString),
    DeleteBranch(BString, bool),
    DeleteRemoteBranch(BString, BString),
    SetUpstream(BString),
    Merge(BString),
    RebaseOnto(BString),
    DeleteTag(BString),
    PushTag(BString),
    StashApply(ObjectId, bool),
    StashDrop(ObjectId),
    /// Fold or unfold a header or a remote in the sidebar.
    ToggleSideSection(SideKey),

    ContinueOp,
    AbortOp,
    SkipOp,

    PlanSet(PlanAction),
    PlanMove(bool),
    PlanStart,
    PlanCancel,
}

impl Cmd {
    /// What a menu with `Menu::preview` set runs when this item is
    /// selected but not yet chosen.
    pub fn preview(&self) -> Cmd {
        match self {
            Cmd::SetTheme(id) => Cmd::PreviewTheme(id),
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    pub const fn ch(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: false,
            alt: false,
        }
    }
    pub const fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
        }
    }
    pub const fn alt(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: false,
            alt: true,
        }
    }
    pub const fn code(code: KeyCode) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
        }
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
    /// Where the menu's top left corner goes. Menus opened from the
    /// keyboard have none and sit in the middle of the screen.
    pub anchor: Option<(u16, u16)>,
    pub scroll: usize,
    /// When set, moving the selection runs the preview of the selected
    /// item's command (`Cmd::preview`) right away, and closing the menu
    /// without choosing runs this one to undo that.
    pub preview: Option<Cmd>,
}

pub enum Purpose {
    NewBranch {
        at: BString,
    },
    RenameBranch(BString),
    NewTag(ObjectId),
    Reword(ObjectId),
    Squash(ObjectId),
    StashPush,
    SetUpstream(BString),
    /// The message for the planner row with this commit.
    PlannerMessage(ObjectId),
    ContextLines,
    TabWidth,
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
    Header {
        key: String,
        title: String,
        count: usize,
    },
    Branch(usize),
    Remote(BString),
    RemoteBranch(usize),
    Tag(usize),
    Stash(usize),
    Empty(String),
}

/// What a sidebar row shows. Rows refer to refs by their place in the
/// lists in `Refs`, which can change when the refs reload. This stays the
/// same. Folded headers and remotes are kept by their key too.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SideKey {
    Header(String),
    Branch(BString),
    Remote(BString),
    RemoteBranch(BString),
    Tag(BString),
    Stash(ObjectId),
}

impl SideRow {
    /// What this row shows, given the refs it was built from.
    pub fn key(&self, refs: &Refs) -> Option<SideKey> {
        match self {
            SideRow::Header { key, .. } => Some(SideKey::Header(key.clone())),
            SideRow::Branch(i) => refs
                .branches
                .get(*i)
                .map(|b| SideKey::Branch(b.name.clone())),
            SideRow::Remote(r) => Some(SideKey::Remote(r.clone())),
            SideRow::RemoteBranch(i) => refs
                .remote_branches
                .get(*i)
                .map(|b| SideKey::RemoteBranch(b.full_name())),
            SideRow::Tag(i) => refs.tags.get(*i).map(|t| SideKey::Tag(t.name.clone())),
            SideRow::Stash(i) => refs.stashes.get(*i).map(|s| SideKey::Stash(s.id)),
            SideRow::Empty(_) => None,
        }
    }
}

pub fn sidebar_rows(refs: &Refs, collapsed: &HashSet<SideKey>) -> Vec<SideRow> {
    let mut rows = Vec::new();
    let header = |rows: &mut Vec<SideRow>, key: &str, title: &str, count: usize| {
        rows.push(SideRow::Header {
            key: key.into(),
            title: title.into(),
            count,
        });
        !collapsed.contains(&SideKey::Header(key.into()))
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
            if !collapsed.contains(&SideKey::Remote(r.clone())) {
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
            act(Some(Key::ch('?')), "Keyboard shortcuts", Cmd::Help)
                .hint()
                .no_menu(),
            act(Some(Key::ctrl('p')), "Command palette", Cmd::Palette).no_menu(),
            act(Some(Key::ch('F')), "Fetch", Cmd::Fetch).no_menu(),
            act(Some(Key::ch('p')), "Pull", Cmd::PullMenu).no_menu(),
            act(Some(Key::ch('P')), "Push", Cmd::PushMenu).no_menu(),
            act(Some(Key::ch('S')), "Stash changes", Cmd::StashPush).no_menu(),
            act(
                Some(Key::ch('t')),
                "Toggle side-by-side diff",
                Cmd::ToggleSplit,
            )
            .no_menu(),
            act(None, "Choose theme…", Cmd::ChooseTheme).no_menu(),
            act(None, "Set context lines…", Cmd::ChooseContextLines).no_menu(),
            act(None, "Set tab width…", Cmd::ChooseTabWidth).no_menu(),
            act(
                None,
                if self.cfg.relative_dates {
                    "Show full dates in history"
                } else {
                    "Show relative dates in history"
                },
                Cmd::ToggleRelativeDates,
            )
            .no_menu(),
            act(
                Some(Key::ch('w')),
                "Show working directory",
                Cmd::ShowWorkdir,
            )
            .no_menu(),
            act(Some(Key::ch('@')), "Command log", Cmd::CommandLog).no_menu(),
            act(Some(Key::ctrl('b')), "Toggle sidebar", Cmd::ToggleSidebar).no_menu(),
            act(Some(Key::ctrl('r')), "Refresh", Cmd::Refresh).no_menu(),
            act(Some(Key::code(KeyCode::Tab)), "Next pane", Cmd::FocusNext).no_menu(),
            act(
                Some(Key::code(KeyCode::BackTab)),
                "Previous pane",
                Cmd::FocusPrev,
            )
            .no_menu(),
            act(
                Some(Key::ch('1')),
                "Focus sidebar",
                Cmd::Focus(Focus::Sidebar),
            )
            .no_menu(),
            act(
                Some(Key::ch('2')),
                "Focus history",
                Cmd::Focus(Focus::History),
            )
            .no_menu(),
            act(
                Some(Key::ch('3')),
                "Focus details",
                Cmd::Focus(Focus::Detail),
            )
            .no_menu(),
            act(Some(Key::ch('.')), "Open context menu", Cmd::ContextMenu).no_menu(),
            act(Some(Key::ctrl('z')), "Suspend", Cmd::Suspend).no_menu(),
            act(Some(Key::ch('q')), "Quit", Cmd::Quit).no_menu(),
        ];
        if self.repo_state != RepoState::Clean {
            let skip = !matches!(self.repo_state, RepoState::Merge);
            v.insert(
                0,
                act(Some(Key::alt('c')), "Continue", Cmd::ContinueOp)
                    .hint()
                    .no_menu(),
            );
            if skip {
                v.insert(
                    1,
                    act(Some(Key::alt('s')), "Skip commit", Cmd::SkipOp).no_menu(),
                );
            }
            v.insert(
                1,
                act(Some(Key::alt('a')), "Abort", Cmd::AbortOp)
                    .hint()
                    .no_menu(),
            );
        }
        v
    }

    pub fn actions(&self, ctx: Context) -> Vec<ActionDef> {
        match ctx {
            Context::Sidebar => self.sidebar_actions(),
            Context::History => self.history_actions(),
            Context::Detail => self.detail_actions(),
            Context::Message => vec![
                act(
                    Some(Key {
                        code: KeyCode::Enter,
                        ctrl: true,
                        alt: false,
                    }),
                    "Commit",
                    Cmd::Commit,
                )
                .hint(),
                act(Some(Key::alt('a')), "Toggle amend", Cmd::ToggleAmend).hint(),
                act(
                    Some(Key::code(KeyCode::Esc)),
                    "Leave message box",
                    Cmd::Focus(Focus::Detail),
                )
                .hint(),
            ],
            Context::Planner => vec![
                act(Some(Key::ch('p')), "Pick", Cmd::PlanSet(PlanAction::Pick)).hint(),
                act(
                    Some(Key::ch('r')),
                    "Reword",
                    Cmd::PlanSet(PlanAction::Reword),
                )
                .hint(),
                act(
                    Some(Key::ch('s')),
                    "Squash into older",
                    Cmd::PlanSet(PlanAction::Squash),
                )
                .hint(),
                act(
                    Some(Key::ch('f')),
                    "Fixup into older",
                    Cmd::PlanSet(PlanAction::Fixup),
                )
                .hint(),
                act(Some(Key::ch('d')), "Drop", Cmd::PlanSet(PlanAction::Drop)).hint(),
                act(
                    Some(Key::ch('e')),
                    "Edit (stop here)",
                    Cmd::PlanSet(PlanAction::Edit),
                )
                .hint(),
                act(Some(Key::ctrl('k')), "Move up (newer)", Cmd::PlanMove(true)).hint(),
                act(
                    Some(Key::ctrl('j')),
                    "Move down (older)",
                    Cmd::PlanMove(false),
                )
                .hint(),
                act(
                    Some(Key::code(KeyCode::Enter)),
                    "Start rebase",
                    Cmd::PlanStart,
                )
                .hint()
                .no_menu(),
                act(Some(Key::code(KeyCode::Esc)), "Cancel", Cmd::PlanCancel)
                    .hint()
                    .no_menu(),
            ],
        }
    }

    fn history_actions(&self) -> Vec<ActionDef> {
        let Some(c) = self.history.selected_commit() else {
            return vec![
                act(
                    Some(Key::ch('c')),
                    "Write commit message",
                    Cmd::FocusMessage,
                )
                .hint(),
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
            act(Some(Key::ch('r')), "Reword", Cmd::Reword(id))
                .hint()
                .enabled(!merge),
            act(Some(Key::ch('s')), "Squash into parent", Cmd::Squash(id))
                .hint()
                .enabled(has_parent && !merge),
            act(Some(Key::ch('f')), "Fixup into parent", Cmd::Fixup(id))
                .enabled(has_parent && !merge),
            act(Some(Key::ch('e')), "Edit commit", Cmd::Edit(id)).enabled(!merge),
            act(Some(Key::ch('d')), "Drop commit", Cmd::Drop(id))
                .hint()
                .enabled(!merge),
            act(Some(Key::ctrl('k')), "Move up", Cmd::Move(id, true)).enabled(!merge),
            act(Some(Key::ctrl('j')), "Move down", Cmd::Move(id, false))
                .enabled(has_parent && !merge),
            act(
                Some(Key::ch('i')),
                "Edit history from here…",
                Cmd::Planner(id),
            )
            .hint()
            .enabled(!merge),
            act(
                Some(Key::ch('A')),
                "Amend staged changes into this commit",
                Cmd::AmendInto(id),
            )
            .enabled(staged && !merge),
            act(
                Some(Key::ch('b')),
                "New branch here…",
                Cmd::NewBranch(id.to_string().into()),
            ),
            act(Some(Key::ch('T')), "New tag here…", Cmd::NewTag(id)),
            act(
                Some(Key::ch('c')),
                "Check out this commit",
                Cmd::CheckoutCommit(id),
            ),
            act(Some(Key::ch('C')), "Cherry-pick", Cmd::CherryPick(id)),
            act(Some(Key::ch('V')), "Revert", Cmd::Revert(id)),
            act(
                Some(Key::ch('X')),
                "Reset current branch to here…",
                Cmd::ResetMenu(id),
            ),
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
                            v.push(
                                act(
                                    Some(Key::ch(' ')),
                                    format!("Unstage {noun}"),
                                    Cmd::Unstage(u.clone()),
                                )
                                .hint(),
                            );
                        }
                        Section::Conflicted => {}
                        _ => {
                            v.push(
                                act(
                                    Some(Key::ch(' ')),
                                    format!("Stage {noun}"),
                                    Cmd::Stage(u.clone()),
                                )
                                .hint(),
                            );
                            v.push(
                                act(
                                    Some(Key::ch('d')),
                                    format!("Discard {noun}"),
                                    Cmd::Discard(u.clone()),
                                )
                                .hint(),
                            );
                        }
                    }
                }
                if let Some((sec, fi)) = row.and_then(Row::file) {
                    let path = self.section_files(sec).get(fi).map(|f| f.path().to_owned());
                    if let Some(p) = path {
                        if sec == Section::Conflicted {
                            v.push(act(
                                Some(Key::ch('O')),
                                "Resolve using ours",
                                Cmd::TakeSide(p.clone(), true),
                            ));
                            v.push(act(
                                Some(Key::ch('T')),
                                "Resolve using theirs",
                                Cmd::TakeSide(p.clone(), false),
                            ));
                            v.push(
                                act(
                                    Some(Key::ch('M')),
                                    "Mark as resolved",
                                    Cmd::MarkResolved(p.clone()),
                                )
                                .hint(),
                            );
                        }
                        v.push(act(
                            Some(Key::ch('o')),
                            "Open in editor",
                            Cmd::OpenInEditor(p),
                        ));
                    }
                }
                v.push(
                    act(Some(Key::ch('v')), "Select lines", Cmd::StartSelect)
                        .hint()
                        .no_menu(),
                );
                v.push(
                    act(
                        Some(Key::ch('c')),
                        "Write commit message",
                        Cmd::FocusMessage,
                    )
                    .hint(),
                );
                v.push(act(Some(Key::ch('a')), "Stage all", Cmd::StageAll));
                v.push(act(Some(Key::ch('u')), "Unstage all", Cmd::UnstageAll));
                v.push(act(Some(Key::ch('A')), "Toggle amend", Cmd::ToggleAmend));
                v.extend(nav);
                v
            }
            Source::Commit(id) | Source::Stash(id) => {
                let mut v = vec![act(
                    Some(Key::ch('y')),
                    "Copy commit hash",
                    Cmd::CopyHash(id),
                )];
                if let Some((sec, fi)) =
                    self.detail.rows.get(self.detail.cursor).and_then(Row::file)
                    && let Some(f) = self.section_files(sec).get(fi)
                    && f.new_path.is_some()
                {
                    v.push(act(
                        Some(Key::ch('o')),
                        "Open file in editor",
                        Cmd::OpenInEditor(f.path().to_owned()),
                    ));
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
                act(
                    Some(Key::code(KeyCode::Enter)),
                    "Fold or unfold",
                    Cmd::ToggleSideSection(SideKey::Header(key.clone())),
                )
                .hint(),
            ],
            SideRow::Remote(r) => vec![
                act(
                    Some(Key::code(KeyCode::Enter)),
                    "Fold or unfold",
                    Cmd::ToggleSideSection(SideKey::Remote(r.clone())),
                )
                .hint(),
                act(Some(Key::ch('F')), "Fetch", Cmd::Fetch),
            ],
            SideRow::Branch(i) => {
                let b = &refs.branches[*i];
                let is_current = b.is_head;
                let name = b.name.clone();
                let mut v = vec![
                    act(Some(Key::ch(' ')), "Check out", Cmd::Checkout(name.clone()))
                        .hint()
                        .enabled(!is_current),
                    act(
                        Some(Key::ch('n')),
                        "New branch from here…",
                        Cmd::NewBranch(name.clone()),
                    )
                    .hint(),
                    act(
                        Some(Key::ch('R')),
                        "Rename…",
                        Cmd::RenameBranch(name.clone()),
                    ),
                    act(
                        Some(Key::ch('d')),
                        "Delete",
                        Cmd::DeleteBranch(name.clone(), false),
                    )
                    .hint()
                    .enabled(!is_current),
                    act(
                        Some(Key::ch('D')),
                        "Force delete",
                        Cmd::DeleteBranch(name.clone(), true),
                    )
                    .enabled(!is_current),
                    act(
                        Some(Key::ch('u')),
                        "Set upstream…",
                        Cmd::SetUpstream(name.clone()),
                    ),
                ];
                if let Some(cur) = &current
                    && !is_current
                {
                    v.push(act(
                        Some(Key::ch('M')),
                        format!("Merge into {cur}"),
                        Cmd::Merge(name.clone()),
                    ));
                    v.push(act(
                        Some(Key::ch('r')),
                        format!("Rebase {cur} onto this"),
                        Cmd::RebaseOnto(name),
                    ));
                }
                v
            }
            SideRow::RemoteBranch(i) => {
                let rb = &refs.remote_branches[*i];
                let full = rb.full_name();
                let mut v = vec![
                    act(
                        Some(Key::ch(' ')),
                        "Check out",
                        Cmd::CheckoutRemote(rb.remote.clone(), rb.name.clone()),
                    )
                    .hint(),
                    act(
                        Some(Key::ch('n')),
                        "New branch from here…",
                        Cmd::NewBranch(full.clone()),
                    )
                    .hint(),
                    act(
                        Some(Key::ch('d')),
                        "Delete on remote",
                        Cmd::DeleteRemoteBranch(rb.remote.clone(), rb.name.clone()),
                    ),
                ];
                if let Some(cur) = &current {
                    v.push(act(
                        Some(Key::ch('M')),
                        format!("Merge into {cur}"),
                        Cmd::Merge(full.clone()),
                    ));
                    v.push(act(
                        Some(Key::ch('r')),
                        format!("Rebase {cur} onto this"),
                        Cmd::RebaseOnto(full),
                    ));
                }
                v
            }
            SideRow::Tag(i) => {
                let t = &refs.tags[*i];
                vec![
                    act(
                        Some(Key::ch('d')),
                        "Delete tag",
                        Cmd::DeleteTag(t.name.clone()),
                    )
                    .hint(),
                    act(None, "Push tag", Cmd::PushTag(t.name.clone())),
                    act(
                        Some(Key::ch('n')),
                        "New branch from here…",
                        Cmd::NewBranch(t.name.clone()),
                    ),
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
                    Unit::Section(Section::Untracked) => {
                        "all untracked files will be deleted".to_string()
                    }
                    Unit::Section(_) => "all unstaged changes will be lost".to_string(),
                    Unit::File(sec, p) => discard_what(*sec, std::slice::from_ref(p)),
                    Unit::Files(sec, paths) => discard_what(*sec, paths),
                    Unit::Hunk(..) => "this hunk's changes will be lost".to_string(),
                    Unit::Lines(..) => "the selected changes will be lost".to_string(),
                };
                s(
                    "Discard changes?",
                    format!("This can't be undone: {what}."),
                    "Discard",
                )
            }
            Cmd::Drop(id) => s(
                "Drop commit?",
                format!("Commit {} will be removed from the branch.", short(id)),
                "Drop",
            ),
            Cmd::Reset(id, ResetMode::Hard) => s(
                "Hard reset?",
                format!(
                    "The branch moves to {} and all uncommitted changes are lost.",
                    short(id)
                ),
                "Reset",
            ),
            Cmd::DeleteBranch(name, force) => s(
                "Delete branch?",
                if *force {
                    format!(
                        "{name} isn't merged. Its commits may be lost if nothing else points at them."
                    )
                } else {
                    format!("Delete the branch {name}?")
                },
                "Delete",
            ),
            Cmd::DeleteRemoteBranch(r, b) => s(
                "Delete remote branch?",
                format!("This deletes {b} on {r} for everyone."),
                "Delete",
            ),
            Cmd::DeleteTag(t) => s("Delete tag?", format!("Delete the tag {t}?"), "Delete"),
            Cmd::StashDrop(id) => {
                let what = match self.refs.stashes.iter().find(|st| st.id == *id) {
                    Some(st) => format!("\"{}\"", st.message),
                    None => format!("Stash {}", short(id)),
                };
                s("Drop stash?", format!("{what} will be deleted."), "Drop")
            }
            Cmd::AbortOp => s(
                "Abort?",
                "Everything done since the operation started will be undone.".into(),
                "Abort",
            ),
            Cmd::Push {
                branch,
                force: true,
            } => s(
                "Force push?",
                format!(
                    "This force pushes {branch}. Commits on the remote that you don't have will be lost. tergi uses --force-with-lease, which refuses if the remote changed since you last fetched."
                ),
                "Force push",
            ),
            Cmd::CommitAll => s(
                "Nothing is staged",
                "Stage all changes and commit them?".into(),
                "Stage all and commit",
            ),
            _ => None,
        }
    }

    pub fn run_cmd(&mut self, cmd: Cmd) {
        let cmd = match cmd {
            Cmd::Confirmed(inner) => *inner,
            other => {
                let other = match other {
                    Cmd::Discard(Unit::Section(sec)) => {
                        let mut seen = HashSet::new();
                        let paths: Vec<BString> = self
                            .section_files(sec)
                            .iter()
                            .map(|f| f.path().to_owned())
                            .filter(|p| seen.insert(p.clone()))
                            .collect();
                        if paths.is_empty() {
                            self.set_status("There's nothing to discard", Level::Info);
                            return;
                        }
                        Cmd::Discard(Unit::Files(sec, paths))
                    }
                    other => other,
                };
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
                self.overlays.push(Overlay::Menu(Menu {
                    title: Some("Keyboard shortcuts".into()),
                    items,
                    selected: 0,
                    anchor: None,
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
                self.overlays.push(Overlay::Menu(Menu {
                    title: Some("Theme".into()),
                    items,
                    selected,
                    anchor: None,
                    scroll: 0,
                    preview: Some(Cmd::RevertTheme),
                }));
            }
            Cmd::SetTheme(id) => self.change_config(|c| c.theme = id.to_string()),
            Cmd::PreviewTheme(id) => self.set_theme(id),
            Cmd::RevertTheme => {
                let id = self.cfg.theme.clone();
                self.set_theme(&id);
            }
            Cmd::ChooseContextLines => {
                let hint = format!("Lines to show around each change, from 0 to {}", config::MAX_CONTEXT_LINES);
                let current = self.cfg.context_lines.to_string();
                self.open_input("Context lines", &hint, false, &current, Purpose::ContextLines);
            }
            Cmd::ChooseTabWidth => {
                let hint = format!("Columns per tab, from 1 to {}", config::MAX_TAB_WIDTH);
                let current = self.cfg.tab_width.to_string();
                self.open_input("Tab width", &hint, false, &current, Purpose::TabWidth);
            }
            Cmd::ToggleRelativeDates => self.change_config(|c| c.relative_dates = !c.relative_dates),
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
                self.spawn_op("Fetch", After::Nothing, move |r, progress| async move {
                    r.git
                        .run_streaming(
                            &["fetch", "--all", "--prune", "--progress"],
                            &crate::git::cli::RunOpts::default(),
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
                self.spawn_op("Pull", After::Nothing, move |r, progress| async move {
                    let mut args = vec!["pull", "--progress"];
                    match mode {
                        PullMode::Default => {}
                        PullMode::FfOnly => args.push("--ff-only"),
                        PullMode::Rebase => args.extend(["--rebase", "--autostash"]),
                        PullMode::Merge => args.extend(["--no-rebase", "--no-edit"]),
                    }
                    r.git
                        .run_streaming(&args, &crate::git::cli::RunOpts::default(), |l| {
                            progress(l.to_string());
                        }).await
                        .map(|_| "Pulled".to_string())
                });
            }
            Cmd::PushMenu => {
                let Some(branch) = self.refs.current_branch().map(|b| b.name.clone()) else {
                    self.set_status("Can't push: no branch is checked out", Level::Error);
                    return;
                };
                let items = vec![
                    act(Some(Key::ch('P')), "Push", Cmd::Push { branch: branch.clone(), force: false }),
                    act(Some(Key::ch('f')), "Force push (with lease)", Cmd::Push { branch: branch.clone(), force: true }),
                ];
                self.open_menu(&format!("Push {branch}"), items, Some(&Cmd::PushMenu));
            }
            Cmd::Push { branch, force } => {
                let Some(branch) = self.refs.branch(branch.as_ref()).cloned() else {
                    self.set_status(format!("Can't push: {branch} doesn't exist any more"), Level::Error);
                    return;
                };
                let remote = if self.refs.remotes.iter().any(|r| r == "origin") {
                    Some(BString::from("origin"))
                } else {
                    self.refs.remotes.first().cloned()
                };
                let has_upstream = branch.upstream.is_some() && !branch.upstream_gone;
                let Some(remote) = remote else {
                    self.set_status("Can't push: this repository has no remotes", Level::Error);
                    return;
                };
                self.spawn_op("Push", After::Nothing, move |r, progress| async move {
                    // Without a refspec, git pushes whatever is checked out
                    // when it runs. We leave the refspec out so that settings
                    // like pushRemote work, so check the branch instead.
                    if ops::head_branch(&r.git).await.as_ref() != Some(&branch.name) {
                        return Err(anyhow::anyhow!("{} isn't checked out any more, so nothing was pushed.", branch.name));
                    }
                    let mut args = vec![os_str(b"push"), os_str(b"--progress")];
                    if force {
                        args.push(os_str(b"--force-with-lease"));
                    }
                    if !has_upstream {
                        args.extend([os_str(b"-u"), os_str(b"--end-of-options"), os_str(&remote), os_str(&branch.name)]);
                    }
                    r.git
                        .run_streaming(&args, &crate::git::cli::RunOpts::default(), |l| {
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
                self.open_input("New branch", &format!("Created at {} and checked out", short_name(at.as_ref())), false, "", Purpose::NewBranch { at });
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
            Cmd::OpenInEditor(p) => self.suspend = Some(Suspend::Edit(self.repo.workdir.join(fs_path(&p)))),
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
            Cmd::CheckoutRemote(remote, name) => {
                let exists = self.refs.branch(name.as_ref()).is_some();
                self.spawn_op("Checkout", After::Nothing, move |r, _| async move {
                    ops::checkout_remote(&r.git, &remote, &name, exists).await.map(|()| format!("Switched to {name}"))
                });
            }
            Cmd::RenameBranch(b) => {
                let initial = b.to_str_lossy().into_owned();
                self.open_input("Rename branch", &format!("New name for {b}"), false, &initial, Purpose::RenameBranch(b));
            }
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
                self.spawn_op("Delete remote branch", After::Nothing, move |r, _| async move {
                    r.git
                        .run(&[os_str(b"push"), os_str(b"--delete"), os_str(b"--end-of-options"), os_str(&remote), os_str(&name)]).await
                        .map(|_| format!("Deleted {name} on {remote}"))
                });
            }
            Cmd::SetUpstream(b) => {
                let guess = format!("{}/{b}", self.refs.remotes.first().map_or("origin".into(), |r| r.to_str_lossy()));
                self.open_input("Set upstream", &format!("Remote branch for {b} to track"), false, &guess, Purpose::SetUpstream(b));
            }
            Cmd::Merge(b) => self.spawn_op("Merge", After::Nothing, move |r, _| async move { ops::merge(&r.git, &b).await.map(|()| format!("Merged {b}")) }),
            Cmd::RebaseOnto(b) => self.rebase_op("Rebase", move |r| async move { rebase::rebase_onto(&r.git, &b).await }),
            Cmd::DeleteTag(t) => self.spawn_op("Delete tag", After::Nothing, move |r, _| async move { ops::delete_tag(&r.git, &t).await.map(|()| format!("Deleted tag {t}")) }),
            Cmd::PushTag(t) => {
                let remote = self.refs.remotes.first().cloned().unwrap_or_else(|| "origin".into());
                self.spawn_op("Push tag", After::Nothing, move |r, p| async move {
                    let refspec = [b"refs/tags/", t.as_bytes()].concat();
                    r.git
                        .run_streaming(&[os_str(b"push"), os_str(b"--progress"), os_str(b"--end-of-options"), os_str(&remote), os_str(&refspec)], &crate::git::cli::RunOpts::default(), |l| p(l.to_string())).await
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
                self.rebuild_sidebar(None);
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
        self.spawn_op(
            if amend { "Amend" } else { "Commit" },
            After::ClearMessage,
            move |r, _| async move {
                if all {
                    ops::stage_all(&r.git).await?;
                }
                ops::commit(&r.git, &msg, amend).await
            },
        );
    }

    fn rebase_op<F>(
        &mut self,
        label: &str,
        f: impl FnOnce(std::sync::Arc<super::Repo>) -> F + Send + 'static,
    ) where
        F: Future<Output = anyhow::Result<Outcome>> + Send + 'static,
    {
        self.spawn_op(label, After::Nothing, move |r, _| async move {
            Ok(match f(r).await? {
                Outcome::Done => "Done".to_string(),
                Outcome::Stopped => {
                    "Stopped. Make your changes, then Continue (alt+c).".to_string()
                }
            })
        });
    }

    /// Open a menu. When the mouse opened it, it goes below the button for
    /// `from` if that's on screen, and at the mouse pointer otherwise.
    fn open_menu(&mut self, title: &str, items: Vec<ActionDef>, from: Option<&Cmd>) {
        let anchor = self.by_mouse.then(|| {
            let button = from.and_then(|cmd| {
                self.hits.iter().find_map(|(r, t)| match t {
                    Target::Button(c) if c == cmd => Some((r.x, r.y + 1)),
                    _ => None,
                })
            });
            button.unwrap_or(self.mouse)
        });
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
    /// selected row. Unlike other menus, it stays near the row when the
    /// keyboard opens it, because it acts on that row.
    pub fn open_context_menu(&mut self, at: Option<(u16, u16)>) {
        let items: Vec<ActionDef> = self
            .actions(self.context())
            .into_iter()
            .filter(|a| a.menu)
            .collect();
        if items.is_empty() {
            return;
        }
        let anchor = at.unwrap_or_else(|| {
            let a = match self.focus {
                Focus::Sidebar => (
                    self.areas.sidebar,
                    self.sidebar.selected.saturating_sub(self.sidebar.scroll),
                ),
                Focus::History => (
                    self.areas.history,
                    // The menu goes below the second row of the entry.
                    self.history
                        .current_row()
                        .saturating_sub(self.history.scroll)
                        * usize::from(HISTORY_ROW_HEIGHT)
                        + usize::from(HISTORY_ROW_HEIGHT - 1),
                ),
                _ => (
                    self.areas.doc,
                    self.detail.cursor.saturating_sub(self.detail.scroll),
                ),
            };
            (
                a.0.x + 4,
                a.0.y + (a.1 as u16).min(a.0.height.saturating_sub(1)) + 1,
            )
        });
        self.overlays.push(Overlay::Menu(Menu {
            title: None,
            items,
            selected: 0,
            anchor: Some(anchor),
            scroll: 0,
            preview: None,
        }));
    }

    fn open_input(
        &mut self,
        title: &str,
        hint: &str,
        multiline: bool,
        initial: &str,
        purpose: Purpose,
    ) {
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
            _ if name.is_empty() && !matches!(b.purpose, Purpose::StashPush) => {
                self.set_status("Nothing entered", Level::Warn);
                self.cancel_input(&b);
            }
            Purpose::NewBranch { at } => {
                self.spawn_op("New branch", After::Nothing, move |r, _| async move {
                    ops::create_branch(&r.git, &name, &at, true)
                        .await
                        .map(|()| format!("Created and switched to {name}"))
                });
            }
            Purpose::RenameBranch(old) => {
                self.spawn_op("Rename branch", After::Nothing, move |r, _| async move {
                    ops::rename_branch(&r.git, &old, &name)
                        .await
                        .map(|()| format!("Renamed {old} to {name}"))
                });
            }
            Purpose::NewTag(id) => {
                self.spawn_op("New tag", After::Nothing, move |r, _| async move {
                    ops::create_tag(&r.git, &name, &id.to_string(), None)
                        .await
                        .map(|()| format!("Created tag {name}"))
                });
            }
            Purpose::SetUpstream(branch) => {
                self.spawn_op("Set upstream", After::Nothing, move |r, _| async move {
                    ops::set_upstream(&r.git, &branch, &name)
                        .await
                        .map(|()| format!("{branch} now tracks {name}"))
                });
            }
            Purpose::Reword(id) => self.rebase_op("Reword", move |r| async move {
                rebase::reword(&r.git, &exe, id, &text).await
            }),
            Purpose::Squash(id) => self.rebase_op("Squash", move |r| async move {
                rebase::single(&r.git, &exe, id, Action::Squash(text)).await
            }),
            Purpose::StashPush => self.spawn_op("Stash", After::Nothing, move |r, _| async move {
                ops::stash_push(&r.git, &name, true, false)
                    .await
                    .map(|()| "Stashed changes".into())
            }),
            Purpose::PlannerMessage(id) => {
                if let Some(p) = &mut self.planner
                    && let Some(r) = p.rows.iter_mut().find(|r| r.id == id)
                {
                    r.message = Some(text);
                }
            }
            Purpose::ContextLines => match name.parse::<u32>() {
                Ok(n) if n <= config::MAX_CONTEXT_LINES => {
                    self.change_config(|c| c.context_lines = n);
                }
                _ => self.set_status(
                    format!(
                        "Context lines must be a number from 0 to {}",
                        config::MAX_CONTEXT_LINES
                    ),
                    Level::Error,
                ),
            },
            Purpose::TabWidth => match name.parse::<usize>() {
                Ok(n) if (1..=config::MAX_TAB_WIDTH).contains(&n) => {
                    self.change_config(|c| c.tab_width = n);
                }
                _ => self.set_status(
                    format!(
                        "The tab width must be a number from 1 to {}",
                        config::MAX_TAB_WIDTH
                    ),
                    Level::Error,
                ),
            },
        }
    }

    pub fn cancel_input(&mut self, b: &InputBox) {
        // Without a message, reword and squash make no sense.
        if let Purpose::PlannerMessage(id) = b.purpose
            && let Some(p) = &mut self.planner
            && let Some(r) = p.rows.iter_mut().find(|r| r.id == id)
            && r.message.is_none()
        {
            r.action = PlanAction::Pick;
        }
    }

    fn apply_unit(&mut self, unit: Unit, mode: ApplyMode, has_head: bool) {
        let (label, past) = match mode {
            ApplyMode::Stage => ("Stage", "Staged"),
            ApplyMode::Unstage => ("Unstage", "Unstaged"),
            ApplyMode::Discard => ("Discard", "Discarded"),
        };
        let noun = unit.noun();
        let done = format!("{past} {noun}");
        let paths: Vec<BString> = match &unit {
            Unit::Section(sec) => self
                .section_files(*sec)
                .iter()
                .map(|f| f.path().to_owned())
                .collect(),
            Unit::File(_, path) => vec![path.clone()],
            Unit::Files(_, paths) => paths.clone(),
            _ => Vec::new(),
        };
        self.detail.anchor = None;
        self.spawn_op(label, After::Nothing, move |r, _| async move {
            let git = &r.git;
            match (&unit, mode) {
                (Unit::Section(Section::Staged), ApplyMode::Unstage) => {
                    ops::unstage_all(git, has_head).await?;
                }
                (Unit::Section(_) | Unit::File(..) | Unit::Files(..), ApplyMode::Stage) => {
                    ops::stage_paths(git, &paths).await?;
                }
                (Unit::Section(_) | Unit::File(..) | Unit::Files(..), ApplyMode::Unstage) => {
                    ops::unstage_paths(git, &paths, has_head).await?;
                }
                (Unit::Section(s) | Unit::File(s, _) | Unit::Files(s, _), ApplyMode::Discard) => {
                    if *s == Section::Untracked {
                        ops::delete_untracked(git, &paths).await?;
                    } else {
                        ops::discard_paths(git, &paths).await?;
                    }
                }
                (Unit::Hunk(_, f, h), _) => {
                    let h = *h;
                    ops::apply_lines(git, f, mode, &|hh, _| hh == h).await?;
                }
                (Unit::Lines(_, f, set), _) => {
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
                    let key = (sec, f.path().to_owned());
                    if !self.detail.collapsed.remove(&key) {
                        self.detail.collapsed.insert(key);
                    }
                    // Keep the cursor on the file header.
                    if let Some(i) = self
                        .detail
                        .rows
                        .iter()
                        .position(|r| *r == Row::File { sec, file: fi })
                    {
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
        let keys: Vec<(Section, BString)> = self
            .detail
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::File { sec, file } => self
                    .section_files(*sec)
                    .get(*file)
                    .map(|f| (*sec, f.path().to_owned())),
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
        let Some(f) = self.section_files(sec).get(fi) else {
            return;
        };
        if !f.too_large {
            return;
        }
        let ctx = self.cfg.context_lines;
        match sec {
            Section::Unstaged | Section::Staged => {
                let f = f.clone();
                self.spawn_read(
                    move |r| async move {
                        crate::git::workdir::load_full_diff(&r.git, &f, sec == Section::Staged, ctx)
                            .await
                    },
                    move |app, full| match full {
                        Ok(Some(full)) => app.put_full_diff(sec, full, None),
                        Ok(None) => {}
                        Err(e) => app.show_error("Loading the diff failed", &e),
                    },
                );
            }
            Section::Untracked => {
                let path = f.path().to_owned();
                self.spawn_read(
                    move |r| async move {
                        crate::git::workdir::load_full_untracked(&r.git, path.as_bstr(), ctx).await
                    },
                    move |app, (full, texts)| app.put_full_diff(sec, full, Some(texts)),
                );
            }
            Section::Commit => {
                let Some(id) = self.detail.commit.as_ref().map(|c| c.id) else {
                    return;
                };
                self.spawn_read(
                    move |r| {
                        crate::util::blocking(move || {
                            crate::git::commit::load(&r.gix.to_thread_local(), id, ctx, usize::MAX)
                        })
                    },
                    move |app, full| {
                        if app.detail.commit.as_ref().is_none_or(|c| c.id != id) {
                            return;
                        }
                        match full {
                            Ok(full) => {
                                app.detail.set_commit(Arc::new(full));
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
    /// look the file up again by its paths. If it's gone, or no longer cut
    /// short, leave it alone.
    fn put_full_diff(&mut self, sec: Section, full: FileDiff, texts: Option<FileTexts>) {
        let Some(w) = &mut self.workdir else { return };
        let list = match sec {
            Section::Staged => &mut w.staged,
            Section::Unstaged => &mut w.unstaged,
            Section::Untracked => &mut w.untracked,
            Section::Commit | Section::Conflicted => return,
        };
        let Some(i) = list.iter().position(|f| {
            f.too_large && f.old_path == full.old_path && f.new_path == full.new_path
        }) else {
            return;
        };
        list[i] = Arc::new(full);
        if let Some(texts) = texts
            && let Some(t) = w.untracked_texts.get_mut(i)
        {
            *t = texts;
        }
        if self.detail.source == Source::Workdir {
            self.detail.reset_for_reload();
            self.rebuild_doc();
        }
    }
}

/// What discarding `paths` in `sec` does, for the dialog that asks first.
fn discard_what(sec: Section, paths: &[BString]) -> String {
    const SHOWN: usize = 5;
    let paths: Vec<String> = paths
        .iter()
        .map(|p| p.to_str_lossy().into_owned())
        .collect();
    let list = match &paths[..] {
        [] => String::new(),
        [p] => p.clone(),
        [rest @ .., last] if paths.len() <= SHOWN => format!("{} and {last}", rest.join(", ")),
        _ => format!(
            "{} and {} other files",
            paths[..SHOWN - 1].join(", "),
            paths.len() - (SHOWN - 1)
        ),
    };
    if sec == Section::Untracked {
        format!("{list} will be deleted")
    } else {
        format!("unstaged changes to {list} will be lost")
    }
}

pub fn short(id: &ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}

fn short_name(s: &BStr) -> String {
    if s.len() == 40 && s.iter().all(u8::is_ascii_hexdigit) {
        s[..7].to_str_lossy().into_owned()
    } else {
        s.to_str_lossy().into_owned()
    }
}
