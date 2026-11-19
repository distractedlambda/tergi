//! Drive the whole app against scratch repositories, drawing into a test
//! backend and sending it the same mouse and key events a terminal would.

use crate::common;

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use common::Scratch;
use common::ui::Ui;
use tergi::app::cmd::{Cmd, Overlay, SideRow, Unit};
use tergi::app::doc::{Row, Section};
use tergi::app::input::Target;
use tergi::app::planner::PlanAction;
use tergi::app::{After, AppEvent};
use tergi::highlight::Kind;
use tergi::ui::draw::draw;
use tergi::util::fs_path;

fn repo_with_change() -> Scratch {
    let s = Scratch::new();
    write_change(&s);
    s
}

/// Two commits, and a change on top of them.
fn write_change(s: &Scratch) {
    s.write("notes.txt", "one\ntwo\nthree\n");
    s.commit_all("Add notes");
    s.write("notes.txt", "one\ntwo\nthree\nfour\n");
    s.commit_all("Add a fourth note");
    s.write("notes.txt", "one\n2\nthree\nfour\n");
}

#[test]
fn shows_history_and_working_directory() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let screen = ui.screen();
    for text in [
        "Working directory · 1 change",
        "Add a fourth note",
        "Add notes",
        "Unstaged changes (1)",
        "notes.txt",
    ] {
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
    ui.wait("the file to show as staged", |u| {
        u.screen().contains("Staged changes (1)")
    });
    assert_eq!(
        s.sh(&["diff", "--cached", "--name-only"]).trim(),
        "notes.txt"
    );
    assert_eq!(s.sh(&["diff", "--name-only"]).trim(), "");
}

