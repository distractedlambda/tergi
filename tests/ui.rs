//! Drive the whole app against scratch repositories, drawing into a test
//! backend and sending it the same mouse and key events a terminal would.

use crate::common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;

use bstr::BString;
use common::Scratch;
use common::ui::Ui;
use tergi::app::cmd::{Cmd, Context, Overlay, SideRow, Unit};
use tergi::app::doc::{Row, Section, Source};
use tergi::app::input::{SCROLL_LOCK, Target};
use tergi::app::planner::PlanAction;
use tergi::app::{After, AppEvent, DETAIL_GRACE, Focus, HISTORY_ROW_HEIGHT};
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
        "Working directory",
        "1 change",
        "Add a fourth note",
        "Add notes",
        "Unstaged changes (1)",
        "notes.txt",
    ] {
        assert!(screen.contains(text), "missing {text:?}\n{screen}");
    }
}

/// Right-click a row in the detail pane and choose the menu item with the
/// key `c`.
fn menu_on_row(ui: &mut Ui, row: impl Fn(&Ui, &Row) -> bool, c: char) {
    let at = ui.doc_row(|r| row(ui, r));
    ui.click(MouseButton::Right, at);
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Menu(_))),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Char(c));
}

#[test]
fn staging_from_the_menu_stages_the_file() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    menu_on_row(&mut ui, |_, r| matches!(r, Row::File { .. }), ' ');
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
    menu_on_row(
        &mut ui,
        |ui, r| matches!(r, Row::File { sec, file } if ui.app.section_files(*sec)[*file].path() == b"\xff.txt".as_slice()),
        ' ',
    );
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
    let help = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Help)));

    // A click outside a menu only closes the menu.
    ui.git('F');
    ui.settle();
    ui.key(KeyCode::Char('?'));
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Menu(_))));
    ui.click(MouseButton::Left, help);
    assert!(ui.app.overlays.is_empty());

    // A click outside a dialog does nothing at all.
    ui.git('S');
    assert!(matches!(ui.app.overlays.last(), Some(Overlay::Input(_))));
    ui.click(MouseButton::Left, help);
    assert_eq!(ui.app.overlays.len(), 1);
}

#[test]
fn menus_from_the_keyboard_open_in_the_middle() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let anchor = |u: &Ui| match u.app.overlays.last() {
        Some(Overlay::Menu(m)) => m.anchor,
        _ => panic!("no menu is open"),
    };

    // `,` opens the git menu in the middle, and so does its push key.
    ui.mouse(MouseEventKind::Moved, 3, 3);
    ui.key(KeyCode::Char(','));
    assert_eq!(anchor(&ui), None);
    ui.key(KeyCode::Char('P'));
    assert_eq!(ui.app.overlays.len(), 1);
    assert_eq!(anchor(&ui), None);
    ui.key(KeyCode::Esc);
    assert!(ui.app.overlays.is_empty());

    // Clicking the push item opens the push menu at the pointer.
    ui.key(KeyCode::Char(','));
    let push = ui.find_hit(|t| matches!(t, Target::MenuItem(i) if matches!(ui.app.overlays.last(), Some(Overlay::Menu(m)) if m.items[*i].cmd == Cmd::PushMenu)));
    ui.click(MouseButton::Left, push);
    assert_eq!(anchor(&ui), Some((push.x, push.y)));
    ui.key(KeyCode::Esc);

    // Keys in the help menu don't run anything, because the same key can
    // mean something else after `.` or `,`.
    ui.key(KeyCode::Char('?'));
    ui.key(KeyCode::Char('P'));
    assert_eq!(ui.app.overlays.len(), 1);
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Menu(m)) if m.help),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Esc);

    // The context menu key is the exception. Its menu opens next to the
    // selected row, because that's what it acts on.
    ui.key(KeyCode::Char('.'));
    assert!(anchor(&ui).is_some());
}

/// Ids in a SHA-256 repository are longer, but everything else works the
/// same.
#[test]
fn opens_a_sha256_repository() {
    let s = Scratch::sha256();
    write_change(&s);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    let commit = ui.app.detail.commit.clone().expect("commit");
    assert_eq!(commit.id.as_bytes().len(), 32);
    assert_eq!(commit.message, "Add a fourth note\n");
    let files: Vec<String> = commit.files.iter().map(|f| f.path().to_string()).collect();
    assert_eq!(files, ["notes.txt"]);
    assert_eq!(
        commit.texts[0].new.as_deref(),
        Some("one\ntwo\nthree\nfour\n")
    );
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
    ui.dot(' ');
    ui.wait("a.txt to be staged", |u| {
        u.screen().contains("Staged changes (1)")
    });
}

#[test]
fn buttons_under_a_dialog_do_not_light_up() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let hint = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Help)));
    let bg = |u: &Ui| u.term.backend().buffer()[(hint.x, hint.y)].bg;
    let normal = bg(&ui);

    // Hovering a hint normally changes its color.
    ui.mouse(MouseEventKind::Moved, hint.x, hint.y);
    assert_ne!(bg(&ui), normal);

    // With a dialog open, it stays as it was.
    ui.mouse(MouseEventKind::Moved, 0, 20);
    ui.git('S');
    ui.mouse(MouseEventKind::Moved, hint.x, hint.y);
    assert_eq!(bg(&ui), normal);
}

#[test]
fn clickable_rows_in_the_detail_pane_light_up() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.draw();
    let away = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Help)));
    let meta = |u: &Ui, click: bool| {
        u.doc_row(|r| matches!(r, Row::Meta(m) if u.app.detail.meta[*m].click.is_some() == click))
    };
    let lights_up = |u: &mut Ui, r: Rect| {
        let at = (r.right() - 2, r.y);
        u.mouse(MouseEventKind::Moved, away.x, away.y);
        let normal = u.term.backend().buffer()[at].bg;
        u.mouse(MouseEventKind::Moved, r.x + 1, r.y);
        u.term.backend().buffer()[at].bg != normal
    };

    let file = ui.doc_row(|r| matches!(r, Row::File { .. }));
    assert!(lights_up(&mut ui, file), "{}", ui.screen());
    // Hunk headers and bottom edges can be dragged.
    let hunk = ui.doc_row(|r| matches!(r, Row::Hunk { .. }));
    assert!(lights_up(&mut ui, hunk), "{}", ui.screen());
    let end = ui.doc_row(|r| matches!(r, Row::HunkEnd { .. }));
    let edge = |u: &Ui| u.term.backend().buffer()[(end.x + 1, end.y)].fg;
    ui.mouse(MouseEventKind::Moved, away.x, away.y);
    let normal = edge(&ui);
    ui.mouse(MouseEventKind::Moved, end.x + 1, end.y);
    assert_ne!(edge(&ui), normal, "{}", ui.screen());
    let parent = meta(&ui, true);
    assert!(lights_up(&mut ui, parent), "{}", ui.screen());
    // Clicking the subject does nothing, so it stays as it was.
    let subject = meta(&ui, false);
    assert!(!lights_up(&mut ui, subject), "{}", ui.screen());
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

    // Hints in the bottom bar only react to the left button.
    let help = ui.find_hit(|t| matches!(t, Target::Button(Cmd::Help)));
    ui.click(MouseButton::Right, help);
    assert!(ui.app.overlays.is_empty());

    // A right-click outside a dialog leaves it open, like a left-click.
    ui.git('S');
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
    ui.dot('i');
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

    // A long diff isn't folded until the user folds it.
    ui.select_commit(2);
    assert_eq!(hunks(&ui), 1);
    ui.key(KeyCode::Char('3'));
    ui.key(KeyCode::Char('Z'));
    assert_eq!(hunks(&ui), 0);
    // The diff of the same file in another commit isn't folded.
    ui.select_commit(1);
    assert_eq!(hunks(&ui), 1);
}

