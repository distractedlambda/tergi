//! Application state and the event loop.

pub mod cmd;
pub mod doc;
pub mod input;
pub mod planner;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use gix::ObjectId;
use ratatui::layout::Rect;

use crate::config::Config;
use crate::git::cli::Git;
use crate::git::commit::CommitDetail;
use crate::git::diff::{DEFAULT_LINE_LIMIT, FileDiff};
use crate::git::graph::GraphRow;
use crate::git::log::{self, CommitInfo};
use crate::git::rebase::{self, RepoState};
use crate::git::refs::{Label, Refs};
use crate::git::workdir::Workdir;
use crate::highlight::{self, FileHighlight, Highlighter};
use crate::theme::Theme;
use crate::ui::textarea::TextArea;
use crate::watch;

pub use cmd::Cmd;
use doc::Detail;
use planner::Planner;

/// Everything about the repository that background threads need.
pub struct Repo {
    pub git: Git,
    pub gix: gix::ThreadSafeRepository,
    pub workdir: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    /// Our own executable, which git calls back into as an editor or to ask
    /// for passwords.
    pub exe: PathBuf,
    pub askpass_socket: Option<PathBuf>,
}

impl Repo {
    pub fn open(path: &std::path::Path) -> Result<Repo> {
        if !path.exists() {
            anyhow::bail!("{} doesn't exist", path.display());
        }
        let repo = gix::discover(path).map_err(|e| {
            // gix's not-found message repeats the path and ends in an error
            // class name, so say it more plainly.
            if e.to_string().starts_with("Could not find a git repository") {
                let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
                anyhow::anyhow!("{} is not inside a git repository", shown.display())
            } else {
                anyhow::anyhow!("could not open the repository: {e}")
            }
        })?;
        let workdir = repo
            .workdir()
            .context("bare repositories aren't supported")?
            .to_path_buf();
        let workdir = workdir.canonicalize().unwrap_or(workdir);
        let git_dir = repo.git_dir().to_path_buf();
        let common_dir = repo.common_dir().to_path_buf();
        Ok(Repo {
            git: Git::new(&workdir),
            gix: repo.into_sync(),
            workdir,
            git_dir,
            common_dir,
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tergi")),
            askpass_socket: None,
        })
    }

    /// Environment for commands that may need to ask for credentials.
    pub fn remote_env(&self) -> Vec<(String, String)> {
        let Some(sock) = &self.askpass_socket else {
            return Vec::new();
        };
        let exe = self.exe.to_string_lossy().into_owned();
        vec![
            ("GIT_ASKPASS".into(), exe.clone()),
            ("SSH_ASKPASS".into(), exe),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (crate::askpass::SOCKET_ENV.into(), sock.to_string_lossy().into_owned()),
        ]
    }
}

pub enum AppEvent {
    Input(crossterm::event::Event),
    History(log::Batch),
    HistoryError(u64, String),
    Refs(u64, Result<Refs>),
    Workdir(u64, Box<Result<Workdir>>),
    Commit(u64, ObjectId, Box<Result<CommitDetail>>),
    Highlight(highlight::Key, FileHighlight),
    OpDone(OpResult),
    Progress(String),
    Watch(watch::Event),
    AskPass(String, Sender<Option<String>>),
    Tick,
}

pub struct OpResult {
    pub label: String,
    pub result: Result<String>,
    pub after: After,
}

/// What to do in the UI once an operation succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum After {
    #[default]
    Nothing,
    ClearMessage,
    OpenPlannerDone,
    SelectHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    History,
    Detail,
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

pub struct Busy {
    pub label: String,
    pub progress: String,
    pub since: Instant,
}

/// The loaded history.
#[derive(Default)]
pub struct History {
    pub commits: Vec<CommitInfo>,
    pub rows: Vec<GraphRow>,
    pub index: HashMap<ObjectId, usize>,
    pub generation: u64,
    pub loading: bool,
    pub tips: Vec<ObjectId>,
    /// Row selected in the list. Row 0 is the working directory, row `i + 1`
    /// is `commits[i]`.
    pub selected: usize,
    pub scroll: usize,
    /// A commit to select once the walk reaches it.
    pub pending_select: Option<ObjectId>,
    /// The new walk replaces the list when its first batch arrives, so the
    /// old list stays on screen until then.
    pub replace_on_next: bool,
    /// Why the last walk failed, if it did.
    pub error: Option<String>,
}