/// Files whose names aren't UTF-8, in a repository whose path isn't
/// either. Two of the names look the same on screen, so the app has to
/// tell them apart by their bytes.
#[test]
fn stages_and_discards_files_whose_names_are_not_utf8() {
    if !common::non_utf8_file_names() {
        eprintln!("skipped: this file system rejects names that aren't UTF-8");
        return;
    }
    let s = Scratch::in_folder(b"caf\xe9");
    let put = |name: &[u8], text: &str| {
        std::fs::write(s.path().join(fs_path(name)), text).expect("write");
    };
    put(b"caf\xe9.txt", "one\n");
    s.commit_all("init");
    put(b"caf\xe9.txt", "two\n");
    put(b"\xfe.txt", "a\n");
    put(b"\xff.txt", "b\n");
    let git_z = |args: &[&str]| -> Vec<Vec<u8>> {
        let out = std::process::Command::new("git")
            .current_dir(s.path())
            .args(args)
            .output()
            .expect("run git");
        let mut v: Vec<Vec<u8>> = out.stdout.split(|&b| b == 0).map(<[u8]>::to_vec).collect();
        v.retain(|f| !f.is_empty());
        v
    };

    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    assert!(ui.screen().contains("caf\u{fffd}.txt"), "{}", ui.screen());
    let r = ui.find_hit(|t| {
        matches!(t, Target::Button(Cmd::Stage(Unit::File(_, p))) if p == b"\xff.txt".as_slice())
    });
    ui.click(MouseButton::Left, r);
    ui.settle();
    assert_eq!(
        git_z(&["diff", "--cached", "--name-only", "-z"]),
        [b"\xff.txt"]
    );

    ui.wait("the staged file", |u| {
        u.screen().contains("Staged changes (1)")
    });
    ui.key(KeyCode::Char('3'));
    ask_to_discard(
        &mut ui,
        |ui, r| matches!(r, Row::File { sec: Section::Unstaged, file } if ui.app.section_files(Section::Unstaged)[*file].path() == b"caf\xe9.txt".as_slice()),
    );
    assert!(
        ui.screen()
            .contains("unstaged changes to caf\u{fffd}.txt will be"),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(
        std::fs::read(s.path().join(fs_path(b"caf\xe9.txt"))).expect("read"),
        b"one\n"
    );
    assert_eq!(git_z(&["ls-files", "-o", "-z"]), [b"\xfe.txt"]);
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

#[test]
fn menus_from_the_keyboard_open_in_the_middle() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let push = ui.find_hit(|t| matches!(t, Target::Button(Cmd::PushMenu)));
    let anchor = |u: &Ui| match u.app.overlays.last() {
        Some(Overlay::Menu(m)) => m.anchor,
        _ => panic!("no menu is open"),
    };

    // Clicking the button opens the menu right below it.
    ui.click(MouseButton::Left, push);
    assert_eq!(anchor(&ui), Some((push.x, push.y + 1)));
    ui.key(KeyCode::Esc);

    // The key opens it in the middle, wherever the mouse is.
    ui.key(KeyCode::Char('P'));
    assert_eq!(anchor(&ui), None);
    ui.key(KeyCode::Esc);

    // So does choosing it from the help menu with its key.
    ui.mouse(MouseEventKind::Moved, 3, 3);
    ui.key(KeyCode::Char('?'));
    assert_eq!(anchor(&ui), None);
    ui.key(KeyCode::Char('P'));
    assert_eq!(ui.app.overlays.len(), 1);
    assert_eq!(anchor(&ui), None);
    ui.key(KeyCode::Esc);

    // The context menu key is the exception. Its menu opens next to the
    // selected row, because that's what it acts on.
    ui.key(KeyCode::Char('.'));
    assert!(anchor(&ui).is_some());
}

#[test]
fn first_load_starts_at_the_top() {
    // An empty repository, so the detail pane has only a few rows.
    let s = Scratch::new();
    s.write("a.txt", "hi\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(0);
    ui.key(KeyCode::Char('3'));
    assert_eq!(ui.app.detail.cursor, 0);
    ui.key(KeyCode::Char('j'));
    assert_eq!(ui.app.detail.cursor, 1);
    ui.key(KeyCode::Char(' '));
    ui.wait("a.txt to be staged", |u| {
        u.screen().contains("Staged changes (1)")
    });
}

#[test]
fn buttons_under_a_dialog_do_not_light_up() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let stash = ui.find_hit(|t| matches!(t, Target::Button(Cmd::StashPush)));
    let bg = |u: &Ui| u.term.backend().buffer()[(stash.x, stash.y)].bg;
    let normal = bg(&ui);

    // Hovering the button normally changes its color.
    ui.mouse(MouseEventKind::Moved, stash.x, stash.y);
    assert_ne!(bg(&ui), normal);

    // With a dialog open, it stays as it was.
    ui.mouse(MouseEventKind::Moved, 0, 35);
    ui.key(KeyCode::Char('S'));
    ui.mouse(MouseEventKind::Moved, stash.x, stash.y);
    assert_eq!(bg(&ui), normal);
}

#[test]
fn right_click_selects_without_activating() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let hunks = |u: &Ui| {
        u.app
            .detail
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Hunk { .. }))
            .count()
    };
    assert_eq!(hunks(&ui), 1);

    // On a file header, the menu opens and the file stays unfolded.
    let file = ui.doc_row(|r| matches!(r, Row::File { .. }));
    ui.click(MouseButton::Right, file);
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Menu(_))));
    assert_eq!(hunks(&ui), 1);
    ui.key(KeyCode::Esc);

    // Buttons only react to the left button.
    let stash = ui.find_hit(|t| matches!(t, Target::Button(Cmd::StashPush)));
    ui.click(MouseButton::Right, stash);
    assert!(ui.app.overlays.is_empty());
    let stage = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Stage(Unit::File(..)))));
    ui.click(MouseButton::Right, stage);
    ui.wait("things to settle", |u| u.app.busy.is_none());
    assert_eq!(s.sh(&["diff", "--cached", "--name-only"]).trim(), "");

    // A right-click outside a dialog leaves it open, like a left-click.
    ui.key(KeyCode::Char('S'));
    ui.click(MouseButton::Right, Rect::new(0, 35, 1, 1));
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Input(_))));
}

#[test]
fn right_click_in_the_planner_opens_one_menu() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    // Plan from the oldest commit, so there are two rows.
    ui.app.select_history(2);
    ui.key(KeyCode::Char('2'));
    ui.key(KeyCode::Char('i'));
    ui.wait("the planner", |u| u.app.planner.is_some());
    let action = ui.find_hit(|t| matches!(t, Target::PlannerAction(1)));
    ui.click(MouseButton::Right, action);
    assert_eq!(ui.app.overlays.len(), 1);
    assert_eq!(ui.app.planner.as_ref().map(|p| p.selected), Some(1));
}

#[test]
fn folds_do_not_carry_over_to_other_commits() {
    let s = Scratch::new();
    s.write("big.txt", "start\n");
    s.commit_all("Start");
    let many: String = (0..500).map(|i| format!("line {i}\n")).collect();
    s.write("big.txt", &many);
    s.commit_all("Add a lot");
    s.write("big.txt", &many.replace("line 7\n", "line seven\n"));
    s.commit_all("Change one line");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    let hunks = |u: &Ui| {
        u.app
            .detail
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Hunk { .. }))
            .count()
    };

    // The big diff starts out folded.
    ui.select_commit(2);
    assert_eq!(hunks(&ui), 0);
    // The small diff of the same file in another commit doesn't.
    ui.select_commit(1);
    assert_eq!(hunks(&ui), 1);
}

#[test]
fn leaving_a_commit_while_it_loads_stops_loading() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    // The commit can't load before the working directory is selected
    // again, because events are only handled in `wait`.
    ui.app.select_history(1);
    assert!(ui.app.detail.loading);
    ui.app.select_history(0);
    ui.wait("the app to go idle", |u| u.app.idle());
}