/// Fold or unfold a file in the detail pane, as Space does on its row.
fn toggle_fold(ui: &mut Ui, sec: Section, path: &str) {
    let app = &ui.app;
    let i = app
        .detail
        .rows
        .iter()
        .position(|r| matches!(r, Row::File { sec: s, file } if *s == sec && app.section_files(sec)[*file].path() == path))
        .expect("the file's row");
    ui.app.detail.cursor = i;
    ui.run_cmd(Cmd::ToggleFold);
}

fn folded(ui: &Ui, sec: Section, path: &str) -> bool {
    ui.app
        .detail
        .collapsed
        .contains(&(sec, BString::from(path)))
}

/// The folded files in the working directory, in order.
fn workdir_folds(ui: &Ui) -> Vec<(Section, BString)> {
    let mut folds: Vec<_> = ui
        .app
        .detail
        .collapsed
        .iter()
        .filter(|(sec, _)| *sec != Section::Commit)
        .cloned()
        .collect();
    folds.sort();
    folds
}

#[test]
fn folds_are_saved_for_each_commit_and_section() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.write("b.txt", "b\n");
    s.commit_all("Add a and b");
    s.write("a.txt", "a2\n");
    s.write("b.txt", "b2\n");
    s.commit_all("Change a and b");
    s.write("a.txt", "a3\n");
    s.write("c.txt", "c\n");
    s.sh(&["add", "c.txt"]);
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().join("state.redb");
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(2);

    // Each commit has its own folds.
    ui.select_commit(1);
    toggle_fold(&mut ui, Section::Commit, "a.txt");
    ui.select_commit(2);
    assert!(!folded(&ui, Section::Commit, "a.txt"));
    toggle_fold(&mut ui, Section::Commit, "b.txt");
    ui.select_commit(1);
    assert!(folded(&ui, Section::Commit, "a.txt"));
    assert!(!folded(&ui, Section::Commit, "b.txt"));

    ui.app.select_history(0);
    toggle_fold(&mut ui, Section::Unstaged, "a.txt");
    toggle_fold(&mut ui, Section::Staged, "c.txt");
    let want = |folds: &[(Section, &str)]| -> Vec<(Section, BString)> {
        folds
            .iter()
            .map(|(sec, p)| (*sec, BString::from(*p)))
            .collect()
    };

    // The next run reads them back.
    ui.finish_saving();
    drop(ui);
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(2);
    assert_eq!(
        workdir_folds(&ui),
        want(&[(Section::Unstaged, "a.txt"), (Section::Staged, "c.txt")])
    );
    ui.select_commit(1);
    assert!(folded(&ui, Section::Commit, "a.txt"));
    assert!(!folded(&ui, Section::Commit, "b.txt"));
    ui.select_commit(2);
    assert!(folded(&ui, Section::Commit, "b.txt"));
    assert!(!folded(&ui, Section::Commit, "a.txt"));

    // A file that is staged or unstaged as a whole takes its fold along.
    ui.app.select_history(0);
    ui.run_cmd(Cmd::Stage(Unit::File(Section::Unstaged, "a.txt".into())));
    ui.settle();
    let staged = want(&[(Section::Staged, "a.txt"), (Section::Staged, "c.txt")]);
    ui.wait("a.txt to be staged and folded", |u| {
        workdir_folds(u) == staged
    });
    ui.run_cmd(Cmd::Unstage(Unit::File(Section::Staged, "c.txt".into())));
    ui.settle();
    let untracked = want(&[(Section::Untracked, "c.txt"), (Section::Staged, "a.txt")]);
    ui.wait("c.txt to be untracked and folded", |u| {
        workdir_folds(u) == untracked
    });
    ui.run_cmd(Cmd::StageAll);
    ui.settle();
    ui.wait("c.txt to be staged and folded", |u| {
        workdir_folds(u) == staged
    });

    ui.finish_saving();
    drop(ui);
    let mut ui = Ui::open_with_store(&s, &store);
    // This is what the store has, before the working directory loads.
    assert_eq!(workdir_folds(&ui), staged);
    ui.wait_loaded(2);

    // Once a file's change is gone, so is its fold.
    s.sh(&["commit", "-m", "Commit a and c"]);
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the folds to go", |u| workdir_folds(u).is_empty());
    ui.finish_saving();
    drop(ui);
    let ui = Ui::open_with_store(&s, &store);
    assert_eq!(workdir_folds(&ui), []);
}

#[test]
fn a_large_commit_remembers_the_files_unfolded_in_it() {
    let s = Scratch::new();
    for i in 0..41 {
        s.write(&format!("f{i:02}.txt"), "x\n");
    }
    s.commit_all("Add many files");
    s.write("other.txt", "y\n");
    s.commit_all("Add another file");
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().join("state.redb");
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(2);
    let commit_folds = |u: &Ui| {
        u.app
            .detail
            .collapsed
            .iter()
            .filter(|(sec, _)| *sec == Section::Commit)
            .count()
    };

    // A commit with more than 40 files starts with all of them folded.
    ui.select_commit(2);
    assert_eq!(commit_folds(&ui), 41);
    toggle_fold(&mut ui, Section::Commit, "f03.txt");
    ui.select_commit(1);
    assert_eq!(commit_folds(&ui), 0);
    ui.select_commit(2);
    assert_eq!(commit_folds(&ui), 40);
    assert!(!folded(&ui, Section::Commit, "f03.txt"));

    // With everything unfolded, nothing is folded the next time either.
    ui.run_cmd(Cmd::FoldAll);
    assert_eq!(commit_folds(&ui), 41);
    ui.run_cmd(Cmd::FoldAll);
    assert_eq!(commit_folds(&ui), 0);
    ui.finish_saving();
    drop(ui);
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(2);
    ui.select_commit(2);
    assert_eq!(commit_folds(&ui), 0);
}

#[test]
fn long_message_lines_scroll_sideways() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    let long = format!("{}the end", "word ".repeat(40));
    s.commit_all(&format!("Subject\n\n{long}"));
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.select_commit(1);
    ui.draw();
    assert!(!ui.screen().contains("the end"), "{}", ui.screen());
    let r = ui.doc_row(|r| matches!(r, Row::Meta(_)));
    for _ in 0..200 {
        ui.mouse(MouseEventKind::ScrollRight, r.x, r.y);
    }
    assert!(ui.screen().contains("the end"), "{}", ui.screen());
}

#[test]
fn each_file_scrolls_sideways_on_its_own() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.write("b.txt", "b\n");
    s.commit_all("init");
    let long = |end: &str| format!("{}{end}\n", "word ".repeat(40));
    s.write("a.txt", &long("end of a"));
    s.write("b.txt", &long("end of b"));
    s.commit_all("long lines");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.draw();
    let r = ui.doc_row(|r| matches!(r, Row::Line { file: 0, .. }));
    for _ in 0..200 {
        ui.mouse(MouseEventKind::ScrollRight, r.x, r.y);
    }
    let screen = ui.screen();
    assert!(screen.contains("end of a"), "{screen}");
    assert!(!screen.contains("end of b"), "{screen}");
}

#[test]
fn sideways_scrolling_stops_at_the_end_of_the_longest_line() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.commit_all("init");
    s.write("a.txt", &format!("{}the end\nshort\n", "word ".repeat(40)));
    s.commit_all("long line");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.draw();
    let r = ui.doc_row(|r| matches!(r, Row::Line { file: 0, .. }));
    for _ in 0..400 {
        ui.mouse(MouseEventKind::ScrollRight, r.x, r.y);
    }
    // The end of the line is at the right edge, with the rest of the pane
    // still full of the line.
    let screen = ui.screen();
    assert!(
        screen.contains(&format!("{}the end", "word ".repeat(8))),
        "{screen}"
    );
    // Going back starts right away, without first undoing any scrolling
    // past the end.
    ui.mouse(MouseEventKind::ScrollLeft, r.x, r.y);
    let screen = ui.screen();
    assert!(!screen.contains("the end"), "{screen}");
    assert!(screen.contains("word the"), "{screen}");
}

