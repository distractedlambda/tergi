//! Random sessions in the UI.
//!
//! Each case opens the app on a small repository with branches, a merge,
//! a tag, a stash, a remote and some changes. Then it takes random steps:
//! keys, clicks on whatever is on screen, right clicks, drags, scrolls,
//! pastes, resizes and commands from the palette. Between them it edits
//! files and runs git behind the app's back, and sometimes waits for the
//! work in the background to finish.
//!
//! There's no model of what should happen. After each step the test only
//! checks that nothing panicked, that the frame drew, and that the state
//! hangs together: selections and cursors point at rows that exist, and
//! only the top dialog can be clicked.
//!
//! The remote is a bare repository next to the working tree, so fetch,
//! pull and push stay on the local disk. Git never opens an editor here,
//! and the app leaves the clipboard and the editor to the main loop, so
//! every command can run.

mod common;

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use common::Scratch;
use common::settings::{Settings, settings};
use common::ui::Ui;
use tergi::app::cmd::{Cmd, Overlay};
use tergi::app::doc::Source;
use tergi::app::input::{Target, palette_filter};
use tergi::app::{AppEvent, Focus, REFRESH_ALL};

/// Files that the steps write and delete. Some of them are in the
/// repository from the start.
const PATHS: &[&str] = &[
    "a.txt",
    "b.txt",
    "dir/c.txt",
    "dir/sub/d.txt",
    "new.txt",
    "bin.dat",
];

/// Git commands that the steps run behind the app's back. Many of them
/// fail in some states, which is fine.
const GIT: &[&[&str]] = &[
    &["add", "-A"],
    &["reset", "-q"],
    &["commit", "-qam", "Commit behind the app's back"],
    &[
        "commit",
        "-q",
        "--amend",
        "-m",
        "Amended behind the app's back",
    ],
    &["checkout", "-q", "main"],
    &["checkout", "-q", "-B", "side"],
    &["checkout", "-q", "--detach", "HEAD~1"],
    &["branch", "-f", "topic", "HEAD~1"],
    &["branch", "-D", "side"],
    &[
        "stash",
        "push",
        "-qu",
        "-m",
        "stashed behind the app's back",
    ],
    &["stash", "pop", "-q"],
    &["stash", "drop", "-q"],
    &["tag", "-f", "v2"],
    &["tag", "-d", "v2"],
    &["reset", "-q", "--hard", "HEAD~1"],
    // Many commands refuse to run over local changes, so sometimes there
    // are none.
    &["reset", "-q", "--hard"],
    &["clean", "-fdq"],
    &["merge", "-q", "--no-edit", "conflicting"],
    &["merge", "--abort"],
    &["rebase", "--abort"],
    &["push", "-q", "origin", "HEAD:main"],
    &["fetch", "-q", "origin"],
    &["pack-refs", "--all"],
];