#[test]
fn clicks_on_detail_rows_that_are_gone_do_nothing() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    let r = ui.doc_row(|r| matches!(r, Row::Line { .. }));
    // The other commit shows a single row until it loads. Without a
    // frame in between, the clicks still find the line from before.
    ui.app.select_history(2);
    assert_eq!(ui.app.detail.rows.len(), 1);
    for button in [MouseButton::Left, MouseButton::Right] {
        for kind in [MouseEventKind::Down(button), MouseEventKind::Up(button)] {
            let ev = MouseEvent {
                kind,
                column: r.x,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            };
            ui.handle(AppEvent::Input(Event::Mouse(ev)));
        }
        assert_eq!(ui.app.detail.cursor, 0);
        assert!(ui.app.overlays.is_empty());
    }
}

#[test]
fn draws_at_any_size() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let sizes: Vec<(u16, u16)> = [1, 2, 3, 4, 7, 12, 24, 39, 40, 70, 100, 140]
        .into_iter()
        .flat_map(|w| (1..=12).chain([24, 36]).map(move |h| (w, h)))
        .collect();
    let draw_all = |ui: &mut Ui, what: &str| {
        for &(w, h) in &sizes {
            ui.term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            let app = &mut ui.app;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ui.term.draw(|f| draw(app, f)).expect("draw");
            }));
            assert!(r.is_ok(), "drawing {what} at {w}x{h} panicked");
        }
        ui.term = Terminal::new(TestBackend::new(140, 36)).expect("terminal");
        ui.draw();
    };

    draw_all(&mut ui, "the working directory");
    ui.key(KeyCode::Char('t'));
    draw_all(&mut ui, "the side-by-side diff");
    ui.app.sidebar_visible = Some(true);
    ui.select_commit(1);
    draw_all(&mut ui, "a commit");

    ui.key(KeyCode::Char('?'));
    draw_all(&mut ui, "the help menu");
    ui.key(KeyCode::Esc);
    ui.key(KeyCode::Char('S'));
    draw_all(&mut ui, "an input box");
    ui.key(KeyCode::Esc);
    ui.run_cmd(Cmd::Palette);
    draw_all(&mut ui, "the palette");
    ui.key(KeyCode::Esc);
    ui.key(KeyCode::Char('@'));
    draw_all(&mut ui, "the command log");
    ui.key(KeyCode::Esc);
    ui.run_cmd(Cmd::Drop(ui.app.history.commits[0].id));
    draw_all(&mut ui, "a confirmation");
    ui.key(KeyCode::Esc);
    ui.app.show_error(
        "Something failed",
        &anyhow::anyhow!("a long explanation\nover two lines"),
    );
    draw_all(&mut ui, "an error");
    ui.key(KeyCode::Esc);

    ui.key(KeyCode::Char('2'));
    ui.key(KeyCode::Char('i'));
    ui.wait("the planner", |u| u.app.planner.is_some());
    draw_all(&mut ui, "the planner");
}

#[test]
fn commits_and_amends_from_the_message_box() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.key(KeyCode::Char('a'));
    ui.settle();
    ui.key(KeyCode::Char('c'));
    ui.type_text("#3 Use a digit");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    ui.settle();
    ui.wait("the new commit", |u| u.app.history.commits.len() == 3);
    assert_eq!(s.log()[0], "#3 Use a digit");
    assert!(ui.app.message.is_empty());
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());

    // Amend only the message.
    ui.key(KeyCode::Char('3'));
    ui.key(KeyCode::Char('A'));
    ui.wait("HEAD's message", |u| {
        u.app.message.text() == "#3 Use a digit"
    });
    ui.key(KeyCode::Char('c'));
    ui.type_text("!");
    ui.key_with(KeyCode::Char('s'), KeyModifiers::CONTROL);
    ui.settle();
    assert_eq!(
        s.log(),
        ["#3 Use a digit!", "Add a fourth note", "Add notes"]
    );
    assert!(!ui.app.amend);
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
}