impl History {
    /// The number of rows, counting the working directory row.
    pub fn row_count(&self) -> usize {
        self.commits.len() + 1
    }

    pub fn selected_commit(&self) -> Option<&CommitInfo> {
        self.selected.checked_sub(1).and_then(|i| self.commits.get(i))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Splitter {
    Sidebar,
    History,
}

#[derive(Debug, Clone)]
pub enum Drag {
    Splitter(Splitter),
    DiffSelect,
    Planner(usize),
}

/// Screen areas from the last frame, for mouse handling.
#[derive(Debug, Clone, Copy, Default)]
pub struct Areas {
    pub sidebar: Rect,
    pub history: Rect,
    pub detail: Rect,
    pub doc: Rect,
    pub message: Rect,
    pub input: Rect,
    pub planner: Rect,
    pub body: Rect,
}

pub struct App {
    pub repo: Arc<Repo>,
    pub tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    pub cfg: Config,
    pub theme: Theme,
    pub quit: bool,

    pub refs: Refs,
    pub labels: HashMap<ObjectId, Vec<Label>>,
    refs_generation: u64,
    refs_inflight: bool,
    refs_again: bool,

    pub history: History,
    history_current: Arc<AtomicU64>,

    pub workdir: Option<Workdir>,
    workdir_generation: u64,
    workdir_inflight: bool,
    workdir_again: bool,

    pub repo_state: RepoState,
    pub focus: Focus,
    pub sidebar: SidebarState,
    pub detail: Detail,
    detail_tx: Sender<(u64, ObjectId)>,
    pub highlighter: Highlighter,

    pub overlays: Vec<cmd::Overlay>,
    pub planner: Option<Planner>,
    pub message: TextArea,
    pub amend: bool,
    /// The message the box had before amend filled in HEAD's message.
    message_before_amend: Option<String>,

    pub split_diff: bool,
    /// Whether the user has shown or hidden the sidebar. `None` means it
    /// depends on the terminal width.
    pub sidebar_visible: Option<bool>,
    pub sidebar_width: u16,
    pub history_frac: f32,

    pub busy: Option<Busy>,
    pub status: Option<(String, Level, Instant)>,
    pub animating: Arc<AtomicBool>,

    pub areas: Areas,
    pub hits: Vec<(Rect, input::Target)>,
    pub mouse: (u16, u16),
    pub drag: Option<Drag>,
    last_click: Option<(Instant, u16, u16)>,
    pub size: (u16, u16),

    /// Something the event loop has to do outside of `handle`, such as
    /// running an editor in the terminal.
    pub suspend: Option<Suspend>,
    _watcher: Option<watch::Handle>,
    pub watch_degraded: bool,
    last_poll: Instant,
}

pub enum Suspend {
    /// Stop the process, as for Ctrl+Z in a shell.
    Stop,
    Edit(PathBuf),
}

#[derive(Default)]
pub struct SidebarState {
    pub rows: Vec<cmd::SideRow>,
    pub selected: usize,
    pub scroll: usize,
    pub collapsed: HashSet<String>,
}

const REFRESH_REFS: u8 = 1;
const REFRESH_WORKDIR: u8 = 2;
pub const REFRESH_ALL: u8 = REFRESH_REFS | REFRESH_WORKDIR;

impl App {
    pub fn new(mut repo: Repo, cfg: Config, config_error: Option<String>) -> App {
        let (tx, rx) = unbounded();
        let tx_ask = tx.clone();
        repo.askpass_socket = crate::askpass::start_server(move |prompt, reply| {
            let _ = tx_ask.send(AppEvent::AskPass(prompt, reply));
        });
        let theme = Theme::new(cfg.theme);
        let tx2 = tx.clone();
        let highlighter = Highlighter::new(theme.dark, move |k, h| {
            let _ = tx2.send(AppEvent::Highlight(k, h));
        });
        let repo = Arc::new(repo);
        let detail_tx = spawn_detail_worker(repo.clone(), tx.clone(), cfg.context_lines);
        let mut message = TextArea::new(true);
        message.placeholder = "Commit message (Ctrl+Enter to commit)".into();
        let mut collapsed = HashSet::new();
        collapsed.insert("tags".to_string());
        let mut app = App {
            repo,
            tx,
            rx,
            cfg,
            theme,
            quit: false,
            refs: Refs::default(),
            labels: HashMap::new(),
            refs_generation: 0,
            refs_inflight: false,
            refs_again: false,
            history: History::default(),
            history_current: Arc::new(AtomicU64::new(0)),
            workdir: None,
            workdir_generation: 0,
            workdir_inflight: false,
            workdir_again: false,
            repo_state: RepoState::Clean,
            focus: Focus::History,
            sidebar: SidebarState {
                collapsed,
                ..Default::default()
            },
            detail: Detail::default(),
            detail_tx,
            highlighter,
            overlays: Vec::new(),
            planner: None,
            message,
            amend: false,
            message_before_amend: None,
            split_diff: false,
            sidebar_visible: None,
            sidebar_width: 28,
            history_frac: 0.45,
            busy: None,
            status: None,
            animating: Arc::new(AtomicBool::new(false)),
            areas: Areas::default(),
            hits: Vec::new(),
            mouse: (0, 0),
            drag: None,
            last_click: None,
            size: (80, 24),
            suspend: None,
            _watcher: None,
            watch_degraded: false,
            last_poll: Instant::now(),
        };
        if let Some(e) = config_error {
            app.set_status(format!("Config error: {e}"), Level::Error);
        }
        app
    }

    pub fn start(&mut self) {
        let tx = self.tx.clone();
        match watch::start(
            &self.repo.workdir,
            &self.repo.git_dir,
            &self.repo.common_dir,
            move |e| {
                let _ = tx.send(AppEvent::Watch(e));
            },
        ) {
            Ok(h) => self._watcher = Some(h),
            Err(e) => {
                self.watch_degraded = true;
                self.set_status(format!("File watching is off: {e:#}"), Level::Warn);
            }
        }
        self.spawn_ticker();
        self.repo_state = rebase::state_in(&self.repo.git_dir);
        self.refresh(REFRESH_ALL);
        self.select_history(0);
    }

    fn spawn_ticker(&self) {
        let tx = self.tx.clone();
        let animating = self.animating.clone();
        std::thread::Builder::new()
            .name("tick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(120));
                    let anim = animating.load(Ordering::Relaxed);
                    if anim && tx.send(AppEvent::Tick).is_err() {
                        break;
                    }
                    if !anim {
                        // Still tick now and then so status messages expire
                        // and polling can happen.
                        std::thread::sleep(Duration::from_millis(880));
                        if tx.send(AppEvent::Tick).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();
    }

    pub fn recv(&self) -> Option<AppEvent> {
        self.rx.recv().ok()
    }

    pub fn try_recv(&self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }

    pub fn set_status(&mut self, msg: impl Into<String>, level: Level) {
        self.status = Some((msg.into(), level, Instant::now()));
    }

    pub fn show_error(&mut self, title: &str, err: &anyhow::Error) {
        self.overlays.push(cmd::Overlay::Message {
            title: title.to_string(),
            body: format!("{err:#}"),
            scroll: 0,
            error: true,
        });
    }

    fn update_animating(&self) {
        let anim = self.busy.is_some() || self.history.loading || self.detail.loading;
        self.animating.store(anim, Ordering::Relaxed);
    }

    // ----- Refreshing -----

    pub fn refresh(&mut self, what: u8) {
        if what & REFRESH_REFS != 0 {
            if self.refs_inflight {
                self.refs_again = true;
            } else {
                self.load_refs();
            }
        }
        if what & REFRESH_WORKDIR != 0 {
            if self.workdir_inflight {
                self.workdir_again = true;
            } else {
                self.load_workdir();
            }
        }
        self.repo_state = rebase::state_in(&self.repo.git_dir);
    }

    fn load_refs(&mut self) {
        self.refs_inflight = true;
        self.refs_generation += 1;
        let generation = self.refs_generation;
        let repo = self.repo.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = Refs::load(&repo.git);
            let _ = tx.send(AppEvent::Refs(generation, r));
        });
    }

    fn load_workdir(&mut self) {
        self.workdir_inflight = true;
        self.workdir_generation += 1;
        let generation = self.workdir_generation;
        let repo = self.repo.clone();
        let tx = self.tx.clone();
        let ctx = self.cfg.context_lines;
        std::thread::spawn(move || {
            let r = Workdir::load(&repo.git, ctx, DEFAULT_LINE_LIMIT);
            let _ = tx.send(AppEvent::Workdir(generation, Box::new(r)));
        });
    }

    fn start_history_walk(&mut self, tips: Vec<ObjectId>) {
        let generation = self.history.generation + 1;
        self.history.generation = generation;
        self.history_current.store(generation, Ordering::Relaxed);
        self.history.loading = true;
        self.history.replace_on_next = true;
        self.history.tips = tips.clone();
        if let Some(c) = self.history.selected_commit() {
            self.history.pending_select = Some(c.id);
        }
        let repo = self.repo.clone();
        let tx = self.tx.clone();
        let current = self.history_current.clone();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let r = log::walk(&repo.gix, tips, generation, &current, move |b| {
                let _ = tx2.send(AppEvent::History(b));
            });
            if let Err(e) = r {
                let _ = tx.send(AppEvent::HistoryError(generation, format!("{e:#}")));
            }
        });
    }