#[derive(Debug, Clone)]
enum Content {
    Fixed(&'static str),
    /// The first `n` lines of `a.txt` as committed, with one changed.
    Lines(usize, Index),
    /// Enough lines that the diff gets long.
    Long(usize),
}

#[derive(Debug, Clone)]
enum Step {
    Key(KeyCode, KeyModifiers),
    Type(&'static str),
    Paste(&'static str),
    /// Click somewhere inside one of the regions in `App::hits`.
    Click {
        hit: Index,
        at: Index,
        button: MouseButton,
        double: bool,
        shift: bool,
    },
    /// Click anywhere on the screen, which may hit nothing.
    ClickAnywhere(Index, Index, MouseButton),
    /// Press on a region, drag through some points, and let go.
    Drag {
        hit: Index,
        to: Vec<(Index, Index)>,
    },
    Scroll {
        x: Index,
        y: Index,
        down: bool,
        shift: bool,
    },
    Resize(u16, u16),
    /// Open the palette and run one of its commands.
    Palette(Index),
    FocusGained,
    Write(usize, Content),
    Remove(usize),
    RemoveDir,
    Git(usize),
    /// Wait for the work in the background to finish.
    Wait,
}

fn key() -> impl Strategy<Value = (KeyCode, KeyModifiers)> {
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    let alt = KeyModifiers::ALT;
    let shift = KeyModifiers::SHIFT;
    // Every key with a binding somewhere, and a few without.
    let chars = "?pFPStw@123.qjkgGhlrsfeidAbTcCVXy auvzZ[]{}oOMnRDg/x";
    let plain: Vec<(KeyCode, KeyModifiers)> = chars
        .chars()
        .map(|c| (KeyCode::Char(c), none))
        .chain(
            [
                KeyCode::Enter,
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::PageUp,
                KeyCode::PageDown,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Backspace,
                KeyCode::Delete,
                KeyCode::Menu,
            ]
            .map(|k| (k, none)),
        )
        .collect();
    let special = vec![
        (KeyCode::Char('p'), ctrl),
        (KeyCode::Char('n'), ctrl),
        (KeyCode::Char('k'), ctrl),
        (KeyCode::Char('j'), ctrl),
        (KeyCode::Char('d'), ctrl),
        (KeyCode::Char('u'), ctrl),
        (KeyCode::Char('b'), ctrl),
        (KeyCode::Char('r'), ctrl),
        (KeyCode::Char('z'), ctrl),
        (KeyCode::Char('c'), ctrl),
        (KeyCode::Char('s'), ctrl),
        (KeyCode::Char('a'), ctrl),
        (KeyCode::Char('e'), ctrl),
        (KeyCode::Enter, ctrl),
        (KeyCode::Char('a'), alt),
        (KeyCode::Char('c'), alt),
        (KeyCode::Char('s'), alt),
        (KeyCode::Down, shift),
        (KeyCode::Up, shift),
        (KeyCode::Tab, shift),
        (KeyCode::F(10), shift),
    ];
    prop_oneof![
        4 => prop::sample::select(plain),
        1 => prop::sample::select(special),
    ]
}

fn text() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "fix",
        "new-branch",
        "v2",
        "a b",
        "origin/main",
        "日本語 ✓",
        "3",
        "0",
        "12",
        "..",
        "-x",
        "line one\nline two",
    ])
}

fn content() -> impl Strategy<Value = Content> {
    prop_oneof![
        prop::sample::select(vec![
            "",
            "one\n",
            "one\ntwo\nthree\n",
            "no newline at the end",
            "crlf\r\nline ends\r\n",
            "\ttabs\tand wide 日本語 ✓\n",
            "\0binary\0data\n",
        ])
        .prop_map(Content::Fixed),
        (1usize..40, any::<Index>()).prop_map(|(n, i)| Content::Lines(n, i)),
        (100usize..400).prop_map(Content::Long),
    ]
}

fn point() -> impl Strategy<Value = (Index, Index)> {
    (any::<Index>(), any::<Index>())
}

fn button() -> impl Strategy<Value = MouseButton> {
    prop_oneof![3 => Just(MouseButton::Left), 1 => Just(MouseButton::Right)]
}

fn size() -> impl Strategy<Value = (u16, u16)> {
    prop_oneof![
        1 => (1u16..=160, 1u16..=50),
        3 => (60u16..=200, 16u16..=60),
        1 => Just((140, 36)),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => key().prop_map(|(k, m)| Step::Key(k, m)),
        2 => text().prop_map(Step::Type),
        1 => text().prop_map(Step::Paste),
        12 => (any::<Index>(), any::<Index>(), button(), prop::bool::weighted(0.2), prop::bool::weighted(0.2))
            .prop_map(|(hit, at, button, double, shift)| Step::Click { hit, at, button, double, shift }),
        1 => (any::<Index>(), any::<Index>(), button())
            .prop_map(|(x, y, b)| Step::ClickAnywhere(x, y, b)),
        2 => (any::<Index>(), prop::collection::vec(point(), 1..4))
            .prop_map(|(hit, to)| Step::Drag { hit, to }),
        2 => (any::<Index>(), any::<Index>(), any::<bool>(), prop::bool::weighted(0.2))
            .prop_map(|(x, y, down, shift)| Step::Scroll { x, y, down, shift }),
        1 => size().prop_map(|(w, h)| Step::Resize(w, h)),
        5 => any::<Index>().prop_map(Step::Palette),
        1 => Just(Step::FocusGained),
        3 => (0..PATHS.len(), content()).prop_map(|(p, c)| Step::Write(p, c)),
        1 => (0..PATHS.len()).prop_map(Step::Remove),
        1 => Just(Step::RemoveDir),
        3 => (0..GIT.len()).prop_map(Step::Git),
        3 => Just(Step::Wait),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    size: (u16, u16),
    steps: Vec<Step>,
    settings: Settings,
}

fn case() -> impl Strategy<Value = Case> {
    (size(), prop::collection::vec(step(), 1..60), settings()).prop_map(
        |(size, steps, settings)| Case {
            size,
            steps,
            settings,
        },
    )
}

/// The lines of `a.txt` as first committed.
fn a_lines(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("line {i}")).collect()
}