#[test]
fn scrolling_one_way_blocks_the_other_for_a_moment() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.commit_all("init");
    s.write("a.txt", &format!("{}the end\n", "word ".repeat(40)));
    s.commit_all("long line");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.draw();
    let r = ui.doc_row(|r| matches!(r, Row::Line { file: 0, .. }));
    // A slow machine could pause long enough between the two steps for
    // the second one to count, so try until both come in time.
    loop {
        let start = Instant::now();
        ui.mouse(MouseEventKind::ScrollDown, r.x, r.y);
        let before = ui.screen();
        ui.mouse(MouseEventKind::ScrollRight, r.x, r.y);
        if start.elapsed() < SCROLL_LOCK {
            assert_eq!(ui.screen(), before);
            break;
        }
    }
    std::thread::sleep(SCROLL_LOCK);
    let before = ui.screen();
    ui.mouse(MouseEventKind::ScrollRight, r.x, r.y);
    assert_ne!(ui.screen(), before);
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
fn the_detail_pane_keeps_its_old_contents_while_a_commit_loads() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    let before = ui.term.backend().buffer().clone();
    let d = ui.app.areas.detail;
    let cells = |ui: &Ui| {
        let buf = ui.term.backend().buffer();
        d.positions().map(|p| buf[p].clone()).collect::<Vec<_>>()
    };
    let old: Vec<_> = d.positions().map(|p| before[p].clone()).collect();

    // Events are only handled in `wait`, so the other commit can't load
    // before these frames are drawn.
    ui.app.select_history(2);
    ui.draw();
    assert!(ui.app.detail.loading);
    assert_eq!(cells(&ui), old, "the pane changed during the grace period");

    ui.app.detail_wait = Instant::now().checked_sub(DETAIL_GRACE);
    ui.draw();
    let faded = cells(&ui);
    assert!(
        faded
            .iter()
            .zip(&old)
            .all(|(a, b)| a.symbol() == b.symbol())
    );
    assert!(
        faded
            .iter()
            .zip(&old)
            .any(|(a, b)| a.symbol().trim() != "" && a.fg != b.fg),
        "the old contents didn't fade"
    );

    ui.wait("the other commit to load", |u| {
        u.app.detail.commit.is_some()
    });
    assert_ne!(cells(&ui), old);
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
    ui.key(KeyCode::Char(','));
    draw_all(&mut ui, "the git menu");
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
    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    draw_all(&mut ui, "the message editor");
    ui.key(KeyCode::Esc);

    ui.key(KeyCode::Char('2'));
    ui.dot('i');
    ui.wait("the planner", |u| u.app.planner.is_some());
    draw_all(&mut ui, "the planner");
}

#[test]
fn commits_and_amends_from_the_message_box() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.git('a');
    ui.settle();
    ui.git('c');
    ui.type_text("#3 Use a digit");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    ui.settle();
    ui.wait("the new commit", |u| u.app.history.commits.len() == 3);
    assert_eq!(s.log()[0], "#3 Use a digit");
    assert!(ui.app.message.is_empty());
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());

    // Amend only the message.
    ui.key(KeyCode::Char('3'));
    ui.git('A');
    ui.wait("HEAD's message", |u| {
        u.app.message.text() == "#3 Use a digit"
    });
    ui.git('c');
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
    s.sh(&["reset", "-q", "--hard"]);
    s.write("more.txt", "more\n");
    s.commit_all("Add more");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    // Reword the oldest commit.
    ui.app.select_history(3);
    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    ui.type_text(" file");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    ui.settle();
    assert!(ui.app.edit.is_none());
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.log(), ["Add more", "Add a fourth note", "Add notes file"]);
    ui.wait("the rewritten history", |u| {
        u.app.history.commits.len() == 3 && u.app.history.commits[2].summary == "Add notes file"
    });

    // Plan: fix up the newest into the middle one, and reword the middle.
    ui.app.select_history(3);
    ui.dot('i');
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

/// The message editor takes the place of the commit message in the
/// detail pane, rather than opening a dialog.
#[test]
fn edits_the_message_where_the_commit_shows_it() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.commit_all("Add a");
    s.write("b.txt", "b\n");
    s.commit_all("Add b\n\nThe body of the message.");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    let shows = |ui: &Ui, text: &str| ui.screen().matches(text).count();
    assert_eq!(shows(&ui, "The body of the message."), 1, "{}", ui.screen());

    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    assert!(ui.app.overlays.is_empty());
    let editor = ui.find_hit(|t| *t == Target::Edit);
    let detail = ui.app.areas.detail;
    assert!(
        editor.y > detail.y && editor.x > detail.x && editor.right() <= detail.right(),
        "{editor:?} isn't in the detail pane {detail:?}"
    );
    assert!(ui.screen().contains("Reword commit"), "{}", ui.screen());
    // The editor holds the message, and the header leaves it out.
    assert_eq!(shows(&ui, "The body of the message."), 1, "{}", ui.screen());
    assert!(!ui.app.detail.rows.is_empty());
    assert!(ui.screen().contains("Author"), "{}", ui.screen());

    // The rest of the screen can't be clicked while the editor is open.
    let other = ui.find_hit(|t| *t == Target::HistoryRow(2));
    ui.click(MouseButton::Left, other);
    assert!(ui.app.edit.is_some());
    assert_eq!(ui.app.history.current_row(), 1);

    // Esc cancels, and the message comes back.
    ui.type_text("!");
    ui.key(KeyCode::Esc);
    ui.draw();
    assert!(ui.app.edit.is_none());
    assert_eq!(shows(&ui, "The body of the message."), 1, "{}", ui.screen());
    assert!(!ui.screen().contains("message.!"), "{}", ui.screen());
    assert_eq!(s.log(), ["Add b", "Add a"]);

    // The Save hint saves.
    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    ui.type_text("!");
    let save = ui.find_hit(|t| *t == Target::Button(Cmd::SubmitInput));
    ui.click(MouseButton::Left, save);
    ui.settle();
    assert!(ui.app.edit.is_none());
    assert_eq!(
        s.sh(&["log", "-1", "--format=%B"]).trim(),
        "Add b\n\nThe body of the message.!"
    );
}

#[test]
fn the_message_editor_closes_when_its_commit_goes_away() {
    let s = four_commits();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.select_commit(2);
    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    // Rewrite the branch behind the app's back.
    s.sh(&["reset", "-q", "--hard", "HEAD~2"]);
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the editor to close", |u| u.app.edit.is_none());
    ui.wait("the new history", |u| u.app.history.commits.len() == 2);
    assert!(!ui.app.detail.hide_message);
    let status = ui.app.status.as_ref().map(|s| s.0.clone());
    assert!(
        status
            .as_deref()
            .is_some_and(|s| s.contains("no longer selected")),
        "{status:?}"
    );
}

/// `main` at "one", with "two" and "three" ahead of it on `ahead`, and
/// "aside" on `aside`, which forks before "one".
fn branch_behind() -> Scratch {
    let s = Scratch::new();
    s.write("zero.txt", "zero\n");
    s.commit_all("zero");
    s.sh(&["checkout", "-q", "-b", "aside"]);
    s.write("aside.txt", "aside\n");
    s.commit_all("aside");
    s.sh(&["checkout", "-q", "main"]);
    s.write("one.txt", "one\n");
    s.commit_all("one");
    s.sh(&["checkout", "-q", "-b", "ahead"]);
    for name in ["two", "three"] {
        s.write(&format!("{name}.txt"), &format!("{name}\n"));
        s.commit_all(name);
    }
    s.sh(&["checkout", "-q", "main"]);
    s
}

/// Select the commit at `rev` in the history.
fn select_rev(ui: &mut Ui, s: &Scratch, rev: &str) {
    let id = s.sh(&["rev-parse", rev]);
    let i = ui
        .app
        .history
        .commits
        .iter()
        .position(|c| c.id.to_string() == id.trim())
        .expect("commit in the history");
    ui.select_commit(i + 1);
}

fn fast_forward_label(ui: &Ui) -> Option<String> {
    ui.app
        .actions(Context::History)
        .into_iter()
        .find(|a| matches!(a.cmd, Cmd::FastForward { .. }))
        .map(|a| a.label)
}