    // ----- Event handling -----

    pub fn handle(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Input(e) => self.handle_input(e),
            AppEvent::History(batch) => self.on_history(batch),
            AppEvent::HistoryError(generation, e) => {
                if generation == self.history.generation {
                    self.history.loading = false;
                    self.history.error = Some(e);
                }
            }
            AppEvent::Refs(generation, r) => {
                self.refs_inflight = false;
                if generation == self.refs_generation {
                    match r {
                        Ok(refs) => self.on_refs(refs),
                        Err(e) => self.set_status(format!("Reading refs failed: {e:#}"), Level::Error),
                    }
                }
                if std::mem::take(&mut self.refs_again) {
                    self.load_refs();
                }
            }
            AppEvent::Workdir(generation, r) => {
                self.workdir_inflight = false;
                if generation == self.workdir_generation {
                    match *r {
                        Ok(w) => self.on_workdir(w),
                        Err(e) => self.set_status(format!("Reading status failed: {e:#}"), Level::Error),
                    }
                }
                if std::mem::take(&mut self.workdir_again) {
                    self.load_workdir();
                }
            }
            AppEvent::Commit(generation, id, r) => {
                if generation == self.detail.request && self.detail.source.commit_id() == Some(id) {
                    self.detail.loading = false;
                    match *r {
                        Ok(c) => {
                            self.detail.set_commit(Arc::new(c));
                            self.rebuild_doc();
                        }
                        Err(e) => {
                            self.detail.commit = None;
                            // Replace the "Loading…" row.
                            self.rebuild_doc();
                            self.set_status(format!("Loading commit failed: {e:#}"), Level::Error);
                        }
                    }
                }
            }
            AppEvent::Highlight(key, hl) => {
                if key.generation == self.detail.generation {
                    self.detail
                        .highlights
                        .insert((key.section, key.file), Arc::new(hl));
                }
            }
            AppEvent::OpDone(r) => self.on_op_done(r),
            AppEvent::Progress(p) => {
                if let Some(b) = &mut self.busy {
                    b.progress = p;
                }
            }
            AppEvent::Watch(watch::Event::Changed(flags)) => {
                let mut what = 0;
                if flags & (watch::WORKTREE | watch::INDEX) != 0 {
                    what |= REFRESH_WORKDIR;
                }
                if flags & watch::REFS != 0 {
                    what |= REFRESH_REFS;
                }
                // While our own operation runs, its changes will be picked up
                // when it finishes.
                if self.busy.is_none() {
                    self.refresh(what);
                }
            }
            AppEvent::Watch(watch::Event::Degraded(msg)) => {
                self.watch_degraded = true;
                self.set_status(msg, Level::Warn);
            }
            AppEvent::AskPass(prompt, reply) => {
                let lower = prompt.to_lowercase();
                let secret = lower.contains("password") || lower.contains("passphrase") || lower.contains("token");
                let mut text = TextArea::new(false);
                text.masked = secret;
                self.overlays.push(cmd::Overlay::Input(cmd::InputBox {
                    title: "Git needs your input".into(),
                    hint: prompt,
                    text,
                    purpose: cmd::Purpose::AskPass(reply),
                }));
            }
            AppEvent::Tick => {
                if let Some((_, level, t)) = &self.status {
                    let ttl = if *level == Level::Error { 12 } else { 6 };
                    if t.elapsed() > Duration::from_secs(ttl) {
                        self.status = None;
                    }
                }
                if self.watch_degraded && self.busy.is_none() && self.last_poll.elapsed() > Duration::from_secs(3) {
                    self.last_poll = Instant::now();
                    self.refresh(REFRESH_WORKDIR);
                }
            }
        }
        self.update_animating();
    }

    fn on_refs(&mut self, refs: Refs) {
        let tips = refs.tips();
        self.labels = refs.labels();
        let changed_tips = {
            let mut a = tips.clone();
            let mut b = self.history.tips.clone();
            a.sort();
            b.sort();
            a != b
        };
        self.refs = refs;
        self.rebuild_sidebar();
        if changed_tips || self.history.generation == 0 {
            self.start_history_walk(tips);
        }
    }

    fn on_history(&mut self, batch: log::Batch) {
        if batch.generation != self.history.generation {
            return;
        }
        let h = &mut self.history;
        if h.replace_on_next {
            h.replace_on_next = false;
            h.error = None;
            h.commits.clear();
            h.rows.clear();
            h.index.clear();
        }
        let base = h.commits.len();
        for (i, c) in batch.commits.iter().enumerate() {
            h.index.insert(c.id, base + i);
        }
        h.commits.extend(batch.commits);
        h.rows.extend(batch.rows);
        if batch.done {
            h.loading = false;
        }
        if let Some(id) = h.pending_select {
            if let Some(&i) = h.index.get(&id) {
                h.pending_select = None;
                let changed = h.selected != i + 1;
                h.selected = i + 1;
                if changed {
                    self.ensure_history_visible();
                }
            } else if batch.done {
                // The commit is gone, usually because a rebase rewrote it.
                // Staying on the same row keeps the user near where they were.
                let row = h.selected;
                self.select_history(row);
            }
        }
    }

    fn on_workdir(&mut self, w: Workdir) {
        self.workdir = Some(w);
        self.repo_state = rebase::state_in(&self.repo.git_dir);
        if self.detail.source == doc::Source::Workdir {
            self.detail.reset_for_reload();
            self.rebuild_doc();
        }
    }

    // ----- Selection -----

    pub fn select_history(&mut self, row: usize) {
        let len = self.history.row_count();
        let row = row.min(len.saturating_sub(1));
        self.history.selected = row;
        self.history.pending_select = None;
        self.ensure_history_visible();
        if row == 0 {
            self.show_source(doc::Source::Workdir);
        } else if let Some(c) = self.history.commits.get(row - 1) {
            let id = c.id;
            self.show_source(doc::Source::Commit(id));
        }
    }

    pub fn ensure_history_visible(&mut self) {
        let h = usize::from(self.areas.history.height.max(1));
        let sel = self.history.selected.min(self.history.row_count().saturating_sub(1));
        if sel < self.history.scroll {
            self.history.scroll = sel;
        } else if sel >= self.history.scroll + h {
            self.history.scroll = sel + 1 - h;
        }
    }

    pub fn show_source(&mut self, source: doc::Source) {
        if self.detail.source == source {
            return;
        }
        let load = source.commit_id();
        self.detail.set_source(source);
        if let Some(id) = load {
            self.detail.loading = true;
            self.detail.request += 1;
            let _ = self.detail_tx.send((self.detail.request, id));
        }
        self.rebuild_doc();
    }

    /// Select a commit in the history by id, if it's loaded.
    pub fn jump_to(&mut self, id: ObjectId) -> bool {
        if let Some(&i) = self.history.index.get(&id) {
            self.select_history(i + 1);
            true
        } else {
            self.show_source(doc::Source::Commit(id));
            false
        }
    }

    pub fn rebuild_doc(&mut self) {
        // Highlighting requested for files as they were before is wasted.
        self.highlighter.set_generation(self.detail.generation);
        let workdir = self.workdir.as_ref();
        self.detail.rebuild(workdir, &self.labels, &self.theme, self.split_diff, &self.cfg);
    }

    pub fn rebuild_sidebar(&mut self) {
        self.sidebar.rows = cmd::sidebar_rows(&self.refs, &self.sidebar.collapsed);
        if self.sidebar.selected >= self.sidebar.rows.len() {
            self.sidebar.selected = self.sidebar.rows.len().saturating_sub(1);
        }
    }

    pub fn section_files(&self, sec: doc::Section) -> &[FileDiff] {
        doc::section_files(self.workdir.as_ref(), self.detail.commit.as_deref(), sec)
    }

    // ----- Operations -----

    /// Run an operation that changes the repository on a background thread.
    pub fn spawn_op(
        &mut self,
        label: impl Into<String>,
        after: After,
        f: impl FnOnce(&Repo, &dyn Fn(String)) -> Result<String> + Send + 'static,
    ) {
        let label = label.into();
        if let Some(b) = &self.busy {
            self.set_status(format!("Please wait: {} is still running", b.label), Level::Warn);
            return;
        }
        self.busy = Some(Busy {
            label: label.clone(),
            progress: String::new(),
            since: Instant::now(),
        });
        self.update_animating();
        let repo = self.repo.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let progress = move |s: String| {
                let _ = tx2.send(AppEvent::Progress(s));
            };
            let result = f(&repo, &progress);
            let _ = tx.send(AppEvent::OpDone(OpResult { label, result, after }));
        });
    }

    fn on_op_done(&mut self, r: OpResult) {
        self.busy = None;
        match r.result {
            Ok(msg) => {
                let msg = if msg.trim().is_empty() {
                    format!("{} finished", r.label)
                } else {
                    msg.trim().lines().last().unwrap_or("").to_string()
                };
                self.set_status(msg, Level::Ok);
                match r.after {
                    After::ClearMessage => {
                        self.message.clear();
                        self.amend = false;
                        self.message_before_amend = None;
                    }
                    After::OpenPlannerDone => self.planner = None,
                    After::SelectHead => {
                        self.history.pending_select = None;
                    }
                    After::Nothing => {}
                }
            }
            Err(e) => {
                self.set_status(format!("{} failed", r.label), Level::Error);
                self.show_error(&format!("{} failed", r.label), &e);
            }
        }
        self.refresh(REFRESH_ALL);
    }

    pub fn sidebar_shown(&self) -> bool {
        self.sidebar_visible.unwrap_or(self.size.0 >= 100)
    }

    pub fn toggle_amend(&mut self) {
        self.amend = !self.amend;
        if self.amend {
            if self.message.is_empty()
                && let Ok(msg) = crate::git::ops::commit_message(&self.repo.git, "HEAD")
            {
                self.message_before_amend = Some(self.message.text());
                self.message.set_text(&msg);
            }
        } else if let Some(prev) = self.message_before_amend.take() {
            self.message.set_text(&prev);
        }
    }

    pub fn copy_to_clipboard(&mut self, text: &str) {
        crate::terminal::write_raw(&format!("\x1b]52;c;{}\x07", crate::util::base64(text.as_bytes())));
        self.set_status(format!("Copied {text}"), Level::Info);
    }

    /// Request highlighting for a file that just became visible.
    pub fn request_highlight(&mut self, sec: doc::Section, file: usize) {
        let key = (sec as u8, file);
        if self.detail.highlights.contains_key(&key) || !self.detail.requested.insert(key) {
            return;
        }
        let files = self.section_files(sec);
        let Some(f) = files.get(file) else { return };
        if f.hunks.is_empty() {
            return;
        }
        let texts = doc::section_texts(self.workdir.as_ref(), self.detail.commit.as_deref(), sec)
            .and_then(|t| t.get(file))
            .cloned()
            .unwrap_or_default();
        self.highlighter.request(highlight::Job {
            key: highlight::Key {
                generation: self.detail.generation,
                section: sec as u8,
                file,
            },
            path: f.path().to_string(),
            hunks: f.hunks.clone(),
            old: texts.old,
            new: texts.new,
        });
    }
}

/// A thread that loads commit details, skipping requests that were replaced
/// by newer ones while it was busy.
fn spawn_detail_worker(repo: Arc<Repo>, tx: Sender<AppEvent>, context: u32) -> Sender<(u64, ObjectId)> {
    let (req_tx, req_rx) = unbounded::<(u64, ObjectId)>();
    std::thread::Builder::new()
        .name("detail".into())
        .spawn(move || {
            let local = repo.gix.to_thread_local();
            while let Ok(mut req) = req_rx.recv() {
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let r = crate::git::commit::load(&local, req.1, context, DEFAULT_LINE_LIMIT);
                if tx.send(AppEvent::Commit(req.0, req.1, Box::new(r))).is_err() {
                    break;
                }
            }
        })
        .ok();
    req_tx
}