/// A repository with a little of everything that the panes show.
fn build(settings: &Settings) -> Scratch {
    let s = Scratch::in_folder(b"repo");
    for (key, value) in &settings.0 {
        s.sh(&["config", key, value]);
    }
    let origin = s.dir.path().join("origin.git");
    let origin = origin.to_str().expect("UTF-8 temporary path");
    s.sh(&["init", "-q", "--bare", origin]);
    s.sh(&["remote", "add", "origin", origin]);

    s.write("a.txt", &(a_lines(30).join("\n") + "\n"));
    s.write("dir/c.txt", "c\n");
    std::fs::write(s.file("bin.dat"), b"\0\x01\x02binary").expect("write");
    s.commit_all("Add files");
    s.sh(&["tag", "-a", "v1", "-m", "Version one"]);
    s.write("a.txt", &(a_lines(32).join("\n") + "\n"));
    s.commit_all("Add two lines");
    s.sh(&["push", "-q", "-u", "origin", "main"]);

    // A merge commit, and a branch that conflicts with main.
    s.sh(&["checkout", "-q", "-b", "topic"]);
    s.write("dir/c.txt", "c changed on topic\n");
    s.commit_all("Change c on topic");
    s.sh(&["checkout", "-q", "main"]);
    s.write("b.txt", "b\n");
    s.commit_all("Add b");
    s.sh(&["merge", "-q", "--no-ff", "--no-edit", "topic"]);
    s.sh(&["checkout", "-q", "-b", "conflicting", "HEAD~1"]);
    s.write("a.txt", "a different start\n");
    s.commit_all("Rewrite a");
    s.sh(&["checkout", "-q", "main"]);

    s.write("a.txt", "stashed\n");
    s.sh(&["stash", "push", "-q", "-m", "saved work"]);

    // Changes of every kind.
    let mut lines = a_lines(32);
    lines[3] = "changed".into();
    lines.insert(20, "inserted".into());
    s.write("a.txt", &(lines.join("\n") + "\n"));
    s.write("dir/c.txt", "staged\n");
    s.sh(&["add", "dir/c.txt"]);
    s.write("new.txt", "untracked\n");
    s
}

/// Run git and ignore whether it worked.
fn try_git(s: &Scratch, args: &[&str]) {
    let _ = std::process::Command::new("git")
        .current_dir(s.path())
        .env("GIT_EDITOR", "true")
        .args(args)
        .output();
}

fn write(s: &Scratch, path: &str, c: &Content) {
    let text = match c {
        Content::Fixed(t) => (*t).to_string(),
        Content::Lines(n, i) => {
            let mut lines = a_lines(*n);
            lines[i.index(*n)] = "changed".into();
            lines.join("\n") + "\n"
        }
        Content::Long(n) => (0..*n).map(|i| format!("long line {i}\n")).collect(),
    };
    let p = s.file(path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, text);
}

/// A point inside `r`.
fn inside(r: Rect, at: &Index) -> (u16, u16) {
    let n = usize::from(r.width.max(1)) * usize::from(r.height.max(1));
    let i = at.index(n);
    let w = usize::from(r.width.max(1));
    (r.x + (i % w) as u16, r.y + (i / w) as u16)
}

fn mouse(ui: &mut Ui, kind: MouseEventKind, (x, y): (u16, u16), shift: bool) {
    let modifiers = if shift {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    let ev = MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers,
    };
    ui.handle(AppEvent::Input(Event::Mouse(ev)));
}

/// Handle what has arrived from the background, as the main loop does
/// between frames. This doesn't wait for anything.
fn drain(ui: &mut Ui) {
    while let Some(ev) = ui.app.try_recv() {
        ui.handle(ev);
    }
}