#[test]
fn rewords_a_commit_and_runs_a_plan() {
    let s = repo_with_change();
    s.sh(&["stash", "-q"]);
    s.write("more.txt", "more\n");
    s.commit_all("Add more");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    // Reword the oldest commit.
    ui.app.select_history(3);
    ui.key(KeyCode::Char('r'));
    ui.wait("the reword dialog", |u| {
        matches!(u.app.overlays.last(), Some(Overlay::Input(_)))
    });
    ui.type_text(" file");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.log(), ["Add more", "Add a fourth note", "Add notes file"]);
    ui.wait("the rewritten history", |u| {
        u.app.history.commits.len() == 3 && u.app.history.commits[2].summary == "Add notes file"
    });

    // Plan: fix up the newest into the middle one, and reword the middle.
    ui.app.select_history(3);
    ui.key(KeyCode::Char('i'));
    ui.wait("the planner", |u| u.app.planner.is_some());
    assert_eq!(ui.app.planner.as_ref().map(|p| p.rows.len()), Some(3));
    ui.key(KeyCode::Char('f'));
    ui.key(KeyCode::Char('j'));
    ui.key(KeyCode::Char('r'));
    ui.wait("the message dialog", |u| {
        matches!(u.app.overlays.last(), Some(Overlay::Input(_)))
    });
    ui.type_text("!");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    ui.key(KeyCode::Enter);
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert!(ui.app.planner.is_none());
    assert_eq!(s.log(), ["Add a fourth note!", "Add notes file"]);
    assert!(s.file("more.txt").exists());
}

/// Open the planner on three commits. Its rows are newest first.
fn open_planner(s: &Scratch) -> Ui {
    s.sh(&["stash", "-q"]);
    s.write("more.txt", "more\n");
    s.commit_all("Add more");
    let mut ui = Ui::open(s);
    ui.wait_loaded(3);
    ui.app.select_history(3);
    ui.key(KeyCode::Char('i'));
    ui.wait("the planner", |u| u.app.planner.is_some());
    ui
}

#[test]
fn the_planner_asks_for_a_message_after_its_row_moves() {
    let s = repo_with_change();
    let mut ui = open_planner(&s);
    // Move the row down before git has answered with its message.
    ui.key(KeyCode::Char('r'));
    ui.key(KeyCode::Char('J'));
    ui.wait("the message dialog", |u| {
        matches!(u.app.overlays.last(), Some(Overlay::Input(_)))
    });
    ui.type_text("!");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    let row = &ui.app.planner.as_ref().expect("the planner").rows[1];
    assert_eq!(row.summary, "Add more");
    assert_eq!(row.action, PlanAction::Reword);
    assert!(
        row.message.as_deref().is_some_and(|m| m.contains('!')),
        "{row:?}"
    );
}

#[test]
fn a_reword_without_its_dialog_goes_back_to_pick() {
    let s = repo_with_change();
    let mut ui = open_planner(&s);
    // Another dialog opens before git has answered with the message.
    ui.key(KeyCode::Char('r'));
    ui.key(KeyCode::Char('@'));
    ui.wait("the row to go back to pick", |u| {
        u.app
            .planner
            .as_ref()
            .is_some_and(|p| p.rows[0].action == PlanAction::Pick)
    });
    assert!(matches!(ui.app.overlays.as_slice(), [Overlay::Log { .. }]));
}

#[test]
fn stages_selected_lines_and_discards_a_file() {
    let s = Scratch::new();
    s.write("f.txt", "1\n2\n3\n4\n5\n");
    s.commit_all("init");
    s.write("f.txt", "1\ntwo\nthree\n4\nfive\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    let line = |u: &Ui, text: &str| {
        let files = u.app.section_files(tergi::app::doc::Section::Unstaged);
        u.app
            .detail
            .rows
            .iter()
            .position(|r| matches!(r, Row::Line { file, hunk, line, .. } if files[*file].hunks[*hunk].lines[*line].text == text && files[*file].hunks[*hunk].lines[*line].new_no.is_some()))
            .expect("line")
    };
    // Select "+two" and "+three" with v and j, then stage them.
    let at = line(&ui, "two");
    ui.app.detail_move_to(at, false);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    ui.key(KeyCode::Char(' '));
    ui.settle();
    ui.wait("staged lines", |u| {
        u.screen().contains("Staged changes (1)")
    });
    assert_eq!(s.index("f.txt"), "1\n2\n3\ntwo\nthree\n4\n5\n");
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());

    // Side by side, the cursor row holds an old and a new line.
    ui.key(KeyCode::Char('t'));
    let at = ui.app.detail.rows.iter().position(|r| matches!(r, Row::Split { sec: tergi::app::doc::Section::Unstaged, left: Some(l), right: Some(r), .. } if l != r)).expect("a split row");
    ui.app.detail_move_to(at, false);
    ui.key(KeyCode::Char(' '));
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.index("f.txt"), "1\n2\n3\ntwo\nthree\n4\nfive\n");

    // Discard asks first.
    ui.key(KeyCode::Char('t'));
    ui.wait("unstaged rows", |u| {
        u.app.detail.rows.iter().any(|r| {
            matches!(
                r,
                Row::File {
                    sec: tergi::app::doc::Section::Unstaged,
                    ..
                }
            )
        })
    });
    let at = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| {
            matches!(
                r,
                Row::File {
                    sec: tergi::app::doc::Section::Unstaged,
                    ..
                }
            )
        })
        .expect("file row");
    ui.app.detail_move_to(at, false);
    ui.key(KeyCode::Char('d'));
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Confirm { .. })),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.sh(&["diff", "--name-only"]).trim(), "");
}