#[test]
fn fast_forwards_only_to_commits_ahead() {
    let s = branch_behind();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(5);
    for rev in ["main", "main~1", "aside"] {
        select_rev(&mut ui, &s, rev);
        assert_eq!(fast_forward_label(&ui), None, "{rev}");
    }
    select_rev(&mut ui, &s, "ahead~1");
    assert_eq!(
        fast_forward_label(&ui).as_deref(),
        Some("Fast-forward main to here")
    );

    // An uncommitted change to a file the commits don't touch stays.
    s.write("zero.txt", "changed\n");
    select_rev(&mut ui, &s, "ahead");
    ui.dot('F');
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.sh(&["rev-parse", "main"]), s.sh(&["rev-parse", "ahead"]));
    assert_eq!(s.sh(&["symbolic-ref", "--short", "HEAD"]).trim(), "main");
    assert_eq!(s.read("three.txt"), "three\n");
    assert_eq!(s.read("zero.txt"), "changed\n");

    // With a detached HEAD, there is no branch to move.
    s.sh(&["checkout", "-q", "--detach", "main~2"]);
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the detached HEAD", |u| u.app.refs.head_branch.is_none());
    select_rev(&mut ui, &s, "ahead");
    assert_eq!(fast_forward_label(&ui), None);
}

#[test]
fn fast_forward_fails_rather_than_lose_changes() {
    let s = branch_behind();
    s.write("one.txt", "mine\n");
    s.sh(&["checkout", "-q", "ahead"]);
    s.write("one.txt", "theirs\n");
    s.commit_all("change one");
    s.sh(&["checkout", "-q", "main"]);
    s.write("one.txt", "mine\n");
    let main_before = s.sh(&["rev-parse", "main"]);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(6);
    select_rev(&mut ui, &s, "ahead");
    ui.dot('F');
    ui.settle();
    match ui.app.overlays.last() {
        Some(Overlay::Message {
            body, error: true, ..
        }) => assert!(body.contains("one.txt"), "{body}"),
        _ => panic!("no error\n{}", ui.screen()),
    }
    assert_eq!(s.sh(&["rev-parse", "main"]), main_before);
    assert_eq!(s.read("one.txt"), "mine\n");
}

/// Commits "one" to "four", each adding a file.
fn four_commits() -> Scratch {
    let s = Scratch::new();
    for name in ["one", "two", "three", "four"] {
        s.write(&format!("{name}.txt"), &format!("{name}\n"));
        s.commit_all(name);
    }
    s
}

#[test]
fn squashes_commits_selected_with_v() {
    let s = four_commits();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.app.select_history(2);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    assert_eq!(ui.app.history.marked_rows(), Some((2, 3)));
    ui.dot('s');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    let text = ui.app.edit.as_ref().map(|e| e.input.text.text());
    assert_eq!(text.as_deref(), Some("two\n\nthree"));
    // Cancelling keeps the selection.
    ui.key(KeyCode::Esc);
    assert!(ui.app.edit.is_none());
    assert_eq!(ui.app.history.marked_rows(), Some((2, 3)));
    ui.dot('s');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    ui.type_text("!");
    ui.key_with(KeyCode::Enter, KeyModifiers::CONTROL);
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.log(), ["four", "two", "one"]);
    assert_eq!(
        s.sh(&["log", "-1", "--format=%B", "HEAD~1"]).trim(),
        "two\n\nthree!"
    );
    assert_eq!(ui.app.history.anchor, None);
}

#[test]
fn fixes_up_commits_selected_with_v() {
    let s = four_commits();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.app.select_history(2);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    ui.dot('f');
    ui.settle();
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(s.log(), ["four", "two", "one"]);
    assert_eq!(ui.app.history.anchor, None);
}

/// A branch "side" that changes `a.txt`, and a commit "main" next to it
/// that adds `b.txt`. Returns their ids.
fn two_branches(s: &Scratch) -> (tergi::git::ObjectId, tergi::git::ObjectId) {
    s.write("a.txt", "base\n");
    s.commit_all("base");
    s.sh(&["checkout", "-q", "-b", "side"]);
    s.write("a.txt", "side\n");
    let side = s.commit_all("side");
    s.sh(&["checkout", "-q", "main"]);
    s.write("b.txt", "main\n");
    let main = s.commit_all("main");
    let id = |h: &str| tergi::git::ObjectId::from_hex(h.trim().as_bytes()).expect("id");
    (id(&side), id(&main))
}

#[test]
fn compares_the_ends_of_a_selection() {
    let s = Scratch::new();
    let (side, main) = two_branches(&s);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    let row = |ui: &Ui, id| ui.app.history.index[&id] + 1;
    let (a, b) = (row(&ui, side), row(&ui, main));
    ui.select_commit(a);
    ui.key(KeyCode::Char('v'));
    // Selecting only one commit still shows that commit.
    assert_eq!(ui.app.detail.source, Source::Commit(side));
    ui.app.select_history(b);
    // The diff goes from the commit lower down to the one above it.
    let (from, to) = if a > b { (side, main) } else { (main, side) };
    assert_eq!(ui.app.detail.source, Source::Compare { from, to });
    ui.wait("the comparison to load", |u| u.app.detail.commit.is_some());
    let detail = ui.app.detail.commit.clone().expect("a comparison");
    assert_eq!(detail.id, to);
    assert_eq!(detail.base.as_ref().map(|b| b.id), Some(from));
    let files: Vec<String> = detail.files.iter().map(|f| f.path().to_string()).collect();
    assert_eq!(files, ["a.txt", "b.txt"]);
    let screen = ui.screen();
    assert!(screen.contains("Changes between two commits"), "{screen}");
    assert!(
        screen.contains(&format!("From       {}", &from.to_string()[..7])),
        "{screen}"
    );
    assert!(
        screen.contains(&format!("To         {}", &to.to_string()[..7])),
        "{screen}"
    );

    // Clicking a commit's line copies its hash.
    let i = (0..ui.app.detail.meta.len())
        .find(|&m| ui.app.detail.meta[m].line.to_string().starts_with("From"))
        .expect("a From line");
    let r = ui.doc_row(|r| *r == Row::Meta(i));
    ui.click(MouseButton::Left, r);
    assert_eq!(ui.app.clipboard, Some(from.to_string()));

    // Ending the selection shows the selected commit again.
    ui.app.focus = tergi::app::Focus::History;
    ui.key(KeyCode::Esc);
    assert_eq!(ui.app.detail.source, Source::Commit(main));
}

#[test]
fn comparison_ends_when_a_rewrite_drops_its_commits() {
    let s = four_commits();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.app.select_history(1);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    assert!(matches!(ui.app.detail.source, Source::Compare { .. }));
    s.sh(&["reset", "-q", "--hard", "HEAD~2"]);
    s.write("five.txt", "five\n");
    s.commit_all("five");
    ui.run_cmd(Cmd::Refresh);
    ui.wait("the new history", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 3
    });
    assert_eq!(ui.app.history.anchor, None);
    let id = ui
        .app
        .history
        .selected_commit()
        .map(|c| c.id)
        .expect("a commit");
    assert_eq!(ui.app.detail.source, Source::Commit(id));
}