/// Handle events until nothing runs in the background and no events are
/// waiting.
fn wait_quiet(ui: &mut Ui) -> Result<(), TestCaseError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        drain(ui);
        ui.draw();
        if ui.app.idle() {
            return Ok(());
        }
        let a = &ui.app;
        prop_assert!(
            Instant::now() < deadline,
            "the app didn't settle: busy {}, history loading {}, commit loading {}\n{}",
            a.busy.is_some(),
            a.history.loading,
            a.detail.loading,
            ui.screen()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_step(ui: &mut Ui, s: &Scratch, step: &Step) -> Result<(), TestCaseError> {
    let screen = |ui: &Ui| {
        let a = ui.term.backend().buffer().area;
        (a.width, a.height)
    };
    match step {
        Step::Key(code, mods) => {
            ui.handle(AppEvent::Input(Event::Key(KeyEvent::new(*code, *mods))));
        }
        Step::Type(t) => {
            for c in t.chars() {
                let k = if c == '\n' {
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
                } else {
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
                };
                ui.handle(AppEvent::Input(Event::Key(k)));
            }
        }
        Step::Paste(t) => ui.handle(AppEvent::Input(Event::Paste((*t).to_string()))),
        Step::Click {
            hit,
            at,
            button,
            double,
            shift,
        } => {
            if ui.app.hits.is_empty() {
                return Ok(());
            }
            let r = ui.app.hits[hit.index(ui.app.hits.len())].0;
            let p = inside(r, at);
            let presses = if *double { 2 } else { 1 };
            for _ in 0..presses {
                mouse(ui, MouseEventKind::Down(*button), p, *shift);
                mouse(ui, MouseEventKind::Up(*button), p, *shift);
            }
        }
        Step::ClickAnywhere(x, y, button) => {
            let (w, h) = screen(ui);
            let p = (
                x.index(usize::from(w)) as u16,
                y.index(usize::from(h)) as u16,
            );
            mouse(ui, MouseEventKind::Down(*button), p, false);
            mouse(ui, MouseEventKind::Up(*button), p, false);
        }
        Step::Drag { hit, to } => {
            if ui.app.hits.is_empty() {
                return Ok(());
            }
            let r = ui.app.hits[hit.index(ui.app.hits.len())].0;
            let from = (r.x, r.y);
            mouse(ui, MouseEventKind::Down(MouseButton::Left), from, false);
            ui.draw();
            let (w, h) = screen(ui);
            let mut last = from;
            for (x, y) in to {
                // Drags can go a little past the edges of the screen.
                last = (
                    x.index(usize::from(w) + 2) as u16,
                    y.index(usize::from(h) + 2) as u16,
                );
                mouse(ui, MouseEventKind::Drag(MouseButton::Left), last, false);
                ui.draw();
            }
            mouse(ui, MouseEventKind::Up(MouseButton::Left), last, false);
        }
        Step::Scroll { x, y, down, shift } => {
            let (w, h) = screen(ui);
            let p = (
                x.index(usize::from(w)) as u16,
                y.index(usize::from(h)) as u16,
            );
            let kind = if *down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            };
            mouse(ui, kind, p, *shift);
        }
        Step::Resize(w, h) => {
            ui.term = Terminal::new(TestBackend::new(*w, *h)).expect("terminal");
            ui.handle(AppEvent::Input(Event::Resize(*w, *h)));
        }
        Step::Palette(i) => {
            // The palette opens from anywhere but a dialog.
            if !ui.app.overlays.is_empty() {
                return Ok(());
            }
            ui.run_cmd(Cmd::Palette);
            ui.draw();
            let n = match ui.app.overlays.last() {
                Some(Overlay::Palette { items, .. }) => palette_filter(items, "").len(),
                _ => 0,
            };
            prop_assert!(n > 0, "the palette is empty\n{}", ui.screen());
            for _ in 0..i.index(n) {
                ui.handle(AppEvent::Input(Event::Key(KeyEvent::new(
                    KeyCode::Down,
                    KeyModifiers::NONE,
                ))));
            }
            ui.draw();
            ui.handle(AppEvent::Input(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))));
        }
        Step::FocusGained => ui.handle(AppEvent::Input(Event::FocusGained)),
        Step::Write(p, c) => write(s, PATHS[*p], c),
        Step::Remove(p) => {
            let _ = std::fs::remove_file(s.file(PATHS[*p]));
        }
        Step::RemoveDir => {
            let _ = std::fs::remove_dir_all(s.file("dir"));
        }
        Step::Git(i) => try_git(s, GIT[*i]),
        Step::Wait => wait_quiet(ui)?,
    }
    // The main loop quits, suspends or opens the editor here. The test
    // carries on instead, and refreshes as the loop does after an editor.
    ui.app.quit = false;
    ui.app.clipboard = None;
    if ui.app.suspend.take().is_some() {
        ui.app.refresh(REFRESH_ALL);
    }
    drain(ui);
    ui.draw();
    Ok(())
}