/// Put the cursor on a row in the detail pane and press `d`, which asks
/// before discarding.
fn ask_to_discard(ui: &mut Ui, row: impl Fn(&Ui, &Row) -> bool) {
    let at = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| row(ui, r))
        .expect("row in the detail pane");
    ui.app.detail_move_to(at, false);
    ui.key(KeyCode::Char('d'));
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Confirm { .. })),
        "{}",
        ui.screen()
    );
}

#[test]
fn a_file_waiting_to_be_discarded_stays_the_same_file() {
    let s = Scratch::new();
    for f in ["a.txt", "b.txt", "c.txt"] {
        s.write(f, "old\n");
    }
    s.commit_all("init");
    s.write("b.txt", "new\n");
    s.write("c.txt", "new\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    ask_to_discard(
        &mut ui,
        |u, r| matches!(r, Row::File { sec: Section::Unstaged, file } if u.app.section_files(Section::Unstaged)[*file].path() == "c.txt"),
    );

    // While the dialog is open, a change shows up ahead of c.txt.
    s.write("a.txt", "new\n");
    ui.run_cmd(Cmd::Refresh);
    ui.wait("a.txt to show up", |u| {
        u.app.section_files(Section::Unstaged).len() == 3
    });
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.sh(&["diff", "--name-only"]).trim(), "a.txt\nb.txt");
}

#[test]
fn discarding_a_section_spares_files_that_show_up_later() {
    let s = Scratch::new();
    s.write("base.txt", "base\n");
    s.commit_all("init");
    s.write("a.txt", "a\n");
    s.write("b.txt", "b\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    ask_to_discard(&mut ui, |_, r| *r == Row::Section(Section::Untracked));
    match ui.app.overlays.last() {
        Some(Overlay::Confirm { body, .. }) => {
            assert!(body.contains("a.txt and b.txt will be deleted"), "{body}");
        }
        _ => panic!("no dialog\n{}", ui.screen()),
    }

    // A new file shows up while the dialog is open.
    s.write("c.txt", "c\n");
    ui.run_cmd(Cmd::Refresh);
    ui.wait("c.txt to show up", |u| {
        u.app.section_files(Section::Untracked).len() == 3
    });
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.sh(&["status", "--porcelain"]).trim(), "?? c.txt");
}

#[test]
fn a_hunk_waiting_to_be_discarded_stays_the_same_hunk() {
    let s = Scratch::new();
    let text = |changes: &[(usize, &str)]| {
        let mut lines: Vec<String> = (1..=30).map(|i| i.to_string()).collect();
        for &(n, t) in changes {
            lines[n - 1] = t.to_string();
        }
        lines.join("\n") + "\n"
    };
    s.write("f.txt", &text(&[]));
    s.commit_all("init");
    s.write("f.txt", &text(&[(15, "fifteen"), (28, "twenty-eight")]));
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    ask_to_discard(&mut ui, |_, r| {
        matches!(
            r,
            Row::Hunk {
                sec: Section::Unstaged,
                hunk: 1,
                ..
            }
        )
    });

    // While the dialog is open, a new hunk shows up above it.
    s.write(
        "f.txt",
        &text(&[(2, "two"), (15, "fifteen"), (28, "twenty-eight")]),
    );
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the new hunk", |u| {
        u.app
            .section_files(Section::Unstaged)
            .first()
            .is_some_and(|f| f.hunks.len() == 3)
    });
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.read("f.txt"), text(&[(2, "two"), (15, "fifteen")]));
}

#[test]
fn resolves_a_conflict_and_continues_the_merge() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.commit_all("init");
    s.sh(&["checkout", "-q", "-b", "other"]);
    s.write("f.txt", "theirs\n");
    s.commit_all("theirs");
    s.sh(&["checkout", "-q", "main"]);
    s.write("f.txt", "ours\n");
    s.commit_all("ours");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    ui.run_cmd(Cmd::Merge("other".into()));
    ui.settle();
    // Git's message about the conflict is shown in a dialog.
    assert!(matches!(
        ui.app.overlays.last(),
        Some(Overlay::Message { .. })
    ));
    ui.key(KeyCode::Enter);
    ui.wait("the conflict", |u| {
        u.screen().contains("Merge conflicts (1)")
    });
    let screen = ui.screen();
    assert!(screen.contains("Merge in progress"), "{screen}");
    let theirs = ui.find_hit(|t| matches!(t, Target::Button(Cmd::TakeSide(_, false))));
    ui.click(MouseButton::Left, theirs);
    ui.settle();
    ui.wait("the resolution", |u| {
        u.screen().contains("Staged changes (1)")
    });
    assert_eq!(s.read("f.txt"), "theirs\n");
    ui.key_with(KeyCode::Char('c'), KeyModifiers::ALT);
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    ui.wait("the merge commit", |u| u.app.history.commits.len() == 4);
    let screen = ui.screen();
    assert!(!screen.contains("in progress"), "{screen}");
}