#[test]
fn selects_commits_with_shift_and_the_mouse() {
    let s = four_commits();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(4);
    ui.app.select_history(1);
    // Shift with a letter is only the letter.
    ui.key_with(KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(ui.app.history.anchor, None);
    ui.key(KeyCode::Char('g'));
    ui.key(KeyCode::Char('j'));
    ui.key_with(KeyCode::Down, KeyModifiers::SHIFT);
    ui.key_with(KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(ui.app.history.marked_rows(), Some((1, 3)));
    ui.key(KeyCode::Esc);
    assert_eq!(ui.app.history.anchor, None);
    assert!(ui.app.overlays.is_empty(), "Esc only ends the selection");

    let row = |ui: &Ui, i: usize| ui.find_hit(|t| *t == Target::HistoryRow(i));
    let r = row(&ui, 2);
    ui.click(MouseButton::Left, r);
    let r = row(&ui, 4);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let ev = MouseEvent {
            kind,
            column: r.x,
            row: r.y,
            modifiers: KeyModifiers::SHIFT,
        };
        ui.handle(AppEvent::Input(Event::Mouse(ev)));
        ui.draw();
    }
    assert_eq!(ui.app.history.marked_rows(), Some((2, 4)));
    // A right click inside the selection keeps it, so its menu is about
    // all of them.
    let r = row(&ui, 3);
    ui.click(MouseButton::Right, r);
    assert_eq!(ui.app.history.marked_rows(), Some((2, 4)));
    assert!(ui.screen().contains("Drop 3 commits"), "{}", ui.screen());
    ui.key(KeyCode::Char('d'));
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.log(), ["four"]);

    // A plain click ends the selection.
    ui.wait_loaded(1);
    ui.app.select_history(1);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('k'));
    assert_eq!(ui.app.history.marked_rows(), Some((1, 1)));
    let r = row(&ui, 1);
    ui.click(MouseButton::Left, r);
    assert_eq!(ui.app.history.anchor, None);
}

/// Open the planner on three commits. Its rows are newest first.
fn open_planner(s: &Scratch) -> Ui {
    s.sh(&["reset", "-q", "--hard"]);
    s.write("more.txt", "more\n");
    s.commit_all("Add more");
    let mut ui = Ui::open(s);
    ui.wait_loaded(3);
    ui.app.select_history(3);
    ui.dot('i');
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
    ui.dot(' ');
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
    ui.dot(' ');
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
    ui.dot('d');
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Confirm { .. })),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.sh(&["diff", "--name-only"]).trim(), "");
}

/// `.S` stashes the file the cursor is in, from any row of it, and leaves
/// the other files alone.
#[test]
fn stashes_one_file_from_the_detail_pane() {
    let s = Scratch::new();
    s.write("a.txt", "a\n");
    s.write("b.txt", "b\n");
    s.commit_all("init");
    s.write("a.txt", "staged\n");
    s.sh(&["add", "a.txt"]);
    s.write("b.txt", "changed\n");
    s.write("new.txt", "new\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    let in_file = |u: &Ui, r: &Row, sec: Section, path: &str| matches!(r, Row::Hunk { sec: s, file, .. } if *s == sec && u.app.section_files(sec)[*file].path() == path);
    let at = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| in_file(&ui, r, Section::Unstaged, "b.txt"))
        .expect("the hunk of b.txt");
    ui.app.detail_move_to(at, false);
    ui.dot('S');
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Input(_))),
        "{}",
        ui.screen()
    );
    ui.type_text("just b");
    ui.key(KeyCode::Enter);
    ui.settle();
    assert_eq!(
        s.sh(&["stash", "list", "--format=%gs"]).trim(),
        "On main: just b"
    );
    assert_eq!(s.sh(&["stash", "show", "--name-only"]).trim(), "b.txt");
    assert_eq!(s.read("b.txt"), "b\n");
    assert_eq!(s.index("a.txt"), "staged\n");
    assert_eq!(s.read("new.txt"), "new\n");

    // An untracked file, with no message, is named after the file.
    ui.wait("the untracked file", |u| {
        u.app.section_files(Section::Untracked).len() == 1
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
                    sec: Section::Untracked,
                    ..
                }
            )
        })
        .expect("the untracked file");
    ui.app.detail_move_to(at, false);
    ui.dot('S');
    ui.key(KeyCode::Enter);
    ui.settle();
    assert!(!s.file("new.txt").exists());
    assert_eq!(
        s.sh(&["stash", "list", "--format=%gs"]).lines().next(),
        Some("On main: new.txt")
    );
    assert_eq!(s.index("a.txt"), "staged\n");
}

/// A stash shows up in the history on top of the commit it was made on,
/// without the commits that git keeps its staged changes and untracked
/// files in. Selecting it shows the stash and offers its actions.
#[test]
fn stashes_show_in_the_history() {
    let s = repo_with_change();
    s.write("new.txt", "new\n");
    s.sh(&["stash", "-q", "-u"]);
    let id = |rev: &str| {
        let hex = s.sh(&["rev-parse", rev]);
        tergi::git::ObjectId::from_hex(hex.trim().as_bytes()).expect("id")
    };
    let (stash, head) = (id("stash@{0}"), id("HEAD"));
    let mut ui = Ui::open(&s);
    ui.wait_loaded(3);
    let i = ui.app.history.index[&stash];
    assert_eq!(ui.app.history.commits[i].parents, [head]);
    assert!(ui.screen().contains("stash@{0}"), "{}", ui.screen());

    ui.app.select_history(i + 1);
    assert_eq!(ui.app.detail.source, Source::Stash(stash));
    ui.dot(' ');
    ui.settle();
    assert_eq!(s.read("notes.txt"), "one\n2\nthree\nfour\n");
    assert_eq!(s.read("new.txt"), "new\n");

    // Applying keeps the stash, and dropping it takes it out of the
    // history.
    ui.wait_loaded(3);
    ui.dot('d');
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Confirm { .. })),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Char('y'));
    ui.settle();
    assert_eq!(s.sh(&["stash", "list"]), "");
    ui.wait_loaded(2);
}

#[test]
fn hidden_stashes_leave_the_history_and_stay_hidden() {
    let s = repo_with_change();
    s.sh(&["stash", "-q"]);
    s.write("notes.txt", "one\ntwo\nthree\nfive\n");
    s.sh(&["stash", "-q"]);
    let id = |rev: &str| {
        let hex = s.sh(&["rev-parse", rev]);
        tergi::git::ObjectId::from_hex(hex.trim().as_bytes()).expect("id")
    };
    let (older, newer) = (id("stash@{1}"), id("stash@{0}"));
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().join("tergi").join("state.redb");
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(4);

    // Hide the older stash from the sidebar.
    let row = ui
        .app
        .sidebar
        .rows
        .iter()
        .position(|r| matches!(r, SideRow::Stash(i) if ui.app.refs.stashes[*i].id == older))
        .expect("the stash in the sidebar");
    ui.app.focus = Focus::Sidebar;
    ui.app.sidebar_select(row);
    ui.dot('H');
    ui.wait("the walk without the older stash", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 3
    });
    assert!(!ui.app.history.index.contains_key(&older));
    assert!(!ui.app.labels.contains_key(&older));
    assert!(ui.screen().contains("⊘"), "{}", ui.screen());

    // Hide the newer one from the history, where it is selected.
    ui.app.focus = Focus::History;
    let i = ui.app.history.index[&newer];
    ui.app.select_history(i + 1);
    ui.dot('H');
    ui.wait("the walk without stashes", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 2
    });
    assert!(!ui.screen().contains("stash@{"), "{}", ui.screen());

    // A new stash moves the others to new names, but they stay hidden,
    // also on the next run.
    ui.finish_saving();
    drop(ui);
    s.write("notes.txt", "six\n");
    s.sh(&["stash", "-q"]);
    let newest = id("stash@{0}");
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(3);
    assert!(ui.app.history.index.contains_key(&newest));
    assert_eq!(ui.app.hidden.stashes, HashSet::from([older, newer]));

    // Showing one brings it back with its new name.
    let row = ui
        .app
        .sidebar
        .rows
        .iter()
        .position(|r| matches!(r, SideRow::Stash(i) if ui.app.refs.stashes[*i].id == older))
        .expect("the stash in the sidebar");
    ui.app.focus = Focus::Sidebar;
    ui.app.sidebar_select(row);
    ui.dot('H');
    ui.wait("the walk with the older stash", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 4
    });
    let labels: Vec<_> = ui.app.labels[&older]
        .iter()
        .map(|l| l.name.clone())
        .collect();
    assert_eq!(labels, ["stash@{2}"]);

    ui.palette("show all hidden");
    ui.wait("the walk with every stash", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 5
    });
    assert!(ui.app.hidden.is_empty());
    ui.finish_saving();
    drop(ui);
    let ui = Ui::open_with_store(&s, &store);
    assert!(ui.app.hidden.is_empty());
}

