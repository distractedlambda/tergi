//! Drive the whole app against a scratch repository, drawing into a test
//! backend and sending it the same mouse and key events a terminal would.

use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use tergi::app::cmd::Cmd;
use tergi::app::doc::Row;
use tergi::app::input::Target;
use tergi::app::{App, AppEvent, Repo};
use tergi::config::Config;
use tergi::ui::draw::draw;

use super::Scratch;

pub struct Ui {
    pub app: App,
    pub term: Terminal<TestBackend>,
    // Declared after `app`, so the app and its tasks go away first.
    rt: tokio::runtime::Runtime,
}

impl Ui {
    pub fn open(s: &Scratch) -> Ui {
        Ui::open_with_config(s, None)
    }

    /// Open the app with a config file, which it saves settings to and
    /// watches.
    pub fn open_with_config(s: &Scratch, config: Option<&Path>) -> Ui {
        let mut repo = Repo::open(s.path()).expect("open repo");
        // Git calls back into tergi during a rebase. Here the current
        // executable is the test runner, so point it at the real one.
        repo.exe = env!("CARGO_BIN_EXE_tergi").into();
        // The app's tasks run on the runtime's own threads, so the tests can
        // stay synchronous and poll the app with `try_recv`.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = {
            let _in_rt = rt.enter();
            let cfg = config.map(|p| Config::load(p).0).unwrap_or_default();
            let mut app = App::new(repo, cfg, None);
            app.config_path = config.map(Path::to_path_buf);
            app.start();
            app
        };
        let term = Terminal::new(TestBackend::new(140, 36)).expect("terminal");
        Ui { app, term, rt }
    }

    /// Handle one event, as the main loop does. Anything the app does in
    /// the background is still running afterwards, so use `wait` for it.
    pub fn handle(&mut self, ev: AppEvent) {
        self.app.handle(ev);
    }

    pub fn run_cmd(&mut self, cmd: Cmd) {
        self.app.run_cmd(cmd);
    }

    /// Select a row in the history and wait for its commit to load.
    pub fn select_commit(&mut self, row: usize) {
        self.app.select_history(row);
        self.wait("the commit to load", |u| u.app.detail.commit.is_some());
    }

    pub fn doc_row(&self, f: impl Fn(&Row) -> bool) -> Rect {
        let i = self
            .app
            .detail
            .rows
            .iter()
            .position(f)
            .expect("row in the detail pane");
        self.find_hit(|t| *t == Target::DocRow(i))
    }

    pub fn draw(&mut self) {
        let app = &mut self.app;
        self.term.draw(|f| draw(app, f)).expect("draw");
    }

    /// Handle events and redraw until `done` returns true.
    pub fn wait(&mut self, what: &str, done: impl Fn(&Ui) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            while let Some(ev) = self.app.try_recv() {
                self.handle(ev);
            }
            self.draw();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n{}",
                self.screen()
            );
            if let Some(ev) = self.next_event(Duration::from_millis(10)) {
                self.handle(ev);
            }
        }
    }

    /// Wait up to `timeout` for the next event, without handling it. This
    /// returns as soon as one arrives, so the tests don't sleep for longer
    /// than they have to.
    pub fn next_event(&mut self, timeout: Duration) -> Option<AppEvent> {
        let app = &mut self.app;
        // The timer has to be made inside the runtime.
        self.rt
            .block_on(async { tokio::time::timeout(timeout, app.next()).await })
            .ok()
            .flatten()
    }

    /// Wait until the history and the working directory have both loaded.
    pub fn wait_loaded(&mut self, commits: usize) {
        self.wait("the first load", |u| {
            !u.app.history.loading
                && u.app.history.commits.len() == commits
                && u.app.workdir.is_some()
        });
    }

    pub fn screen(&self) -> String {
        let buf = self.term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    pub fn find_hit(&self, f: impl Fn(&Target) -> bool) -> Rect {
        self.app
            .hits
            .iter()
            .find(|(_, t)| f(t))
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("no such target on screen\n{}", self.screen()))
    }

    pub fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        let ev = MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        self.handle(AppEvent::Input(Event::Mouse(ev)));
        self.draw();
    }

    pub fn click(&mut self, button: MouseButton, r: Rect) {
        self.mouse(MouseEventKind::Down(button), r.x, r.y);
        self.mouse(MouseEventKind::Up(button), r.x, r.y);
    }

    pub fn key(&mut self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    pub fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.handle(AppEvent::Input(Event::Key(KeyEvent::new(code, modifiers))));
        self.draw();
    }

    /// Run a command from the command palette.
    pub fn palette(&mut self, search: &str) {
        self.key_with(KeyCode::Char('p'), KeyModifiers::CONTROL);
        self.type_text(search);
        self.key(KeyCode::Enter);
    }

    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    /// Wait until the running operation is done. The reloads it causes
    /// may still be on their way.
    pub fn settle(&mut self) {
        self.wait("the operation to finish", |u| u.app.busy.is_none());
    }
}
