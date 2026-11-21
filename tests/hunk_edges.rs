//! Moving the edges of hunks with the keys and the mouse, in the whole app.

use crate::common;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use common::Scratch;
use common::ui::Ui;
use tergi::app::doc::{Row, Section};
use tergi::app::{Drag, Level};
use tergi::git::diff::LineKind;

/// Lines 1 to 30, with lines 5 and 15 changed when `changed` is set. With
/// three lines of context, the changes make two hunks, lines 2 to 8 and 12
/// to 18, with lines 9 to 11 between them.
fn text(changed: bool) -> String {
    (1..=30)
        .map(|i| match i {
            5 | 15 if changed => format!("changed {i}\n"),
            _ => format!("{i}\n"),
        })
        .collect()
}

/// The first and last line on the new side of each hunk of the first file
/// in `sec`.
fn hunks(ui: &Ui, sec: Section) -> Vec<(u32, u32)> {
    ui.app.section_files(sec)[0]
        .hunks
        .iter()
        .map(|h| (h.new_start, h.new_start + h.new_len - 1))
        .collect()
}

fn put_cursor(ui: &mut Ui, f: impl Fn(&Row) -> bool) {
    let i = ui
        .app
        .detail
        .rows
        .iter()
        .position(f)
        .expect("row in the detail pane");
    ui.app.detail.cursor = i;
    ui.draw();
}

fn status(ui: &Ui) -> String {
    ui.app
        .status
        .as_ref()
        .map(|(msg, _, _)| msg.clone())
        .unwrap_or_default()
}

fn ctrl(ui: &mut Ui, c: char) {
    ui.key_with(KeyCode::Char(c), KeyModifiers::CONTROL);
}

#[test]
fn keys_move_the_edges_of_a_hunk_until_it_meets_the_next() {
    let s = Scratch::new();
    s.write("f.txt", &text(false));
    s.commit_all("init");
    s.write("f.txt", &text(true));
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    put_cursor(&mut ui, |r| {
        matches!(
            r,
            Row::Hunk {
                sec: Section::Unstaged,
                hunk: 0,
                ..
            }
        )
    });
    assert_eq!(hunks(&ui, Section::Unstaged), [(2, 8), (12, 18)]);

    // K shows the line above, until the hunk starts at the top of the file.
    ui.key(KeyCode::Char('K'));
    assert_eq!(hunks(&ui, Section::Unstaged), [(1, 8), (12, 18)]);
    ui.key(KeyCode::Char('K'));
    assert_eq!(hunks(&ui, Section::Unstaged), [(1, 8), (12, 18)]);
    assert_eq!(status(&ui), "There are no more lines above this hunk");

    // J hides the top line, until the hunk starts with its change.
    for _ in 0..5 {
        ui.key(KeyCode::Char('J'));
    }
    assert_eq!(hunks(&ui, Section::Unstaged), [(5, 8), (12, 18)]);
    let first = &ui.app.section_files(Section::Unstaged)[0].hunks[0].lines[0];
    assert_eq!(first.kind, LineKind::Del);

    // Ctrl+J shows the lines below, and the third one joins the hunks. The
    // cursor stays on the header of the hunk it was on.
    ctrl(&mut ui, 'j');
    ctrl(&mut ui, 'j');
    assert_eq!(hunks(&ui, Section::Unstaged), [(5, 10), (12, 18)]);
    ctrl(&mut ui, 'j');
    assert_eq!(hunks(&ui, Section::Unstaged), [(5, 18)]);
    assert!(matches!(
        ui.app.detail.rows[ui.app.detail.cursor],
        Row::Hunk { hunk: 0, .. }
    ));
    // Ctrl+K hides the bottom line.
    ctrl(&mut ui, 'k');
    assert_eq!(hunks(&ui, Section::Unstaged), [(5, 17)]);

    // The hunk stages as it is shown, with no context above its first
    // change.
    ui.dot(' ');
    ui.settle();
    ui.wait("the staged hunk", |u| {
        u.app
            .workdir
            .as_ref()
            .is_some_and(|w| w.unstaged.is_empty() && w.staged.len() == 1)
    });
    assert_eq!(s.sh(&["show", ":f.txt"]), text(true));
    // The file left the unstaged changes, so its choices are gone.
    ui.wait("the choices to go", |u| u.app.detail.shown_lines.is_empty());
}