/// Put the cursor on a row in the detail pane and press `.d`, which asks
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
    ui.dot('d');
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
    ui.key(KeyCode::Char('3'));
    let at = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| {
            matches!(
                r,
                Row::File {
                    sec: Section::Conflicted,
                    ..
                }
            )
        })
        .expect("the conflicted file");
    ui.app.detail_move_to(at, false);
    ui.dot('T');
    ui.settle();
    ui.wait("the resolution", |u| {
        u.screen().contains("Staged changes (1)")
    });
    assert_eq!(s.read("f.txt"), "theirs\n");
    // The banner's Continue button finishes the merge.
    let cont = ui.find_hit(|t| matches!(t, Target::Button(Cmd::ContinueOp)));
    ui.click(MouseButton::Left, cont);
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
fn moving_the_selection_keeps_it_on_screen() {
    let s = Scratch::new();
    many_commits(&s, 60);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(60);
    let visible = |u: &Ui| {
        let top = u.app.history.current_row() * usize::from(HISTORY_ROW_HEIGHT);
        let bottom = top + usize::from(HISTORY_ROW_HEIGHT);
        let scroll = u.app.history.scroll;
        top >= scroll && bottom <= scroll + usize::from(u.app.areas.history.height)
    };
    for key in [KeyCode::Char('G'), KeyCode::Char('g'), KeyCode::PageDown] {
        ui.key(key);
        assert!(
            visible(&ui),
            "row {} is off screen after {key:?}",
            ui.app.history.current_row()
        );
    }
    for _ in 0..30 {
        ui.key(KeyCode::Char('j'));
        assert!(
            visible(&ui),
            "row {} is off screen",
            ui.app.history.current_row()
        );
    }
}

#[test]
fn the_wheel_scrolls_the_history_one_row_at_a_time() {
    let s = Scratch::new();
    many_commits(&s, 60);
    let mut ui = Ui::open(&s);
    ui.wait_loaded(60);
    let a = ui.app.areas.history;
    ui.mouse(MouseEventKind::ScrollDown, a.x + 1, a.y + 1);
    assert_eq!(ui.app.history.scroll, 1);
    // Only the second row of the working directory entry is left, and the
    // newest commit starts right below it.
    let line = |u: &Ui, y: u16| {
        u.screen()
            .lines()
            .nth(usize::from(y))
            .unwrap_or("")
            .to_string()
    };
    assert!(line(&ui, a.y).contains("No changes"), "{}", ui.screen());
    assert!(line(&ui, a.y + 1).contains("Commit 60"), "{}", ui.screen());
    assert_eq!(ui.find_hit(|t| *t == Target::HistoryRow(0)).height, 1);
    ui.mouse(MouseEventKind::ScrollUp, a.x + 1, a.y + 1);
    assert_eq!(ui.app.history.scroll, 0);
    assert!(
        line(&ui, a.y).contains("Working directory"),
        "{}",
        ui.screen()
    );
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

#[test]
fn hidden_branches_leave_the_history_and_stay_hidden() {
    let s = Scratch::new();
    let (side, _) = two_branches(&s);
    s.sh(&["update-ref", "refs/remotes/origin/side", "side"]);
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().join("tergi").join("state.redb");
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(3);
    let row_of = |u: &Ui, f: &dyn Fn(&Ui, &SideRow) -> bool| {
        u.app
            .sidebar
            .rows
            .iter()
            .position(|r| f(u, r))
            .expect("a row in the sidebar")
    };
    let side_row = |u: &Ui, r: &SideRow| matches!(r, SideRow::Branch(i) if u.app.refs.branches[*i].name == "side");
    let remote_row = |u: &Ui, r: &SideRow| matches!(r, SideRow::RemoteBranch(i) if u.app.refs.remote_branches[*i].name == "side");
    let hide = |ui: &mut Ui, f: &dyn Fn(&Ui, &SideRow) -> bool| {
        let row = row_of(ui, f);
        ui.app.focus = Focus::Sidebar;
        ui.app.sidebar_select(row);
        ui.dot('H');
    };

    // The remote branch still brings the commit in.
    hide(&mut ui, &side_row);
    ui.wait("the walk", |u| !u.app.history.loading);
    assert_eq!(ui.app.history.commits.len(), 3);
    let labels: Vec<_> = ui.app.labels[&side]
        .iter()
        .map(|l| l.name.clone())
        .collect();
    assert_eq!(labels, ["origin/side"]);

    hide(&mut ui, &remote_row);
    ui.wait("the walk without side", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 2
    });
    assert!(!ui.app.history.index.contains_key(&side));
    assert!(!ui.app.labels.contains_key(&side));
    assert!(ui.screen().contains("⊘"), "{}", ui.screen());

    // The checked out branch can't be hidden.
    let main_row = row_of(
        &ui,
        &|u, r| matches!(r, SideRow::Branch(i) if u.app.refs.branches[*i].name == "main"),
    );
    ui.app.sidebar_select(main_row);
    let hide_main = ui
        .app
        .actions(Context::Sidebar)
        .into_iter()
        .find(|a| a.label == "Hide from history")
        .expect("the action");
    assert!(!hide_main.enabled);

    // The next run reads them back.
    ui.finish_saving();
    drop(ui);
    let mut ui = Ui::open_with_store(&s, &store);
    ui.wait_loaded(2);
    let want: HashSet<BString> = ["refs/heads/side", "refs/remotes/origin/side"]
        .into_iter()
        .map(BString::from)
        .collect();
    assert_eq!(ui.app.hidden.refs, want);

    // Showing one brings the commit back.
    hide(&mut ui, &side_row);
    ui.wait("the walk with side", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 3
    });
    hide(&mut ui, &side_row);
    ui.wait("the walk without side", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 2
    });

    ui.palette("show all hidden");
    ui.wait("the walk with everything", |u| {
        !u.app.history.loading && u.app.history.commits.len() == 3
    });
    assert!(ui.app.hidden.is_empty());
    ui.finish_saving();
    drop(ui);
    let ui = Ui::open_with_store(&s, &store);
    assert!(ui.app.hidden.is_empty());
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
    ui.dot(' ');
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
fn force_push_only_pushes_the_branch_the_menu_named() {
    let remote = tempfile::tempdir().expect("tempdir");
    let s = repo_with_change();
    s.sh(&["reset", "-q", "--hard"]);
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
    ui.git('P');
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Menu(m)) if m.title.as_deref() == Some("Push main")),
        "no menu\n{}",
        ui.screen()
    );

    // Another branch gets checked out while the menu is open.
    s.sh(&["checkout", "-q", "other"]);
    ui.key(KeyCode::Char('f'));
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
fn identifiers_can_get_colors_by_name() {
    let s = Scratch::new();
    s.write(
        "main.rs",
        "fn main() {\n    let count = 1;\n    let total = count + 1;\n}\n",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    let mut ui = Ui::open_with_config(&s, Some(&config));
    ui.wait_loaded(0);
    // The colors that each place `word` is drawn on screen starts with.
    let drawn = |u: &Ui, word: &str| -> Vec<Color> {
        let buf = u.term.backend().buffer();
        let a = buf.area;
        let n = word.len() as u16;
        (a.top()..a.bottom())
            .flat_map(|y| (a.left()..a.right().saturating_sub(n)).map(move |x| (x, y)))
            .filter(|&(x, y)| (0..n).all(|i| buf[(x + i, y)].symbol() == &word[i as usize..][..1]))
            .map(|p| buf[p].fg)
            .collect()
    };
    let colors = |u: &Ui, word: &str| {
        u.app
            .theme
            .syntax_colors(true)
            .color(Kind::Identifier, word)
            .expect("a color")
    };
    // Identifiers get colors by default.
    let rainbow = |u: &Ui| {
        let c = colors(u, "count");
        drawn(u, "count") == [c, c] && drawn(u, "total") == [colors(u, "total")]
    };
    ui.wait("identifier colors", rainbow);
    let fg = ui.app.theme.fg;
    assert_ne!(colors(&ui, "count"), fg);

    ui.palette("stop coloring identifiers");
    assert!(!ui.app.cfg.rainbow_identifiers);
    ui.wait("plain identifiers", |u| drawn(u, "count") == [fg, fg]);
    assert_eq!(
        std::fs::read_to_string(&config).expect("config"),
        "rainbow_identifiers = false\n"
    );

    ui.palette("color identifiers by name");
    assert!(ui.app.cfg.rainbow_identifiers);
    ui.wait("identifier colors again", rainbow);
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

/// Two files with two hunks each, changed in the newest commit, which is
/// selected with the detail pane focused.
fn two_files_two_hunks() -> (Scratch, Ui) {
    let s = Scratch::new();
    let text = |changes: &[usize]| {
        (1..=30)
            .map(|i| {
                if changes.contains(&i) {
                    format!("changed {i}\n")
                } else {
                    format!("{i}\n")
                }
            })
            .collect::<String>()
    };
    s.write("a.txt", &text(&[]));
    s.write("b.txt", &text(&[]));
    s.commit_all("init");
    s.write("a.txt", &text(&[2, 28]));
    s.write("b.txt", &text(&[2, 28]));
    s.commit_all("change both");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.wait("the commit's files", |u| {
        u.app
            .detail
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Hunk { .. }))
            .count()
            == 4
    });
    ui.key(KeyCode::Char('3'));
    (s, ui)
}