#[test]
#[should_panic(expected = "the operation blew up")]
fn a_panic_in_the_background_reaches_the_caller() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    async fn explode() -> anyhow::Result<String> {
        panic!("the operation blew up")
    }
    ui.app.spawn_op("Explode", After::Nothing, |_, _| explode());
    ui.wait("the operation", |u| u.app.busy.is_none());
}

/// Make `n` empty commits on main in one go. Running git for each one
/// would be slow.
fn many_commits(s: &Scratch, n: usize) {
    use std::io::Write as _;
    let mut stream = String::new();
    for i in 1..=n {
        let msg = format!("Commit {i}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{i}\ncommitter Test User <test@example.com> {} +0000\ndata {}\n{msg}",
            1_700_000_000 + i,
            msg.len()
        ));
        if i > 1 {
            stream.push_str(&format!("from :{}\n", i - 1));
        }
        stream.push('\n');
    }
    let mut child = std::process::Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(s.path())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("run git fast-import");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stream.as_bytes())
        .expect("write the stream");
    assert!(child.wait().expect("git fast-import").success());
}

#[test]
fn selection_follows_its_commit_through_a_reload() {
    let s = Scratch::new();
    many_commits(&s, 210);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(210);
    // The walk sends the first 200 commits on their own. Select one near
    // the end of them, then add commits on top, so the next walk sends it
    // in its second batch.
    ui.app.select_history(199);
    let id = ui
        .app
        .history
        .selected_commit()
        .map(|c| c.id)
        .expect("a selected commit");
    for i in 0..5 {
        s.commit_all(&format!("New {i}"));
    }
    ui.run_cmd(Cmd::Refresh);

    // Between the batches, the selection may be missing, but it must never
    // be some other commit.
    let deadline = Instant::now() + Duration::from_secs(15);
    while ui.app.history.loading || ui.app.history.commits.len() != 215 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the reload"
        );
        if let Some(ev) = ui.next_event(Duration::from_millis(10)) {
            ui.handle(ev);
        }
        if let Some(c) = ui.app.history.selected_commit() {
            assert_eq!(c.id, id, "the selection moved to {}", c.summary);
        }
    }
    assert_eq!(ui.app.history.selected_commit().map(|c| c.id), Some(id));
    assert_eq!(ui.app.history.index.get(&id), Some(&203));
}

#[test]
fn sidebar_selection_follows_its_branch_through_a_reload() {
    let s = repo_with_change();
    s.sh(&["branch", "b"]);
    s.sh(&["branch", "d"]);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let row_of =
        |u: &Ui, name: &str| {
            u.app.sidebar.rows.iter().position(
                |r| matches!(r, SideRow::Branch(i) if u.app.refs.branches[*i].name == name),
            )
        };
    let d = row_of(&ui, "d").expect("a row for d");
    ui.app.sidebar_select(d);

    // A new branch sorts in before d.
    s.sh(&["branch", "c"]);
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the new branch", |u| row_of(u, "c").is_some());
    assert_eq!(Some(ui.app.sidebar.selected), row_of(&ui, "d"));
}

/// "caf\xe9" and "caf\xff" both show up as "caf\u{fffd}", so the sidebar
/// has to keep the bytes to act on the right branch or remote.
#[test]
fn branches_and_remotes_whose_names_are_not_utf8() {
    let s = Scratch::new();
    if !s.allow_non_utf8_refs() {
        return;
    }
    write_change(&s);
    s.sh_bytes(&[b"branch", b"caf\xe9"]);
    s.sh_bytes(&[b"branch", b"caf\xff"]);
    for remote in [&b"r\xe9"[..], b"r\xff"] {
        s.sh_bytes(&[b"remote", b"add", remote, b"/nowhere"]);
        let r = [b"refs/remotes/", remote, b"/main"].concat();
        s.sh_bytes(&[b"update-ref", &r, b"HEAD"]);
    }
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    assert!(ui.screen().contains("caf\u{fffd}"), "{}", ui.screen());

    let row_of = |u: &Ui, f: &dyn Fn(&SideRow) -> bool| {
        u.app
            .sidebar
            .rows
            .iter()
            .position(f)
            .expect("a row in the sidebar")
    };
    let row = row_of(
        &ui,
        &|r| matches!(r, SideRow::Branch(i) if ui.app.refs.branches[*i].name == b"caf\xff".as_slice()),
    );
    ui.click(
        MouseButton::Left,
        ui.find_hit(|t| *t == Target::SidebarRow(row)),
    );
    ui.key(KeyCode::Char(' '));
    ui.settle();
    assert_eq!(
        s.sh_bytes(&[b"symbolic-ref", b"HEAD"]),
        b"refs/heads/caf\xff\n"
    );

    let remote_rows = |u: &Ui| {
        u.app
            .sidebar
            .rows
            .iter()
            .filter(|r| matches!(r, SideRow::RemoteBranch(_)))
            .count()
    };
    assert_eq!(remote_rows(&ui), 2);
    let row = row_of(&ui, &|r| *r == SideRow::Remote(b"r\xe9".into()));
    ui.click(
        MouseButton::Left,
        ui.find_hit(|t| *t == Target::SidebarRow(row)),
    );
    assert_eq!(remote_rows(&ui), 1);
}