/// Check that the state hangs together after a step.
fn check_state(ui: &Ui) -> Result<(), TestCaseError> {
    let a = &ui.app;
    let screen = || ui.screen();

    let h = &a.history;
    prop_assert_eq!(h.index.len(), h.commits.len());
    prop_assert_eq!(h.rows.len(), h.commits.len());
    for (i, c) in h.commits.iter().enumerate() {
        prop_assert_eq!(h.index.get(&c.id), Some(&i));
    }
    // While a walk runs, the selected commit may not be in the list yet.
    if !h.loading && h.error.is_none() {
        prop_assert!(
            h.selected_row().is_some(),
            "the selected commit {:?} isn't in the history\n{}",
            h.selected,
            screen()
        );
    }

    let d = &a.detail;
    let last = d.rows.len().saturating_sub(1);
    prop_assert!(
        d.cursor <= last,
        "the cursor is on row {} of {}\n{}",
        d.cursor,
        d.rows.len(),
        screen()
    );
    if let Some(anchor) = d.anchor {
        prop_assert!(
            anchor <= last,
            "the selection starts on row {anchor} of {}\n{}",
            d.rows.len(),
            screen()
        );
    }

    prop_assert!(
        a.sidebar.selected < a.sidebar.rows.len(),
        "sidebar row {} of {} is selected\n{}",
        a.sidebar.selected,
        a.sidebar.rows.len(),
        screen()
    );

    if let Some(p) = &a.planner {
        prop_assert!(
            p.selected < p.rows.len(),
            "planner row {} of {} is selected\n{}",
            p.selected,
            p.rows.len(),
            screen()
        );
    }

    for o in &a.overlays {
        match o {
            Overlay::Menu(m) => prop_assert!(
                m.selected < m.items.len(),
                "menu item {} of {} is selected\n{}",
                m.selected,
                m.items.len(),
                screen()
            ),
            Overlay::Palette {
                input,
                items,
                selected,
            } => {
                let n = palette_filter(items, &input.text()).len();
                prop_assert!(
                    *selected < n.max(1),
                    "palette item {selected} of {n} is selected\n{}",
                    screen()
                );
            }
            _ => {}
        }
    }

    // Only the top dialog can be clicked. Drawing a dialog clears the
    // hits recorded before it, so nothing behind it should be left.
    if !a.overlays.is_empty() {
        for (_, t) in &a.hits {
            prop_assert!(
                matches!(
                    t,
                    Target::Button(_)
                        | Target::MenuItem(_)
                        | Target::PaletteItem(_)
                        | Target::InputText
                        | Target::Overlay
                ),
                "{t:?} can be clicked behind a dialog\n{}",
                screen()
            );
        }
    }
    if a.focus == Focus::Message {
        prop_assert!(
            a.detail.source == Source::Workdir,
            "the message box has focus but isn't shown\n{}",
            screen()
        );
    }
    Ok(())
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let s = build(&case.settings);
    let mut ui = Ui::open(&s);
    ui.term = Terminal::new(TestBackend::new(case.size.0, case.size.1)).expect("terminal");
    wait_quiet(&mut ui)?;
    check_state(&ui)?;
    for (i, step) in case.steps.iter().enumerate() {
        run_step(&mut ui, &s, step)
            .map_err(|e| TestCaseError::fail(format!("step {i} ({step:?}): {e}")))?;
        check_state(&ui)
            .map_err(|e| TestCaseError::fail(format!("after step {i} ({step:?}): {e}")))?;
    }
    // Whatever is still running has to finish cleanly too.
    wait_quiet(&mut ui)?;
    check_state(&ui)
}

/// Run `cases` cases, unless `PROPTEST_CASES` asks for some other number.
/// `ProptestConfig::with_cases` would ignore the variable.
fn config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = cases;
    }
    config
}

// A session spends most of its time waiting for git, so one test would
// leave most cores idle. The test harness runs separate tests in
// parallel, so the cases are split over several of them. Each one picks
// its own random cases. `PROPTEST_CASES` sets the number for each test.
macro_rules! random_sessions {
    ($($name:ident)*) => {
        proptest! {
            #![proptest_config(config(8))]
            $(
                #[test]
                fn $name(case in case()) {
                    check(&case)?;
                }
            )*
        }
    };
}

random_sessions!(
    random_sessions_1 random_sessions_2 random_sessions_3 random_sessions_4
    random_sessions_5 random_sessions_6 random_sessions_7 random_sessions_8
);