fn cursor_row(ui: &Ui) -> Row {
    ui.app.detail.rows[ui.app.detail.cursor].clone()
}

#[test]
fn the_detail_pane_moves_by_file_then_hunk_then_line() {
    let (_s, mut ui) = two_files_two_hunks();
    assert!(matches!(cursor_row(&ui), Row::Meta(0)));

    // At the top level, j stops on the commit details that do something
    // on Enter, then on the files.
    let mut files = Vec::new();
    for _ in 0..6 {
        ui.key(KeyCode::Char('j'));
        if let Row::File { file, .. } = cursor_row(&ui) {
            files.push(file);
        }
    }
    files.dedup();
    assert_eq!(files, [0, 1]);
    ui.key(KeyCode::Char('k'));
    assert_eq!(
        cursor_row(&ui),
        Row::File {
            sec: Section::Commit,
            file: 0
        }
    );

    // l goes into the file's hunks, and j crosses into the next file.
    ui.key(KeyCode::Char('l'));
    let hunk = |file, hunk| Row::Hunk {
        sec: Section::Commit,
        file,
        hunk,
    };
    assert_eq!(cursor_row(&ui), hunk(0, 0));
    ui.key(KeyCode::Char('j'));
    assert_eq!(cursor_row(&ui), hunk(0, 1));
    ui.key(KeyCode::Char('j'));
    assert_eq!(cursor_row(&ui), hunk(1, 0));

    // Enter goes into the lines, starting at the first change. j and k
    // stay inside the hunk.
    ui.key(KeyCode::Enter);
    let line = |ui: &Ui| match cursor_row(ui) {
        Row::Line {
            file, hunk, line, ..
        } => (file, hunk, line),
        r => panic!("the cursor is on {r:?}"),
    };
    let f = &ui.app.section_files(Section::Commit)[1];
    let (_, h, l) = line(&ui);
    assert_ne!(
        f.hunks[h].lines[l].kind,
        tergi::git::diff::LineKind::Context
    );
    for _ in 0..20 {
        ui.key(KeyCode::Char('j'));
        assert_eq!(line(&ui).1, 0);
    }
    ui.key(KeyCode::Char('g'));
    assert_eq!(line(&ui), (1, 0, 0));
    for _ in 0..20 {
        ui.key(KeyCode::Char('k'));
        assert_eq!(line(&ui).1, 0);
    }

    // h backs out one level at a time, and then to the history.
    ui.key(KeyCode::Char('h'));
    assert_eq!(cursor_row(&ui), hunk(1, 0));
    ui.key(KeyCode::Char('h'));
    assert_eq!(
        cursor_row(&ui),
        Row::File {
            sec: Section::Commit,
            file: 1
        }
    );
    ui.key(KeyCode::Char('h'));
    assert_eq!(ui.app.focus, tergi::app::Focus::History);
}

#[test]
fn moving_to_a_file_shows_the_whole_file() {
    let s = Scratch::new();
    let short = |n: usize| format!("one\ntwo {n}\nthree\n");
    let long = |n: usize| {
        (0..80)
            .map(|i| format!("line {i} {n}\n"))
            .collect::<String>()
    };
    for i in 0..8 {
        s.write(&format!("f{i}.txt"), &short(0));
    }
    s.write("long.txt", &long(0));
    s.write("z.txt", &short(0));
    s.commit_all("init");
    for i in 0..8 {
        s.write(&format!("f{i}.txt"), &short(1));
    }
    s.write("long.txt", &long(1));
    s.write("z.txt", &short(1));
    s.commit_all("change everything");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.wait("the commit's files", |u| {
        u.app
            .detail
            .rows
            .iter()
            .filter(|r| matches!(r, Row::File { .. }))
            .count()
            == 10
    });
    ui.key(KeyCode::Char('3'));

    // With the cursor on a file's header, the whole file is on screen, or
    // the header is at the top when the file doesn't fit. Returns whether
    // it fits.
    let check = |ui: &Ui| -> Option<bool> {
        let d = &ui.app.detail;
        let c = d.cursor;
        let Row::File { sec, file } = d.rows[c] else {
            return None;
        };
        let last = (c..d.rows.len())
            .take_while(|&i| d.rows[i].file() == Some((sec, file)))
            .last()
            .expect("the header");
        let h = usize::from(ui.app.areas.doc.height);
        let fits = last - c < h;
        if fits {
            assert!(
                d.scroll <= c && last < d.scroll + h,
                "rows {c}..={last} aren't all shown from {}\n{}",
                d.scroll,
                ui.screen()
            );
        } else {
            assert_eq!(d.scroll, c, "the header isn't at the top\n{}", ui.screen());
        }
        Some(fits)
    };

    let mut seen = Vec::new();
    for _ in 0..30 {
        ui.key(KeyCode::Char('j'));
        seen.extend(check(&ui));
    }
    for _ in 0..30 {
        ui.key(KeyCode::Char('k'));
        seen.extend(check(&ui));
    }
    for _ in 0..12 {
        ui.key(KeyCode::Char(']'));
        seen.extend(check(&ui));
    }
    for _ in 0..12 {
        ui.key(KeyCode::Char('['));
        seen.extend(check(&ui));
    }
    // Back out from the last line of the long file to its header.
    for c in ['G', 'k', 'l', 'l', 'G', 'h', 'h'] {
        ui.key(KeyCode::Char(c));
    }
    assert!(matches!(cursor_row(&ui), Row::File { .. }));
    seen.extend(check(&ui));
    assert!(seen.contains(&true) && seen.contains(&false));
}

