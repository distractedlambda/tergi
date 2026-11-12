//! Drive the whole app against scratch repositories, drawing into a test
//! backend and sending it the same mouse and key events a terminal would.

mod common;

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use common::Scratch;
use tergi::app::cmd::{Cmd, Overlay, Unit};
use tergi::app::input::Target;
use tergi::app::{App, AppEvent, Repo};
use tergi::config::Config;
use tergi::ui::draw::draw;

struct Ui {
    app: App,
    term: Terminal<TestBackend>,
}

impl Ui {
    fn open(s: &Scratch) -> Ui {
        let repo = Repo::open(s.path()).expect("open repo");
        let mut app = App::new(repo, Config::default(), None);
        app.start();
        let term = Terminal::new(TestBackend::new(140, 36)).expect("terminal");
        Ui { app, term }
    }

    fn draw(&mut self) {
        let app = &mut self.app;
        self.term.draw(|f| draw(app, f)).expect("draw");
    }

    /// Handle events and redraw until `done` returns true.
    fn wait(&mut self, what: &str, done: impl Fn(&Ui) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            while let Some(ev) = self.app.try_recv() {
                self.app.handle(ev);
            }
            self.draw();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the history and the working directory have both loaded.
    fn wait_loaded(&mut self, commits: usize) {
        self.wait("the first load", |u| {
            !u.app.history.loading && u.app.history.commits.len() == commits && u.app.workdir.is_some()
        });
    }

    fn screen(&self) -> String {
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

    fn find_hit(&self, f: impl Fn(&Target) -> bool) -> Rect {
        self.app
            .hits
            .iter()
            .find(|(_, t)| f(t))
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("no such target on screen\n{}", self.screen()))
    }

    fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
        let ev = MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        self.app.handle(AppEvent::Input(Event::Mouse(ev)));
        self.draw();
    }

    fn click(&mut self, button: MouseButton, r: Rect) {
        self.mouse(MouseEventKind::Down(button), r.x, r.y);
        self.mouse(MouseEventKind::Up(button), r.x, r.y);
    }

    fn key(&mut self, code: KeyCode) {
        self.app.handle(AppEvent::Input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))));
        self.draw();
    }
}

fn repo_with_change() -> Scratch {
    let s = Scratch::new();
    s.write("notes.txt", "one\ntwo\nthree\n");
    s.commit_all("Add notes");
    s.write("notes.txt", "one\ntwo\nthree\nfour\n");
    s.commit_all("Add a fourth note");
    s.write("notes.txt", "one\n2\nthree\nfour\n");
    s
}

#[test]
fn shows_history_and_working_directory() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let screen = ui.screen();
    for text in ["Working directory · 1 change", "Add a fourth note", "Add notes", "Unstaged changes (1)", "notes.txt"] {
        assert!(screen.contains(text), "missing {text:?}\n{screen}");
    }
}

#[test]
fn clicking_stage_stages_the_file() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let r = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Stage(Unit::File(..)))));
    ui.click(MouseButton::Left, r);
    ui.wait("the file to show as staged", |u| u.screen().contains("Staged changes (1)"));
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "notes.txt");
    assert_eq!(s.sh(&["diff", "--name-only"]).trim(), "");
}

#[test]
fn right_click_on_a_commit_opens_its_menu() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    // Row 1 is the newest commit. Row 0 is the working directory.
    let r = ui.find_hit(|t| matches!(t, Target::HistoryRow(1)));
    ui.click(MouseButton::Right, r);
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Menu(_))));
    let screen = ui.screen();
    assert!(screen.contains("Reword"), "{screen}");
    assert!(screen.contains("Squash into parent"), "{screen}");
    // Clicking outside a menu closes it.
    ui.click(MouseButton::Left, Rect::new(0, 35, 1, 1));
    assert!(ui.app.overlays.is_empty());
}

#[test]
fn clicks_behind_a_dialog_are_ignored() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let stash = ui.find_hit(|t| matches!(t, Target::Button(Cmd::StashPush)));

    // A click outside a menu only closes the menu.
    ui.key(KeyCode::Char('?'));
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Menu(_))));
    ui.click(MouseButton::Left, stash);
    assert!(ui.app.overlays.is_empty());

    // A click outside a dialog does nothing at all.
    ui.key(KeyCode::Char('S'));
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Input(_))));
    ui.click(MouseButton::Left, stash);
    assert_eq!(ui.app.overlays.len(), 1);
}