#[test]
fn force_push_only_pushes_the_branch_it_asked_about() {
    let remote = tempfile::tempdir().expect("tempdir");
    let s = repo_with_change();
    s.sh(&["stash", "-q"]);
    s.sh(&["init", "-q", "--bare", &remote.path().to_string_lossy()]);
    s.sh(&["remote", "add", "origin", &remote.path().to_string_lossy()]);
    s.sh(&["branch", "other"]);
    s.sh(&["push", "-q", "-u", "origin", "main", "other"]);
    s.sh(&["commit", "-q", "--amend", "-m", "Amended on main"]);
    s.sh(&["checkout", "-q", "other"]);
    s.sh(&["commit", "-q", "--amend", "-m", "Amended on other"]);
    s.sh(&["checkout", "-q", "main"]);
    let on_remote = |b: &str| {
        s.sh(&[
            "--git-dir",
            &remote.path().to_string_lossy(),
            "rev-parse",
            b,
        ])
    };
    let (main_before, other_before) = (on_remote("main"), on_remote("other"));

    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.key(KeyCode::Char('P'));
    ui.key(KeyCode::Char('f'));
    match ui.app.overlays.last() {
        Some(Overlay::Confirm { body, .. }) => {
            assert!(body.contains("force pushes main"), "{body}");
        }
        _ => panic!("no dialog\n{}", ui.screen()),
    }

    // Another branch gets checked out while the dialog is open.
    s.sh(&["checkout", "-q", "other"]);
    ui.key(KeyCode::Char('y'));
    ui.settle();
    match ui.app.overlays.last() {
        Some(Overlay::Message {
            body, error: true, ..
        }) => assert!(body.contains("main isn't checked out"), "{body}"),
        _ => panic!("no error\n{}", ui.screen()),
    }
    assert_eq!(on_remote("main"), main_before);
    assert_eq!(on_remote("other"), other_before);
}

#[test]
fn the_theme_menu_previews_themes() {
    let s = Scratch::new();
    s.write("main.rs", "fn main() {\n    let x = \"hi\";\n}\n");
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    let mut ui = Ui::open_with_config(&s, Some(&config));
    ui.wait_loaded(0);
    // The color that `fn` in the diff is drawn in, if it's on screen.
    let drawn = |u: &Ui| {
        let buf = u.term.backend().buffer();
        let a = buf.area;
        (a.top()..a.bottom())
            .flat_map(|y| (a.left()..a.right().saturating_sub(7)).map(move |x| (x, y)))
            .find(|&(x, y)| {
                (0..7).all(|i| buf[(x + i, y)].symbol() == &"fn main"[i as usize..][..1])
            })
            .map(|p| buf[p].fg)
    };
    let keyword = |u: &Ui| u.app.theme.syntax.color(Kind::Keyword);
    ui.wait("syntax highlighting", |u| {
        drawn(u).is_some_and(|c| Some(c) == keyword(u))
    });
    let first = ui.app.theme.id;
    let first_color = keyword(&ui);
    let open = |ui: &mut Ui| {
        ui.palette("choose theme");
        assert!(
            matches!(ui.app.overlays.last(), Some(Overlay::Menu(m)) if m.title.as_deref() == Some("Theme"))
        );
    };

    // Moving through the menu switches themes, syntax colors included, and
    // Esc goes back to the theme from before. None of that is saved.
    open(&mut ui);
    ui.key(KeyCode::Down);
    let second = ui.app.theme.id;
    assert_ne!(second, first);
    assert_ne!(keyword(&ui), first_color);
    ui.wait("new syntax colors", |u| drawn(u) == keyword(u));
    ui.key(KeyCode::Esc);
    assert!(ui.app.overlays.is_empty());
    assert_eq!(ui.app.theme.id, first);
    ui.wait("the old syntax colors", |u| drawn(u) == first_color);
    assert!(!config.exists());

    // Enter keeps the theme and saves it.
    open(&mut ui);
    ui.key(KeyCode::Down);
    ui.key(KeyCode::Enter);
    assert!(ui.app.overlays.is_empty());
    assert_eq!(ui.app.theme.id, second);
    let saved = format!("theme = \"{second}\"\n");
    assert_eq!(std::fs::read_to_string(&config).expect("config"), saved);

    // Clicking outside the menu cancels it, like Esc.
    open(&mut ui);
    ui.key(KeyCode::Up);
    assert_eq!(ui.app.theme.id, first);
    ui.click(MouseButton::Left, Rect::new(0, 35, 1, 1));
    assert!(ui.app.overlays.is_empty());
    assert_eq!(ui.app.theme.id, second);
    assert_eq!(std::fs::read_to_string(&config).expect("config"), saved);
}