#[test]
fn esc_backs_out_one_level_at_a_time() {
    let s = Scratch::new();
    s.write("f.txt", "1\n2\n3\n");
    s.commit_all("init");
    s.write("f.txt", "1\ntwo\nthree\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    assert_eq!(cursor_row(&ui), Row::Section(Section::Unstaged));
    ui.key(KeyCode::Char('j'));
    ui.key(KeyCode::Char('l'));
    ui.key(KeyCode::Char('l'));
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    assert!(ui.app.detail.selection().is_some());

    // The first Esc only ends the selection.
    ui.key(KeyCode::Esc);
    assert!(ui.app.detail.selection().is_none());
    assert!(matches!(cursor_row(&ui), Row::Line { .. }));
    ui.key(KeyCode::Esc);
    assert!(matches!(cursor_row(&ui), Row::Hunk { .. }));
    ui.key(KeyCode::Esc);
    assert!(matches!(cursor_row(&ui), Row::File { .. }));
    assert!(ui.app.overlays.is_empty());
    // At the file level there is nothing left to back out of, so Esc does
    // nothing more.
    ui.key(KeyCode::Esc);
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert!(matches!(cursor_row(&ui), Row::File { .. }));
}

#[test]
fn comma_opens_the_git_menu_everywhere() {
    let s = Scratch::new();
    s.write("f.txt", "1\n2\n3\n");
    s.commit_all("one");
    s.write("f.txt", "1\n2\nthree\n");
    s.commit_all("two");
    s.write("f.txt", "1\ntwo\nthree\n");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let git_menu = |u: &Ui| matches!(u.app.overlays.last(), Some(Overlay::Menu(m)) if m.title.as_deref() == Some("Git"));
    let open_and_close = |ui: &mut Ui, what: &str| {
        ui.key(KeyCode::Char(','));
        assert!(git_menu(ui), "{what}\n{}", ui.screen());
        ui.key(KeyCode::Esc);
        assert!(ui.app.overlays.is_empty(), "{what}\n{}", ui.screen());
    };

    for pane in ['1', '2', '3'] {
        ui.key(KeyCode::Char(pane));
        open_and_close(&mut ui, &format!("pane {pane}"));
    }

    // Deep in the detail pane, with lines selected. The selection stays.
    ui.key(KeyCode::Char('j'));
    ui.key(KeyCode::Char('l'));
    ui.key(KeyCode::Char('l'));
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    assert!(ui.app.detail.selection().is_some());
    open_and_close(&mut ui, "a line selection");
    assert!(ui.app.detail.selection().is_some());
    ui.key(KeyCode::Esc);
    ui.key(KeyCode::Esc);
    open_and_close(&mut ui, "the hunk level");
    assert!(matches!(cursor_row(&ui), Row::Hunk { .. }));

    // In the history, with commits selected. They stay selected.
    ui.key(KeyCode::Char('2'));
    ui.app.select_history(1);
    ui.key(KeyCode::Char('v'));
    ui.key(KeyCode::Char('j'));
    assert_eq!(ui.app.history.marked_rows(), Some((1, 2)));
    open_and_close(&mut ui, "a commit selection");
    assert_eq!(ui.app.history.marked_rows(), Some((1, 2)));
    ui.key(KeyCode::Esc);
    assert_eq!(ui.app.history.anchor, None);

    // In the planner.
    ui.app.select_history(2);
    ui.dot('i');
    ui.wait("the planner", |u| u.app.planner.is_some());
    open_and_close(&mut ui, "the planner");
    assert!(ui.app.planner.is_some());

    // In the message box, `,` is text.
    ui.key(KeyCode::Esc);
    assert!(ui.app.planner.is_none());
    ui.git('c');
    assert_eq!(ui.app.focus, Focus::Message);
    ui.type_text("a, b");
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    assert_eq!(ui.app.message.text(), "a, b");
}

#[test]
fn moving_by_hunks_opens_folded_files() {
    let (_s, mut ui) = two_files_two_hunks();
    ui.key(KeyCode::Char('Z'));
    assert_eq!(ui.app.detail.collapsed.len(), 2);
    let first = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| matches!(r, Row::File { file: 0, .. }))
        .expect("the first file");
    ui.app.detail_move_to(first, false);

    // l opens the folded file it goes into.
    ui.key(KeyCode::Char('l'));
    assert!(matches!(
        cursor_row(&ui),
        Row::Hunk {
            file: 0,
            hunk: 0,
            ..
        }
    ));
    ui.key(KeyCode::Char('j'));
    // So does j when it moves into the next file.
    ui.key(KeyCode::Char('j'));
    assert!(matches!(
        cursor_row(&ui),
        Row::Hunk {
            file: 1,
            hunk: 0,
            ..
        }
    ));
    assert!(ui.app.detail.collapsed.is_empty());

    // Space folds the file the cursor is in, from any level, and leaves
    // the cursor on its header.
    ui.key(KeyCode::Char('l'));
    ui.key(KeyCode::Char(' '));
    assert_eq!(
        cursor_row(&ui),
        Row::File {
            sec: Section::Commit,
            file: 1
        }
    );
    assert_eq!(ui.app.detail.collapsed.len(), 1);

    // Going back over the folded first file opens it too, at its last hunk.
    ui.app.detail.collapsed.clear();
    ui.key(KeyCode::Char('Z'));
    ui.app.detail.collapsed.retain(|(_, p)| p == "a.txt");
    ui.app.rebuild_doc();
    let hunk = ui
        .app
        .detail
        .rows
        .iter()
        .position(|r| {
            matches!(
                r,
                Row::Hunk {
                    file: 1,
                    hunk: 0,
                    ..
                }
            )
        })
        .expect("a hunk of the second file");
    ui.app.detail_move_to(hunk, false);
    ui.key(KeyCode::Char('k'));
    assert!(matches!(
        cursor_row(&ui),
        Row::Hunk {
            file: 0,
            hunk: 1,
            ..
        }
    ));
}

#[test]
fn actions_on_the_selection_need_a_dot_first() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    ui.key(KeyCode::Char('2'));

    // On its own, d doesn't drop the commit.
    ui.key(KeyCode::Char('d'));
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    ui.dot('d');
    assert!(
        matches!(ui.app.overlays.last(), Some(Overlay::Confirm { .. })),
        "{}",
        ui.screen()
    );
    ui.key(KeyCode::Esc);

    // Ctrl+J is an item in the menu, so it doesn't move the menu's selection.
    ui.key(KeyCode::Char('.'));
    ui.key_with(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert!(ui.app.overlays.is_empty(), "{}", ui.screen());
    ui.wait("the commit to move", |u| u.app.busy.is_none());
}

#[test]
fn the_focused_pane_has_a_lit_border() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    // The color of the top left corner of each pane's border.
    let corners = |u: &Ui| {
        let buf = u.term.backend().buffer();
        let a = &u.app.areas;
        [a.sidebar_frame, a.history_frame, a.detail_frame].map(|r| {
            assert_eq!(buf[(r.x, r.y)].symbol(), "╭", "{}", u.screen());
            buf[(r.x, r.y)].fg
        })
    };
    let (lit, unlit) = (ui.app.theme.accent, ui.app.theme.border);
    for (focus, expected) in [
        (Focus::Sidebar, [lit, unlit, unlit]),
        (Focus::History, [unlit, lit, unlit]),
        (Focus::Detail, [unlit, unlit, lit]),
        (Focus::Message, [unlit, unlit, lit]),
    ] {
        ui.app.focus = focus;
        ui.draw();
        assert_eq!(corners(&ui), expected, "{focus:?}");
    }

    // The message editor takes the keys, so its pane is the lit one.
    ui.app.focus = Focus::History;
    ui.select_commit(1);
    ui.dot('r');
    ui.wait("the message editor", |u| u.app.edit.is_some());
    assert_eq!(corners(&ui), [unlit, unlit, lit]);
}

#[test]
fn dragging_a_border_resizes_the_panes() {
    let s = repo_with_change();
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    let handle = ui.find_hit(|t| *t == Target::Splitter(tergi::app::Splitter::Sidebar));
    assert_eq!(handle.width, 2);
    assert_eq!(handle.x, ui.app.areas.sidebar_frame.right() - 1);
    let start = handle.x;
    ui.mouse(MouseEventKind::Down(MouseButton::Left), start, 5);
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), start + 6, 5);
    ui.mouse(MouseEventKind::Up(MouseButton::Left), start + 6, 5);
    // The sidebar's right border follows the pointer.
    assert_eq!(ui.app.areas.sidebar_frame.right() - 1, start + 6);
    assert_eq!(ui.app.areas.history_frame.x, start + 7);

    let handle = ui.find_hit(|t| *t == Target::Splitter(tergi::app::Splitter::History));
    let start = handle.x;
    ui.mouse(MouseEventKind::Down(MouseButton::Left), start + 1, 5);
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), start - 10, 5);
    ui.mouse(MouseEventKind::Up(MouseButton::Left), start - 10, 5);
    let moved = ui.app.areas.history_frame.right() as i32 - (start as i32 - 10);
    assert!(moved.abs() <= 1, "{}", ui.screen());
}