#[test]
fn moved_edges_stay_after_a_reload_until_reset() {
    let s = Scratch::new();
    s.write("f.txt", &text(false));
    s.commit_all("init");
    s.write("f.txt", &text(true));
    let mut ui = Ui::open(&s);
    ui.wait_loaded(1);
    ui.key(KeyCode::Char('3'));
    put_cursor(&mut ui, |r| matches!(r, Row::Hunk { hunk: 1, .. }));
    ui.key(KeyCode::Char('K'));
    ctrl(&mut ui, 'j');
    assert_eq!(hunks(&ui, Section::Unstaged), [(2, 8), (11, 19)]);

    // Another file changes, and the working directory loads again.
    s.write("g.txt", "new\n");
    ui.run_cmd(tergi::app::cmd::Cmd::Refresh);
    ui.wait("the new file", |u| {
        u.app
            .workdir
            .as_ref()
            .is_some_and(|w| w.untracked.len() == 1)
    });
    assert_eq!(hunks(&ui, Section::Unstaged), [(2, 8), (11, 19)]);

    // Staging the second hunk leaves the first one in the unstaged changes,
    // and in the staged changes it shows up as git gives it.
    ui.dot(' ');
    ui.settle();
    ui.wait("the staged hunk", |u| {
        u.app
            .workdir
            .as_ref()
            .is_some_and(|w| w.staged.len() == 1 && w.unstaged.len() == 1)
    });
    assert_eq!(hunks(&ui, Section::Unstaged), [(2, 8)]);
    assert_eq!(hunks(&ui, Section::Staged), [(12, 18)]);

    // The menu resets the file.
    put_cursor(&mut ui, |r| {
        matches!(
            r,
            Row::Hunk {
                sec: Section::Unstaged,
                ..
            }
        )
    });
    ui.key(KeyCode::Char('K'));
    assert_eq!(hunks(&ui, Section::Unstaged), [(1, 8)]);
    ui.dot('=');
    assert_eq!(hunks(&ui, Section::Unstaged), [(2, 8)]);
    assert!(ui.app.detail.shown_lines.is_empty());
}

#[test]
fn dragging_the_edges_of_a_hunk() {
    let s = Scratch::new();
    s.write("f.txt", &text(false));
    s.commit_all("init");
    s.write("f.txt", &text(true));
    s.commit_all("change");
    let mut ui = Ui::open(&s);
    ui.wait_loaded(2);
    ui.select_commit(1);
    let header = |ui: &Ui| ui.doc_row(|r| matches!(r, Row::Hunk { hunk: 1, .. }));
    let lines = |ui: &Ui| hunks(ui, Section::Commit);
    assert_eq!(lines(&ui), [(2, 8), (12, 18)]);

    // The header follows the mouse up, and the hunk's lines stay where
    // they are on the screen.
    let r = header(&ui);
    let line_12 = ui.doc_row(|r| {
        matches!(
            r,
            Row::Line {
                hunk: 1,
                line: 0,
                ..
            }
        )
    });
    ui.mouse(MouseEventKind::Down(MouseButton::Left), r.x, r.y);
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), r.x, r.y - 1);
    assert_eq!(lines(&ui), [(2, 8), (11, 18)]);
    assert_eq!(header(&ui).y, r.y - 1);
    let line_11 = ui.doc_row(|r| {
        matches!(
            r,
            Row::Line {
                hunk: 1,
                line: 0,
                ..
            }
        )
    });
    assert_eq!(line_11.y, line_12.y - 1);

    // Back down hides the line again.
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), r.x, r.y);
    assert_eq!(lines(&ui), [(2, 8), (12, 18)]);

    // Three rows up joins the hunks, and the drag ends.
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), r.x, r.y - 3);
    assert_eq!(lines(&ui), [(2, 18)]);
    assert!(ui.app.drag.is_none());
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), r.x, r.y - 5);
    assert_eq!(lines(&ui), [(2, 18)]);
    ui.mouse(MouseEventKind::Up(MouseButton::Left), r.x, r.y - 5);

    // The bottom edge follows the mouse down and back up.
    let end = ui.doc_row(|r| matches!(r, Row::HunkEnd { .. }));
    ui.mouse(MouseEventKind::Down(MouseButton::Left), end.x, end.y);
    assert!(matches!(
        ui.app.drag,
        Some(Drag::HunkEdge { top: false, .. })
    ));
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), end.x, end.y + 2);
    assert_eq!(lines(&ui), [(2, 20)]);
    assert_eq!(
        ui.doc_row(|r| matches!(r, Row::HunkEnd { .. })).y,
        end.y + 2
    );
    ui.mouse(MouseEventKind::Drag(MouseButton::Left), end.x, end.y - 1);
    assert_eq!(lines(&ui), [(2, 17)]);
    ui.mouse(MouseEventKind::Up(MouseButton::Left), end.x, end.y - 1);
    assert!(ui.app.drag.is_none());
    assert!(ui.app.status.as_ref().is_none_or(|s| s.1 != Level::Error));

    // Another commit starts out as git shows it.
    ui.select_commit(2);
    ui.select_commit(1);
    assert_eq!(lines(&ui), [(2, 8), (12, 18)]);
}