#[test]
fn settings_follow_the_config_file() {
    let s = repo_with_change();
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "tab_width = 4\n").expect("write config");
    let mut ui = Ui::open_with_config(&s, Some(&config));
    ui.wait_loaded(2);
    // Make the history wide enough to show dates.
    ui.app.history_frac = 0.7;
    let status = |u: &Ui| {
        u.app
            .status
            .as_ref()
            .map(|(m, ..)| m.clone())
            .unwrap_or_default()
    };

    std::fs::write(&config, "theme = \"nord\"\nrelative_dates = false\n").expect("write config");
    ui.wait("the new settings", |u| {
        u.app.theme.id == "nord" && !u.app.cfg.relative_dates
    });
    let c = &ui.app.history.commits[0];
    let day = tergi::util::format_day(c.time, c.offset);
    ui.wait("full dates in the history", |u| u.screen().contains(&day));

    // A change that breaks the file is ignored, and so is an unknown theme.
    for broken in [
        "theme = \"dracula\"\ntab_width = [\n",
        "theme = \"plaid\"\ntab_width = 8\n",
    ] {
        ui.app.status = None;
        std::fs::write(&config, broken).expect("write config");
        ui.wait("a warning", |u| {
            status(u).starts_with("Ignored the change to the config file")
        });
        assert_eq!(ui.app.theme.id, "nord");
        assert_eq!(ui.app.cfg.tab_width, 4);
    }

    // Fixing it applies the change.
    std::fs::write(&config, "theme = \"dracula\"\ntab_width = 8\n").expect("write config");
    ui.wait("the fixed settings", |u| {
        u.app.theme.id == "dracula" && u.app.cfg.tab_width == 8 && u.app.cfg.relative_dates
    });
}

#[test]
fn palette_commands_change_and_save_settings() {
    let s = Scratch::new();
    let lines = |name: &str, changed: usize| -> String {
        (1..=20)
            .map(|i| {
                if i == changed {
                    format!("{name} changed\n")
                } else {
                    format!("{name} {i:02}\n")
                }
            })
            .collect()
    };
    s.write("a.txt", &lines("a", 0));
    s.write("b.txt", &lines("b", 0));
    s.commit_all("Add files");
    s.write("a.txt", &lines("a", 10));
    s.commit_all("Change a");
    s.write("b.txt", &lines("b", 10));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("tergi").join("config.toml");
    let mut ui = Ui::open_with_config(&s, Some(&config));
    ui.wait_loaded(2);
    assert!(ui.screen().contains("b 07"), "{}", ui.screen());

    // Context lines reload the working directory's diff.
    ui.palette("context lines");
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Input(_))));
    ui.key(KeyCode::Backspace);
    ui.type_text("1");
    ui.key(KeyCode::Enter);
    // The hunk header shows the line before the hunk, so look below it.
    ui.wait("less context", |u| {
        !u.screen().contains("b 12") && u.screen().contains("b 11")
    });
    // Commits use the new number too.
    ui.select_commit(1);
    ui.wait("the commit's diff", |u| u.screen().contains("a changed"));
    assert!(
        ui.screen().contains("a 11") && !ui.screen().contains("a 12"),
        "{}",
        ui.screen()
    );

    // Numbers out of range are refused.
    ui.palette("tab width");
    ui.key(KeyCode::Backspace);
    ui.type_text("99");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.app.cfg.tab_width, 4);
    ui.palette("tab width");
    ui.key(KeyCode::Backspace);
    ui.type_text("2");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.app.cfg.tab_width, 2);

    ui.palette("full dates");
    assert!(!ui.app.cfg.relative_dates);

    // The folder didn't exist, so saving made it.
    let saved = std::fs::read_to_string(&config).expect("config");
    assert_eq!(
        saved,
        "context_lines = 1\ntab_width = 2\nrelative_dates = false\n"
    );

    // Saving also started watching the file.
    std::fs::write(&config, "relative_dates = true\n").expect("write config");
    ui.wait("relative dates again", |u| {
        u.app.cfg.relative_dates && u.app.cfg.context_lines == 3
    });
}
