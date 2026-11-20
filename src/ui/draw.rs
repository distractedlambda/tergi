//! Drawing the whole screen.
//!
//! Drawing happens in two phases. First we work out the layout and store it
//! in the app, since mouse handling needs it. Then everything is drawn from
//! a shared reference, recording clickable areas in a hit list as we go.

use gix::bstr::ByteSlice as _;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::app::cmd::{ActionDef, Cmd, Overlay, SideKey, SideRow, Unit};
use crate::app::doc::{Row, Section, Source};
use crate::app::input::{Target, palette_filter};
use crate::app::planner::PlanAction;
use crate::app::{App, Drag, Focus, HISTORY_ROW_HEIGHT, Level, Splitter};
use crate::git::diff::{FileStatus, LineKind};
use crate::git::graph::DOWN;
use crate::git::rebase::RepoState;
use crate::git::refs::LabelKind;
use crate::highlight::LineSpans;
use crate::theme::{Syntax, Theme};
use crate::ui::text::{self, put, put_line, truncate, width};
use crate::util::{format_day, short_date};

type Hits = Vec<(Rect, Target)>;

const SPINNER: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    app.size = (area.width, area.height);
    layout(app, area);

    let mut hits: Hits = Vec::new();
    let mut cursor: Option<Position> = None;
    {
        let buf = f.buffer_mut();
        buf.set_style(area, Style::default().bg(app.theme.bg).fg(app.theme.fg));

        // The text boxes need mutable access, so draw them first.
        if app.detail.source == Source::Workdir
            && app.planner.is_none()
            && app.areas.message.height > 0
        {
            let theme = app.theme.clone();
            let r = app.areas.message;
            let bg = if app.focus == Focus::Message {
                theme.panel
            } else {
                theme.bg
            };
            let pos = app.message.render(
                buf,
                r,
                Style::default().bg(bg).fg(theme.fg),
                Style::default().fg(theme.faint).bg(bg),
            );
            if app.focus == Focus::Message && app.overlays.is_empty() {
                cursor = Some(pos);
            }
        }

        let a: &App = app;
        draw_topbar(a, buf, Rect::new(area.x, area.y, area.width, 1), &mut hits);
        if a.repo_state != RepoState::Clean && area.height > 4 {
            draw_banner(
                a,
                buf,
                Rect::new(area.x, area.y + 1, area.width, 1),
                &mut hits,
            );
        }
        if a.areas.sidebar.width > 0 {
            draw_sidebar(a, buf, &mut hits);
            splitter(
                a,
                buf,
                &mut hits,
                Splitter::Sidebar,
                a.areas.sidebar.right(),
                a.areas.body,
            );
        }
        if a.planner.is_some() {
            draw_planner(a, buf, &mut hits);
        } else {
            draw_history(a, buf, &mut hits);
            splitter(
                a,
                buf,
                &mut hits,
                Splitter::History,
                a.areas.history.right(),
                a.areas.body,
            );
            draw_detail(a, buf, &mut hits);
        }
        draw_bottom(
            a,
            buf,
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            &mut hits,
        );
    }
    let buf = f.buffer_mut();
    if let Some(pos) = draw_overlays(app, buf, area, &mut hits) {
        cursor = Some(pos);
    }
    app.hits = hits;
    if let Some(p) = cursor {
        f.set_cursor_position(p);
    }
    request_visible_highlights(app);
}

fn layout(app: &mut App, area: Rect) {
    let banner = u16::from(app.repo_state != RepoState::Clean && area.height > 4);
    let top = 1 + banner;
    let body = Rect::new(
        area.x,
        area.y + top,
        area.width,
        area.height.saturating_sub(top + 1),
    );
    app.areas.body = body;
    let mut x = body.x;
    let show_sidebar = app.sidebar_shown() && body.width >= 40;
    if show_sidebar {
        let w = app.sidebar_width.min(body.width / 2);
        app.areas.sidebar = Rect::new(x, body.y, w, body.height);
        x += w + 1;
    } else {
        app.areas.sidebar = Rect::default();
    }
    let rest = body.right().saturating_sub(x);
    let hw = ((f32::from(rest) * app.history_frac) as u16)
        .clamp(rest.min(20), rest.saturating_sub(21).max(rest.min(20)));
    app.areas.history = Rect::new(x, body.y, hw, body.height);
    app.areas.planner = Rect::new(x, body.y, rest, body.height);
    let dx = x + hw + 1;
    app.areas.detail = Rect::new(dx, body.y, body.right().saturating_sub(dx), body.height);

    let d = app.areas.detail;
    if app.detail.source == Source::Workdir && d.height > 8 {
        let lines = app.message.line_count().clamp(2, 8) as u16;
        app.areas.message = Rect::new(d.x + 1, d.y + 1, d.width.saturating_sub(2), lines);
        let used = lines + 2;
        app.areas.doc = Rect::new(d.x, d.y + used, d.width, d.height - used);
    } else {
        app.areas.message = Rect::default();
        app.areas.doc = d;
    }

    // Keep scroll offsets in range after resizes and reloads.
    let clamp = |scroll: &mut usize, len: usize, h: u16| {
        let max = len.saturating_sub(usize::from(h).max(1));
        if *scroll > max {
            *scroll = max;
        }
    };
    let (hlen, slen) = (app.history.row_count(), app.sidebar.rows.len());
    let hpage = app.history_page() as u16;
    clamp(&mut app.history.scroll, hlen, hpage);
    clamp(&mut app.sidebar.scroll, slen, app.areas.sidebar.height);
    let doc_len = app.detail.rows.len() + usize::from(app.areas.doc.height / 2);
    let doc_h = app.areas.doc.height;
    clamp(&mut app.detail.scroll, doc_len, doc_h);
    if let Some(p) = &mut app.planner {
        let h = app.areas.planner.height.saturating_sub(4);
        let sel = p.selected;
        if sel < p.scroll {
            p.scroll = sel;
        } else if sel >= p.scroll + usize::from(h.max(1)) {
            p.scroll = sel + 1 - usize::from(h.max(1));
        }
    }
}

fn fill(buf: &mut Buffer, r: Rect, bg: Color) {
    buf.set_style(r, Style::default().bg(bg));
}

/// Whether the mouse is over `r` in the panes. While an overlay is open
/// nothing under it reacts to the mouse, so nothing there shows hover.
fn hovered(app: &App, r: Rect) -> bool {
    if !app.overlays.is_empty() {
        return false;
    }
    let (x, y) = app.mouse;
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}

/// Draw a clickable button and return its width.
#[allow(clippy::too_many_arguments)]
fn button(
    app: &App,
    buf: &mut Buffer,
    hits: &mut Hits,
    x: u16,
    y: u16,
    label: &str,
    cmd: Cmd,
    accent: Option<Color>,
) -> u16 {
    let w = width(label, 4) as u16 + 2;
    let r = Rect::new(x, y, w, 1);
    let t = &app.theme;
    let bg = match (hovered(app, r), accent) {
        (true, _) => t.button_hover,
        (false, Some(c)) => c,
        (false, None) => t.button,
    };
    let fg = if accent.is_some() { t.badge_fg } else { t.fg };
    put(
        buf,
        x,
        y,
        &format!(" {label} "),
        w,
        Style::default().bg(bg).fg(fg),
    );
    hits.push((r, Target::Button(cmd)));
    w
}

/// Draw buttons right-aligned so the last one ends at `right`. Buttons are
/// dropped from the end until the group fits right of `left`, so the first
/// one should be the most important. Returns the x where the group starts.
#[allow(clippy::too_many_arguments)]
fn buttons_right(
    app: &App,
    buf: &mut Buffer,
    hits: &mut Hits,
    left: u16,
    right: u16,
    y: u16,
    mut items: Vec<(&str, Cmd, Option<Color>)>,
) -> u16 {
    let total = |items: &[(&str, Cmd, Option<Color>)]| -> u16 {
        items.iter().map(|(l, _, _)| width(l, 4) as u16 + 3).sum()
    };
    while !items.is_empty() && right.saturating_sub(total(&items)) < left {
        items.pop();
    }
    let mut x = right.saturating_sub(total(&items));
    let start = x;
    for (label, cmd, accent) in items {
        x += button(app, buf, hits, x, y, label, cmd, accent) + 1;
    }
    start
}

fn splitter(app: &App, buf: &mut Buffer, hits: &mut Hits, which: Splitter, x: u16, body: Rect) {
    let r = Rect::new(x, body.y, 1, body.height);
    let active = matches!(&app.drag, Some(Drag::Splitter(s)) if *s == which) || hovered(app, r);
    let color = if active {
        app.theme.accent
    } else {
        app.theme.border
    };
    for y in r.top()..r.bottom() {
        if let Some(c) = buf.cell_mut((x, y)) {
            c.set_char('│').set_fg(color).set_bg(app.theme.bg);
        }
    }
    hits.push((r, Target::Splitter(which)));
}

fn draw_topbar(app: &App, buf: &mut Buffer, r: Rect, hits: &mut Hits) {
    let t = &app.theme;
    fill(buf, r, t.bar);
    let base = Style::default().bg(t.bar).fg(t.fg);
    let mut x = put(
        buf,
        r.x + 1,
        r.y,
        "tergi",
        6,
        base.fg(t.accent).add_modifier(Modifier::BOLD),
    );
    let name = app
        .repo
        .workdir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    x = put(
        buf,
        x + 2,
        r.y,
        &name,
        30,
        base.add_modifier(Modifier::BOLD),
    );
    let branch = match (&app.refs.head_branch, app.refs.head) {
        (Some(b), _) => format!("⎇ {b}"),
        (None, Some(h)) => format!("detached at {}", h.to_hex_with_len(7)),
        (None, None) => app
            .workdir
            .as_ref()
            .and_then(|w| w.status.branch.clone())
            .map_or_else(
                || "no commits yet".into(),
                |b| format!("⎇ {b} (no commits yet)"),
            ),
    };
    x = put(buf, x + 2, r.y, &branch, 40, base.fg(t.ok));
    if let Some(b) = app.refs.current_branch() {
        if b.upstream_gone {
            x = put(buf, x + 1, r.y, "upstream gone", 14, base.fg(t.warn));
        } else if let Some(up) = &b.upstream {
            x = put(buf, x + 1, r.y, &format!("→ {up}"), 40, base.fg(t.dim));
            let mut counts = Vec::new();
            if b.ahead > 0 {
                counts.push(format!("↑{}", b.ahead));
            }
            if b.behind > 0 {
                counts.push(format!("↓{}", b.behind));
            }
            if !counts.is_empty() {
                x = put(buf, x + 1, r.y, &counts.join(" "), 14, base.fg(t.warn));
            }
        }
    }

    let buttons = vec![
        ("⟳ Fetch", Cmd::Fetch, None),
        ("↓ Pull", Cmd::PullMenu, None),
        ("↑ Push", Cmd::PushMenu, None),
        ("Stash", Cmd::StashPush, None),
        ("⌘ Commands", Cmd::Palette, None),
    ];
    let start = buttons_right(
        app,
        buf,
        hits,
        x + 2,
        r.right().saturating_sub(1),
        r.y,
        buttons,
    );

    // Activity goes in the middle.
    let spin = SPINNER[(app
        .busy
        .as_ref()
        .map_or_else(std::time::Instant::now, |b| b.since)
        .elapsed()
        .as_millis()
        / 120) as usize
        % SPINNER.len()];
    let activity = if let Some(b) = &app.busy {
        Some(if b.progress.is_empty() {
            format!("{spin} {}…", b.label)
        } else {
            format!("{spin} {}: {}", b.label, b.progress)
        })
    } else if app.history.loading {
        let s = SPINNER[(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
            / 120) as usize
            % SPINNER.len()];
        Some(format!(
            "{s} Loading history… {} commits",
            app.history.commits.len()
        ))
    } else {
        None
    };
    if let Some(act) = activity {
        let avail = start.saturating_sub(x + 4);
        put(
            buf,
            x + 3,
            r.y,
            &truncate(&act, usize::from(avail)),
            avail,
            base.fg(t.accent),
        );
    }
}

fn draw_banner(app: &App, buf: &mut Buffer, r: Rect, hits: &mut Hits) {
    let t = &app.theme;
    let bg = t.badge_tag;
    fill(buf, r, bg);
    let style = Style::default().bg(bg).fg(t.badge_fg);
    let conflicts = app
        .workdir
        .as_ref()
        .map_or(0, |w| w.status.conflicted().count());
    let mut text = match &app.repo_state {
        RepoState::Rebase {
            head_name,
            step,
            total,
            stopped,
        } => {
            let mut s = "Rebase in progress".to_string();
            if let Some(h) = head_name {
                s.push_str(&format!(" on {h}"));
            }
            if let (Some(a), Some(b)) = (step, total) {
                s.push_str(&format!(" (step {a} of {b})"));
            }
            if let Some(st) = stopped {
                s.push_str(&format!(", stopped at {}", &st[..st.len().min(7)]));
            }
            s
        }
        RepoState::Merge => "Merge in progress".into(),
        RepoState::CherryPick => "Cherry-pick in progress".into(),
        RepoState::Revert => "Revert in progress".into(),
        RepoState::Clean => String::new(),
    };
    if conflicts > 0 {
        text.push_str(&format!(
            ". {conflicts} file{} with conflicts.",
            if conflicts == 1 { "" } else { "s" }
        ));
    }
    let mut items = vec![("Continue", Cmd::ContinueOp, Some(t.badge_local))];
    if !matches!(app.repo_state, RepoState::Merge) {
        items.push(("Skip", Cmd::SkipOp, None));
    }
    items.push(("Abort", Cmd::AbortOp, Some(t.badge_danger)));
    let start = buttons_right(
        app,
        buf,
        hits,
        r.x + 20,
        r.right().saturating_sub(1),
        r.y,
        items,
    );
    put(
        buf,
        r.x + 1,
        r.y,
        &text,
        start.saturating_sub(r.x + 2),
        style.add_modifier(Modifier::BOLD),
    );
}

fn row_bg(app: &App, selected: bool, focused: bool, r: Rect, base: Color) -> Color {
    if selected {
        if focused {
            app.theme.sel
        } else {
            app.theme.sel_unfocused
        }
    } else if hovered(app, r) && app.drag.is_none() {
        app.theme.hover
    } else {
        base
    }
}

fn draw_sidebar(app: &App, buf: &mut Buffer, hits: &mut Hits) {
    let a = app.areas.sidebar;
    let t = &app.theme;
    fill(buf, a, t.panel);
    let focused = app.focus == Focus::Sidebar && app.planner.is_none();
    let refs = &app.refs;
    for (i, row) in app
        .sidebar
        .rows
        .iter()
        .enumerate()
        .skip(app.sidebar.scroll)
        .take(usize::from(a.height))
    {
        let y = a.y + (i - app.sidebar.scroll) as u16;
        let rr = Rect::new(a.x, y, a.width, 1);
        let bg = row_bg(app, i == app.sidebar.selected, focused, rr, t.panel);
        fill(buf, rr, bg);
        let st = Style::default().bg(bg).fg(t.fg);
        let w = a.width.saturating_sub(1);
        match row {
            SideRow::Header { key, title, count } => {
                let arrow = if app
                    .sidebar
                    .collapsed
                    .contains(&SideKey::Header(key.clone()))
                {
                    '▸'
                } else {
                    '▾'
                };
                let x = put(
                    buf,
                    a.x + 1,
                    y,
                    &format!("{arrow} {}", title.to_uppercase()),
                    w,
                    st.fg(t.dim).add_modifier(Modifier::BOLD),
                );
                put(
                    buf,
                    x + 1,
                    y,
                    &count.to_string(),
                    w.saturating_sub(x - a.x),
                    st.fg(t.faint),
                );
            }
            SideRow::Branch(b) => {
                let br = &refs.branches[*b];
                let (mark, style) = if br.is_head {
                    ("●", st.fg(t.accent).add_modifier(Modifier::BOLD))
                } else {
                    (" ", st)
                };
                let mut track = String::new();
                if br.upstream_gone {
                    track.push_str(" gone");
                } else {
                    if br.ahead > 0 {
                        track.push_str(&format!(" ↑{}", br.ahead));
                    }
                    if br.behind > 0 {
                        track.push_str(&format!(" ↓{}", br.behind));
                    }
                }
                let tw = width(&track, 4) as u16;
                put(buf, a.x + 2, y, mark, 1, style);
                put(
                    buf,
                    a.x + 4,
                    y,
                    &truncate(
                        &br.name.to_str_lossy(),
                        usize::from(w.saturating_sub(4 + tw)),
                    ),
                    w.saturating_sub(4 + tw),
                    style,
                );
                put(
                    buf,
                    a.right().saturating_sub(tw + 1),
                    y,
                    &track,
                    tw,
                    st.fg(t.warn),
                );
            }
            SideRow::Remote(name) => {
                let arrow = if app
                    .sidebar
                    .collapsed
                    .contains(&SideKey::Remote(name.clone()))
                {
                    '▸'
                } else {
                    '▾'
                };
                put(
                    buf,
                    a.x + 2,
                    y,
                    &format!("{arrow} {name}"),
                    w.saturating_sub(2),
                    st.fg(t.dim),
                );
            }
            SideRow::RemoteBranch(b) => {
                let rb = &refs.remote_branches[*b];
                put(
                    buf,
                    a.x + 6,
                    y,
                    &truncate(&rb.name.to_str_lossy(), usize::from(w.saturating_sub(6))),
                    w.saturating_sub(6),
                    st,
                );
            }
            SideRow::Tag(i) => {
                put(buf, a.x + 2, y, "◇", 1, st.fg(t.tag_fg));
                put(
                    buf,
                    a.x + 4,
                    y,
                    &truncate(
                        &refs.tags[*i].name.to_str_lossy(),
                        usize::from(w.saturating_sub(4)),
                    ),
                    w.saturating_sub(4),
                    st,
                );
            }
            SideRow::Stash(i) => {
                let s = &refs.stashes[*i];
                put(buf, a.x + 2, y, "≡", 1, st.fg(t.dim));
                put(
                    buf,
                    a.x + 4,
                    y,
                    &truncate(&s.message, usize::from(w.saturating_sub(4))),
                    w.saturating_sub(4),
                    st,
                );
            }
            SideRow::Empty(msg) => {
                put(
                    buf,
                    a.x + 4,
                    y,
                    msg,
                    w.saturating_sub(4),
                    st.fg(t.faint).add_modifier(Modifier::ITALIC),
                );
            }
        }
        hits.push((rr, Target::SidebarRow(i)));
    }
}

fn draw_history(app: &App, buf: &mut Buffer, hits: &mut Hits) {
    let a = app.areas.history;
    let t = &app.theme;
    if a.width == 0 {
        return;
    }
    let focused = app.focus == Focus::History;
    let h = &app.history;
    let start = h.scroll;
    // An entry that only half fits at the bottom is still drawn.
    let end = (start + usize::from(a.height.div_ceil(HISTORY_ROW_HEIGHT))).min(h.row_count());
    let now = crate::util::now();

    // Fit the graph to the widest visible row, within limits.
    let graph_w = (start..end)
        .filter(|&i| i > 0)
        .filter_map(|i| h.rows.get(i - 1))
        .map(|r| r.cells.len())
        .max()
        .unwrap_or(1)
        .min(usize::from(a.width / 3)) as u16;
    let selected = h.selected_row();

    for i in start..end {
        let y = a.y + (i - start) as u16 * HISTORY_ROW_HEIGHT;
        let y2 = y + 1;
        let rr = Rect::new(a.x, y, a.width, HISTORY_ROW_HEIGHT.min(a.bottom() - y));
        let bg = row_bg(app, selected == Some(i), focused, rr, t.bg);
        fill(buf, rr, bg);
        hits.push((rr, Target::HistoryRow(i)));
        let st = Style::default().bg(bg).fg(t.fg);
        let x = a.x + 1;
        let text_x = x + graph_w + 1;
        let text_w = a.right().saturating_sub(text_x + 1);
        let second_row = y2 < a.bottom();
        if i == 0 {
            let n = app.workdir.as_ref().map_or(0, |w| w.change_count());
            put(
                buf,
                x,
                y,
                "◌",
                1,
                st.fg(if n > 0 { t.warn } else { t.faint }),
            );
            put(
                buf,
                text_x,
                y,
                "Working directory",
                text_w,
                st.fg(if n > 0 { t.fg } else { t.dim })
                    .add_modifier(Modifier::ITALIC),
            );
            if second_row && app.workdir.is_some() {
                let changes = match n {
                    0 => "No changes".to_string(),
                    1 => "1 change".to_string(),
                    n => format!("{n} changes"),
                };
                put(buf, text_x, y2, &changes, text_w, st.fg(t.dim));
            }
            continue;
        }
        let Some(c) = h.commits.get(i - 1) else {
            continue;
        };
        if let Some(row) = h.rows.get(i - 1) {
            for (k, cell) in row.cells.iter().take(usize::from(graph_w)).enumerate() {
                let cx = x + k as u16;
                let mut ch = cell.glyph();
                if ch == '●' && Some(c.id) == app.refs.head {
                    ch = '◉';
                }
                let fg = t.lane(cell.color());
                if let Some(bc) = buf.cell_mut((cx, y)) {
                    bc.set_char(ch).set_fg(fg).set_bg(bg);
                }
                // Lanes that go on down to the next commit carry on
                // through the second row.
                if second_row
                    && k % 2 == 0
                    && cell.bits() & DOWN != 0
                    && let Some(bc) = buf.cell_mut((cx, y2))
                {
                    bc.set_char('│').set_fg(fg).set_bg(bg);
                }
            }
        }

        let right = text_x + text_w;
        let mut lx = text_x;
        if let Some(labels) = app.labels.get(&c.id) {
            for l in labels {
                if lx + 4 >= right {
                    break;
                }
                let lbg = match l.kind {
                    LabelKind::Head => t.badge_head,
                    LabelKind::Local => t.badge_local,
                    LabelKind::Remote => t.badge_remote,
                    LabelKind::Tag => t.badge_tag,
                };
                let name = truncate(&l.name.to_str_lossy(), 24);
                let mut s = Style::default().bg(lbg).fg(t.badge_fg);
                if l.kind == LabelKind::Head {
                    s = s.add_modifier(Modifier::BOLD);
                }
                lx = put(
                    buf,
                    lx,
                    y,
                    &format!(" {name} "),
                    right.saturating_sub(lx),
                    s,
                ) + 1;
            }
        }
        let summary_w = right.saturating_sub(lx);
        put(
            buf,
            lx,
            y,
            &truncate(&c.summary, usize::from(summary_w)),
            summary_w,
            st,
        );

        if !second_row {
            continue;
        }
        // The short id goes on the right, if there's room for it after
        // the author and the date.
        let id = c.id.to_hex_with_len(7).to_string();
        let date = if app.cfg.relative_dates {
            short_date(c.time, now)
        } else {
            format_day(c.time, c.offset)
        };
        let meta = format!("{} · {date}", c.author);
        let id_w = width(&id, 4) as u16;
        let meta_w = if usize::from(text_w) >= width(&meta, 4) + usize::from(id_w) + 2 {
            put(buf, right - id_w, y2, &id, id_w, st.fg(t.faint));
            text_w - id_w - 2
        } else {
            text_w
        };
        put(
            buf,
            text_x,
            y2,
            &truncate(&meta, usize::from(meta_w)),
            meta_w,
            st.fg(t.dim),
        );
    }
    if let Some(err) = &h.error {
        let y = a.y + (end - start) as u16 * HISTORY_ROW_HEIGHT + 1;
        if y < a.bottom() {
            let w = a.width.saturating_sub(2);
            let msg = format!("Couldn't read the history: {err}");
            for (line, y) in wrap(&msg, usize::from(w)).iter().zip(y..a.bottom()) {
                put(buf, a.x + 1, y, line, w, Style::default().fg(t.error));
            }
        }
    }
}

fn draw_detail(app: &App, buf: &mut Buffer, hits: &mut Hits) {
    let d = app.areas.detail;
    let t = &app.theme;
    if d.width == 0 {
        return;
    }
    if app.detail.source == Source::Workdir && app.areas.message.height > 0 {
        let label_y = d.y;
        let focused = app.focus == Focus::Message;
        put(
            buf,
            d.x + 1,
            label_y,
            if app.amend {
                "Amend last commit"
            } else {
                "Commit message"
            },
            d.width / 2,
            Style::default()
                .fg(if focused { t.accent } else { t.dim })
                .add_modifier(Modifier::BOLD),
        );
        let staged = app.workdir.as_ref().map_or(0, |w| w.staged.len());
        let label = if app.amend { "Amend" } else { "Commit" };
        let amend_label = if app.amend { "☑ Amend" } else { "☐ Amend" };
        buttons_right(
            app,
            buf,
            hits,
            d.x + 1,
            d.right().saturating_sub(1),
            label_y,
            vec![
                (amend_label, Cmd::ToggleAmend, None),
                (
                    label,
                    Cmd::Commit,
                    (staged > 0 || app.amend).then_some(t.badge_head),
                ),
            ],
        );
        let m = app.areas.message;
        if !focused {
            // The text was drawn earlier. Give the box a subtle edge.
            let edge = Rect::new(d.x, m.y, 1, m.height);
            for y in edge.top()..edge.bottom() {
                if let Some(c) = buf.cell_mut((edge.x, y)) {
                    c.set_char('▏').set_fg(t.border);
                }
            }
        }
        // Summary length hint, like most git tools.
        let summary_len = app
            .message
            .text()
            .lines()
            .next()
            .map_or(0, |l| l.chars().count());
        if summary_len > 0 {
            let s = format!("{summary_len}");
            let color = if summary_len > 72 {
                t.error
            } else if summary_len > 50 {
                t.warn
            } else {
                t.faint
            };
            put(
                buf,
                d.right().saturating_sub(s.len() as u16 + 1),
                m.bottom(),
                &s,
                4,
                Style::default().fg(color),
            );
        }
        hits.push((m, Target::Message));
    }
    draw_doc(app, buf, hits);
}

fn status_color(t: &Theme, s: FileStatus) -> Color {
    match s {
        FileStatus::Added | FileStatus::Untracked => t.add_fg,
        FileStatus::Deleted => t.del_fg,
        FileStatus::Renamed | FileStatus::Copied => t.rename_fg,
        FileStatus::Conflicted => t.warn,
        FileStatus::Modified | FileStatus::TypeChange => t.accent,
    }
}

fn draw_doc(app: &App, buf: &mut Buffer, hits: &mut Hits) {
    let a = app.areas.doc;
    let t = &app.theme;
    let det = &app.detail;
    let focused = app.focus == Focus::Detail;
    let staging = det.source == Source::Workdir;
    let sel = det.selection();
    let g = det.gutter as u16;
    for (i, row) in det
        .rows
        .iter()
        .enumerate()
        .skip(det.scroll)
        .take(usize::from(a.height))
    {
        let y = a.y + (i - det.scroll) as u16;
        let rr = Rect::new(a.x, y, a.width, 1);
        hits.push((rr, Target::DocRow(i)));
        let is_cursor = i == det.cursor && focused;
        let in_sel = sel.is_some_and(|(lo, hi)| (lo..=hi).contains(&i));
        match row {
            Row::Empty(text) => {
                put(
                    buf,
                    a.x + 2,
                    y + 1,
                    text,
                    a.width.saturating_sub(4),
                    Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
                );
            }
            Row::Blank => {
                if is_cursor {
                    fill(buf, rr, t.sel_unfocused);
                }
            }
            Row::Meta(m) => {
                let bg = if is_cursor { t.sel_unfocused } else { t.bg };
                fill(buf, rr, bg);
                if let Some(ml) = det.meta.get(*m)
                    && buf.area.contains(Position::new(a.x + 2, y))
                {
                    put_line(
                        buf,
                        a.x + 2,
                        y,
                        &ml.line,
                        det.hscroll,
                        a.width.saturating_sub(3),
                    );
                }
            }
            Row::Section(sec) => {
                let bg = if is_cursor { t.sel } else { t.bar };
                fill(buf, rr, bg);
                let count = app.section_files(*sec).len();
                let arrow = if det.collapsed_sections.contains(sec) {
                    '▸'
                } else {
                    '▾'
                };
                let label_end = put(
                    buf,
                    a.x + 1,
                    y,
                    &format!("{arrow} {} ({count})", sec.title()),
                    a.width,
                    Style::default()
                        .bg(bg)
                        .fg(t.fg)
                        .add_modifier(Modifier::BOLD),
                );
                let items = match sec {
                    Section::Staged => {
                        vec![("Unstage all", Cmd::Unstage(Unit::Section(*sec)), None)]
                    }
                    Section::Conflicted => vec![],
                    _ => vec![
                        ("Stage all", Cmd::Stage(Unit::Section(*sec)), None),
                        ("Discard all", Cmd::Discard(Unit::Section(*sec)), None),
                    ],
                };
                buttons_right(
                    app,
                    buf,
                    hits,
                    label_end + 2,
                    a.right().saturating_sub(1),
                    y,
                    items,
                );
            }
            Row::File { sec, file } => {
                let Some(f) = app.section_files(*sec).get(*file) else {
                    continue;
                };
                let bg = if is_cursor { t.sel } else { t.file_bg };
                fill(buf, rr, bg);
                let st = Style::default().bg(bg).fg(t.fg);
                let folded = det.collapsed.contains(&(*sec, f.path().to_owned()));
                let mut x = put(
                    buf,
                    a.x + 1,
                    y,
                    if folded { "▸" } else { "▾" },
                    1,
                    st.fg(t.dim),
                );
                x = put(
                    buf,
                    x + 1,
                    y,
                    &f.status.letter().to_string(),
                    1,
                    st.fg(status_color(t, f.status))
                        .add_modifier(Modifier::BOLD),
                );
                let (adds, dels) = f.added_removed();
                let items: Vec<(&str, Cmd, Option<Color>)> = if staging {
                    match sec {
                        Section::Staged => vec![(
                            "Unstage",
                            Cmd::Unstage(Unit::File(*sec, f.path().to_owned())),
                            None,
                        )],
                        Section::Conflicted => {
                            let p = f.path().to_owned();
                            vec![
                                ("Ours", Cmd::TakeSide(p.clone(), true), None),
                                ("Theirs", Cmd::TakeSide(p.clone(), false), None),
                                ("Resolved", Cmd::MarkResolved(p), None),
                            ]
                        }
                        _ => vec![
                            (
                                "Stage",
                                Cmd::Stage(Unit::File(*sec, f.path().to_owned())),
                                None,
                            ),
                            (
                                "Discard",
                                Cmd::Discard(Unit::File(*sec, f.path().to_owned())),
                                None,
                            ),
                        ],
                    }
                } else {
                    Vec::new()
                };
                // Keep room for the counts and the start of the path.
                let counts = format!("+{adds} −{dels}");
                let room =
                    width(&counts, 4) as u16 + 2 + (width(&f.display_path(), 4) as u16).min(16);
                let bx = buttons_right(
                    app,
                    buf,
                    hits,
                    x + 2 + room,
                    a.right().saturating_sub(1),
                    y,
                    items,
                );
                let cw = width(&counts, 4) as u16;
                let cx = bx.saturating_sub(cw + 2);
                if !f.binary {
                    let ax = put(buf, cx, y, &format!("+{adds}"), cw, st.fg(t.add_fg));
                    put(buf, ax + 1, y, &format!("−{dels}"), cw, st.fg(t.del_fg));
                }
                put(
                    buf,
                    x + 1,
                    y,
                    &truncate(&f.display_path(), usize::from(cx.saturating_sub(x + 2))),
                    cx.saturating_sub(x + 2),
                    st.add_modifier(Modifier::BOLD),
                );
            }
            Row::Notice { text, .. } => {
                let bg = if is_cursor { t.sel_unfocused } else { t.bg };
                fill(buf, rr, bg);
                put(
                    buf,
                    a.x + 3,
                    y,
                    text,
                    a.width.saturating_sub(4),
                    Style::default()
                        .bg(bg)
                        .fg(t.dim)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            Row::Hunk { sec, file, hunk } => {
                let Some(f) = app.section_files(*sec).get(*file) else {
                    continue;
                };
                let Some(h) = f.hunks.get(*hunk) else {
                    continue;
                };
                let bg = if is_cursor { t.sel } else { t.hunk_bg };
                fill(buf, rr, bg);
                let items: Vec<(&str, Cmd, Option<Color>)> =
                    if staging && *sec != Section::Conflicted {
                        let lines =
                            det.selected_lines(|s| app.section_files(s))
                                .filter(|(s, f, l)| {
                                    s == sec && f == file && l.iter().any(|(hh, _)| hh == hunk)
                                });
                        match (sec, lines) {
                            (Section::Staged, Some((s, _, l))) => vec![(
                                "Unstage lines",
                                Cmd::Unstage(Unit::Lines(s, f.clone(), l)),
                                None,
                            )],
                            (Section::Staged, None) => vec![(
                                "Unstage hunk",
                                Cmd::Unstage(Unit::Hunk(*sec, f.clone(), *hunk)),
                                None,
                            )],
                            (_, Some((s, _, l))) => vec![
                                (
                                    "Stage lines",
                                    Cmd::Stage(Unit::Lines(s, f.clone(), l.clone())),
                                    None,
                                ),
                                (
                                    "Discard lines",
                                    Cmd::Discard(Unit::Lines(s, f.clone(), l)),
                                    None,
                                ),
                            ],
                            (_, None) => vec![
                                (
                                    "Stage hunk",
                                    Cmd::Stage(Unit::Hunk(*sec, f.clone(), *hunk)),
                                    None,
                                ),
                                (
                                    "Discard hunk",
                                    Cmd::Discard(Unit::Hunk(*sec, f.clone(), *hunk)),
                                    None,
                                ),
                            ],
                        }
                    } else {
                        Vec::new()
                    };
                let header = h.header();
                let room = (width(&header, 4) as u16).min(18);
                let bx = buttons_right(
                    app,
                    buf,
                    hits,
                    a.x + 2 + room,
                    a.right().saturating_sub(1),
                    y,
                    items,
                );
                put(
                    buf,
                    a.x + 1,
                    y,
                    &header,
                    bx.saturating_sub(a.x + 2),
                    Style::default().bg(bg).fg(t.dim),
                );
            }
            Row::Line {
                sec,
                file,
                hunk,
                line,
            } => {
                let Some(f) = app.section_files(*sec).get(*file) else {
                    continue;
                };
                let Some(l) = f.hunks.get(*hunk).and_then(|h| h.lines.get(*line)) else {
                    continue;
                };
                let spans = det
                    .highlights
                    .get(&(*sec as u8, *file))
                    .and_then(|h| h.lines.get(*hunk))
                    .and_then(|h| h.get(*line));
                let marks = det
                    .marks
                    .get(&(*sec, *file))
                    .and_then(|m| m.get(*hunk))
                    .and_then(|m| m.get(*line));
                let (bg, emph) = line_colors(t, l.kind);
                fill(buf, rr, bg);
                let gutter_bg = if in_sel {
                    t.line_sel
                } else if is_cursor {
                    t.sel
                } else {
                    bg
                };
                let gw = g * 2 + 3;
                fill(buf, Rect::new(a.x, y, gw.min(a.width), 1), gutter_bg);
                let gs = Style::default().bg(gutter_bg).fg(t.faint);
                if in_sel {
                    put(buf, a.x, y, "▌", 1, gs.fg(t.accent));
                }
                let num = |n: Option<u32>| n.map_or_else(String::new, |n| n.to_string());
                put(
                    buf,
                    a.x + 1,
                    y,
                    &format!("{:>w$}", num(l.old_no), w = usize::from(g)),
                    g,
                    gs,
                );
                put(
                    buf,
                    a.x + 2 + g,
                    y,
                    &format!("{:>w$}", num(l.new_no), w = usize::from(g)),
                    g,
                    gs,
                );
                let sign = match l.kind {
                    LineKind::Add | LineKind::Del => "▎",
                    LineKind::Context => " ",
                };
                let tx = a.x + gw;
                put(
                    buf,
                    tx,
                    y,
                    sign,
                    1,
                    Style::default().bg(bg).fg(if l.kind == LineKind::Add {
                        t.add_fg
                    } else {
                        t.del_fg
                    }),
                );
                draw_code(
                    buf,
                    tx + 1,
                    y,
                    a.right().saturating_sub(tx + 1),
                    &l.text,
                    det.hscroll,
                    Style::default().bg(bg).fg(t.fg),
                    spans,
                    t.syntax,
                    marks.map(Vec::as_slice),
                    emph,
                    app.cfg.tab_width,
                );
                if l.no_eol {
                    let w = width(&l.text, app.cfg.tab_width) as u16;
                    let x = tx + 2 + w.saturating_sub(det.hscroll as u16);
                    if x < a.right() {
                        put(buf, x, y, "⏎̸", 2, Style::default().bg(bg).fg(t.faint));
                    }
                }
            }
            Row::Split {
                sec,
                file,
                hunk,
                left,
                right,
            } => {
                let Some(f) = app.section_files(*sec).get(*file) else {
                    continue;
                };
                let Some(h) = f.hunks.get(*hunk) else {
                    continue;
                };
                let hl = det
                    .highlights
                    .get(&(*sec as u8, *file))
                    .and_then(|x| x.lines.get(*hunk));
                let marks = det.marks.get(&(*sec, *file)).and_then(|m| m.get(*hunk));
                let half = a.width / 2;
                for (side, idx) in [(0u16, left), (1u16, right)] {
                    let x0 = a.x + side * half;
                    let w = if side == 0 { half } else { a.width - half };
                    let r = Rect::new(x0, y, w, 1);
                    let Some(li) = idx else {
                        fill(buf, r, t.panel);
                        continue;
                    };
                    let l = &h.lines[*li];
                    let (bg, emph) = line_colors(t, l.kind);
                    fill(buf, r, bg);
                    let gutter_bg = if in_sel {
                        t.line_sel
                    } else if is_cursor {
                        t.sel
                    } else {
                        bg
                    };
                    fill(buf, Rect::new(x0, y, (g + 2).min(w), 1), gutter_bg);
                    if in_sel && side == 0 {
                        put(
                            buf,
                            x0,
                            y,
                            "▌",
                            1,
                            Style::default().bg(gutter_bg).fg(t.accent),
                        );
                    }
                    let no = if side == 0 { l.old_no } else { l.new_no };
                    put(
                        buf,
                        x0 + 1,
                        y,
                        &format!(
                            "{:>w$}",
                            no.map_or(String::new(), |n| n.to_string()),
                            w = usize::from(g)
                        ),
                        g,
                        Style::default().bg(gutter_bg).fg(t.faint),
                    );
                    let spans = hl.and_then(|v| v.get(*li));
                    let mk = marks.and_then(|m| m.get(*li));
                    draw_code(
                        buf,
                        x0 + g + 2,
                        y,
                        w.saturating_sub(g + 3),
                        &l.text,
                        det.hscroll,
                        Style::default().bg(bg).fg(t.fg),
                        spans,
                        t.syntax,
                        mk.map(Vec::as_slice),
                        emph,
                        app.cfg.tab_width,
                    );
                }
                if let Some(c) = buf.cell_mut((a.x + half - 1, y)) {
                    c.set_char('│').set_fg(t.border);
                }
            }
        }
    }
}

fn line_colors(t: &Theme, kind: LineKind) -> (Color, Color) {
    match kind {
        LineKind::Add => (t.add_bg, t.add_emph),
        LineKind::Del => (t.del_bg, t.del_emph),
        LineKind::Context => (t.bg, t.bg),
    }
}

/// Draw one line of code with syntax colors and word emphasis.
#[allow(clippy::too_many_arguments)]
fn draw_code(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    skip: usize,
    base: Style,
    spans: Option<&LineSpans>,
    syntax: &Syntax,
    marks: Option<&[(usize, usize)]>,
    emph: Color,
    tab: usize,
) {
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut span_i = 0;
    text::draw(buf, x, y, text, skip, usize::from(w), tab, |cell| {
        let mut style = base;
        if let Some(sp) = spans {
            while span_i < sp.len() && (sp[span_i].1 as usize) <= cell.byte {
                span_i += 1;
            }
            if let Some(&(s, e, kind)) = sp.get(span_i)
                && (s as usize) <= cell.byte
                && cell.byte < e as usize
                && let Some(c) = syntax.color(kind)
            {
                style = style.fg(c);
            }
        }
        if marks.is_some_and(|m| m.iter().any(|&(s, e)| s <= cell.byte && cell.byte < e)) {
            style = style.bg(emph);
        }
        style
    });
}

fn draw_planner(app: &App, buf: &mut Buffer, hits: &mut Hits) {
    let Some(p) = &app.planner else { return };
    let a = app.areas.planner;
    let t = &app.theme;
    fill(buf, a, t.bg);
    let base = p.base.map_or_else(
        || "the root".to_string(),
        |b| b.to_hex_with_len(7).to_string(),
    );
    put(
        buf,
        a.x + 1,
        a.y,
        &format!("Edit history: {} commits on top of {base}", p.rows.len()),
        a.width.saturating_sub(2),
        Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
    );
    put(
        buf,
        a.x + 1,
        a.y + 1,
        "Newest at the top. Drag rows or use ctrl+j/k to reorder. Click an action to change it.",
        a.width.saturating_sub(2),
        Style::default().fg(t.dim),
    );
    let list_y = a.y + 3;
    let h = a.height.saturating_sub(5);
    for (i, r) in p
        .rows
        .iter()
        .enumerate()
        .skip(p.scroll)
        .take(usize::from(h))
    {
        let y = list_y + (i - p.scroll) as u16;
        let rr = Rect::new(a.x, y, a.width, 1);
        let dragging = matches!(app.drag, Some(Drag::Planner(d)) if d == i);
        let bg = if dragging {
            t.line_sel
        } else {
            row_bg(app, i == p.selected, true, rr, t.bg)
        };
        fill(buf, rr, bg);
        let color = match r.action {
            PlanAction::Pick => t.button,
            PlanAction::Reword => t.badge_head,
            PlanAction::Squash | PlanAction::Fixup => t.badge_tag,
            PlanAction::Drop => t.badge_danger,
            PlanAction::Edit => t.badge_local,
        };
        let ar = Rect::new(a.x + 1, y, 9, 1);
        put(
            buf,
            ar.x,
            y,
            &format!(" {:<7} ", r.action.name()),
            9,
            Style::default().bg(color).fg(t.badge_fg),
        );
        hits.push((rr, Target::PlannerRow(i)));
        hits.push((ar, Target::PlannerAction(i)));
        let st = Style::default().bg(bg).fg(t.fg);
        let x = put(
            buf,
            a.x + 12,
            y,
            &r.id.to_hex_with_len(7).to_string(),
            8,
            st.fg(t.accent),
        );
        let mut sum_style = st;
        if r.action == PlanAction::Drop {
            sum_style = sum_style.fg(t.faint).add_modifier(Modifier::CROSSED_OUT);
        }
        let x = put(
            buf,
            x + 1,
            y,
            &truncate(&r.summary, usize::from(a.right().saturating_sub(x + 2))),
            a.right().saturating_sub(x + 2),
            sum_style,
        );
        if let Some(m) = &r.message {
            let first = m.lines().next().unwrap_or("");
            put(
                buf,
                x + 2,
                y,
                &format!("→ {first}"),
                a.right().saturating_sub(x + 3),
                st.fg(t.dim).add_modifier(Modifier::ITALIC),
            );
        }
        if matches!(r.action, PlanAction::Squash | PlanAction::Fixup) {
            put(buf, a.x + 10, y, "↓", 1, st.fg(t.warn));
        }
    }
    let by = a.bottom().saturating_sub(1);
    buttons_right(
        app,
        buf,
        hits,
        a.x + 1,
        a.right().saturating_sub(1),
        by,
        vec![
            ("Cancel", Cmd::PlanCancel, None),
            ("Start rebase", Cmd::PlanStart, Some(t.badge_head)),
        ],
    );
}

fn draw_bottom(app: &App, buf: &mut Buffer, r: Rect, hits: &mut Hits) {
    let t = &app.theme;
    fill(buf, r, t.bar);
    let base = Style::default().bg(t.bar).fg(t.dim);
    if let Some((msg, level, _)) = &app.status {
        let color = match level {
            Level::Info => t.fg,
            Level::Ok => t.ok,
            Level::Warn => t.warn,
            Level::Error => t.error,
        };
        put(
            buf,
            r.x + 1,
            r.y,
            &truncate(msg, usize::from(r.width.saturating_sub(2))),
            r.width.saturating_sub(2),
            base.fg(color),
        );
        return;
    }
    let mut actions: Vec<ActionDef> = app
        .actions(app.context())
        .into_iter()
        .filter(|a| a.hint && a.enabled)
        .collect();
    actions.extend(app.global_actions().into_iter().filter(|a| a.hint));
    let mut x = r.x + 1;
    for a in actions {
        let Some(k) = a.key else { continue };
        let key = k.label();
        let w = (width(&key, 4) + width(&a.label, 4) + 1) as u16;
        if x + w + 2 > r.right() {
            break;
        }
        let rr = Rect::new(x, r.y, w, 1);
        let bg = if hovered(app, rr) {
            t.button_hover
        } else {
            t.bar
        };
        let nx = put(buf, x, r.y, &key, w, base.bg(bg).fg(t.accent));
        put(buf, nx + 1, r.y, &a.label, w, base.bg(bg));
        hits.push((rr, Target::Button(a.cmd)));
        x += w + 3;
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 3,
        w,
        h,
    )
}

fn frame(buf: &mut Buffer, r: Rect, t: &Theme, title: Option<&str>) {
    Clear.render(r, buf);
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.border).bg(t.panel))
        .style(Style::default().bg(t.panel).fg(t.fg));
    if let Some(title) = title {
        b = b
            .title(format!(" {title} "))
            .title_style(Style::default().fg(t.fg).add_modifier(Modifier::BOLD));
    }
    b.render(r, buf);
}

fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            if !line.is_empty() && width(&line, 4) + 1 + width(word, 4) > w {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

fn draw_overlays(app: &mut App, buf: &mut Buffer, area: Rect, hits: &mut Hits) -> Option<Position> {
    let theme = app.theme.clone();
    let t = &theme;
    let mouse = app.mouse;
    let hover =
        |r: Rect| mouse.0 >= r.x && mouse.0 < r.right() && mouse.1 >= r.y && mouse.1 < r.bottom();
    let log = if matches!(app.overlays.last(), Some(Overlay::Log { .. })) {
        app.repo.git.log_entries()
    } else {
        Vec::new()
    };
    let mut cursor = None;
    let mut input_area = None;
    let n = app.overlays.len();
    for (oi, ov) in app.overlays.iter_mut().enumerate() {
        let top = oi + 1 == n;
        if top {
            // Only the topmost overlay takes clicks. Everything under it,
            // including the top bar buttons, is out of reach while it's open.
            hits.clear();
        }
        match ov {
            Overlay::Menu(m) => {
                let label_w = m
                    .items
                    .iter()
                    .map(|a| width(&a.label, 4) + a.key.map_or(0, |k| k.label().len() + 3))
                    .max()
                    .unwrap_or(10);
                let w = (label_w as u16 + 4)
                    .max(m.title.as_ref().map_or(0, |s| width(s, 4) as u16 + 6))
                    .min(area.width.saturating_sub(2));
                let max_h = area.height.saturating_sub(2);
                let h = (m.items.len() as u16 + 2).min(max_h);
                let r = match m.anchor {
                    None => centered(area, w, h),
                    Some((x, y)) => {
                        let x = x.min(area.right().saturating_sub(w + 1));
                        let y = if y + h > area.bottom() {
                            area.bottom().saturating_sub(h + 1)
                        } else {
                            y
                        };
                        Rect::new(x, y, w, h)
                    }
                };
                frame(buf, r, t, m.title.as_deref());
                if top {
                    hits.push((r, Target::Overlay));
                }
                let visible = usize::from(h.saturating_sub(2));
                if m.selected < m.scroll {
                    m.scroll = m.selected;
                } else if m.selected >= m.scroll + visible {
                    m.scroll = m.selected + 1 - visible;
                }
                m.scroll = m.scroll.min(m.items.len().saturating_sub(visible));
                for (i, a) in m.items.iter().enumerate().skip(m.scroll).take(visible) {
                    let iy = r.y + 1 + (i - m.scroll) as u16;
                    let ir = Rect::new(r.x + 1, iy, w.saturating_sub(2), 1);
                    let bg = if i == m.selected {
                        t.sel
                    } else if hover(ir) && a.enabled {
                        t.hover
                    } else {
                        t.panel
                    };
                    fill(buf, ir, bg);
                    let fg = if a.enabled { t.fg } else { t.faint };
                    put(
                        buf,
                        ir.x + 1,
                        iy,
                        &a.label,
                        ir.width.saturating_sub(2),
                        Style::default().bg(bg).fg(fg),
                    );
                    if let Some(k) = a.key {
                        let kl = k.label();
                        put(
                            buf,
                            ir.right().saturating_sub(kl.len() as u16 + 1),
                            iy,
                            &kl,
                            kl.len() as u16,
                            Style::default().bg(bg).fg(t.accent),
                        );
                    }
                    if top {
                        hits.push((ir, Target::MenuItem(i)));
                    }
                }
            }
            Overlay::Confirm {
                title,
                body,
                yes,
                yes_label,
            } => {
                let w = 64u16.min(area.width.saturating_sub(4));
                let lines = wrap(body, usize::from(w.saturating_sub(4)));
                let h = lines.len() as u16 + 5;
                let r = centered(area, w, h);
                frame(buf, r, t, Some(title));
                if top {
                    hits.push((r, Target::Overlay));
                }
                for (i, l) in lines.iter().enumerate() {
                    put(
                        buf,
                        r.x + 2,
                        r.y + 2 + i as u16,
                        l,
                        r.width.saturating_sub(4),
                        Style::default().bg(t.panel).fg(t.fg),
                    );
                }
                let by = r.bottom().saturating_sub(2);
                let yes = yes.clone();
                let yes_label = format!("{yes_label} (y)");
                draw_dialog_buttons(
                    t,
                    mouse,
                    buf,
                    hits,
                    r.right().saturating_sub(2),
                    by,
                    vec![
                        ("Cancel (n)", Cmd::CloseOverlay, None),
                        (&yes_label, yes, Some(t.badge_danger)),
                    ],
                    top,
                );
            }
            Overlay::Input(b) => {
                let w = if b.text.multiline { 80u16 } else { 60 }.min(area.width.saturating_sub(4));
                let hint_lines = wrap(&b.hint, usize::from(w.saturating_sub(4)));
                let text_h: u16 = if b.text.multiline { 10 } else { 1 };
                let h = text_h + hint_lines.len() as u16 + 6;
                let r = centered(area, w, h);
                frame(buf, r, t, Some(&b.title));
                if top {
                    hits.push((r, Target::Overlay));
                }
                for (i, l) in hint_lines.iter().enumerate() {
                    put(
                        buf,
                        r.x + 2,
                        r.y + 1 + i as u16,
                        l,
                        r.width.saturating_sub(4),
                        Style::default().bg(t.panel).fg(t.dim),
                    );
                }
                let tr = Rect::new(
                    r.x + 2,
                    r.y + 2 + hint_lines.len() as u16,
                    r.width.saturating_sub(4),
                    text_h,
                );
                let pos = b.text.render(
                    buf,
                    tr,
                    Style::default().bg(t.bg).fg(t.fg),
                    Style::default().bg(t.bg).fg(t.faint),
                );
                if top {
                    hits.push((tr, Target::InputText));
                    cursor = Some(pos);
                    input_area = Some(tr);
                }
                let by = r.bottom().saturating_sub(2);
                let save = if b.text.multiline {
                    "Save (ctrl+enter)"
                } else {
                    "OK (enter)"
                };
                draw_dialog_buttons(
                    t,
                    mouse,
                    buf,
                    hits,
                    r.right().saturating_sub(2),
                    by,
                    vec![
                        ("Cancel (esc)", Cmd::CloseOverlay, None),
                        (save, Cmd::SubmitInput, Some(t.badge_head)),
                    ],
                    top,
                );
            }
            Overlay::Palette {
                input,
                items,
                selected,
            } => {
                let w = 72u16.min(area.width.saturating_sub(4));
                let filtered = palette_filter(items, &input.text());
                let list_h = (filtered.len() as u16).clamp(1, 16);
                let h = list_h + 4;
                let r = Rect::new(
                    area.x + (area.width.saturating_sub(w)) / 2,
                    area.y + 2,
                    w,
                    h.min(area.height.saturating_sub(3)),
                );
                frame(buf, r, t, Some("Commands"));
                if top {
                    hits.push((r, Target::Overlay));
                }
                let tr = Rect::new(r.x + 2, r.y + 1, r.width.saturating_sub(4), 1);
                let pos = input.render(
                    buf,
                    tr,
                    Style::default().bg(t.bg).fg(t.fg),
                    Style::default().bg(t.bg).fg(t.faint),
                );
                if top {
                    cursor = Some(pos);
                }
                let sel = (*selected).min(filtered.len().saturating_sub(1));
                let skip = sel.saturating_sub(usize::from(list_h).saturating_sub(1));
                for (row, &idx) in filtered
                    .iter()
                    .enumerate()
                    .skip(skip)
                    .take(usize::from(list_h))
                {
                    let a = &items[idx];
                    let iy = r.y + 3 + (row - skip) as u16;
                    let ir = Rect::new(r.x + 1, iy, r.width.saturating_sub(2), 1);
                    let bg = if row == sel {
                        t.sel
                    } else if hover(ir) {
                        t.hover
                    } else {
                        t.panel
                    };
                    fill(buf, ir, bg);
                    put(
                        buf,
                        ir.x + 1,
                        iy,
                        &a.label,
                        ir.width.saturating_sub(14),
                        Style::default().bg(bg).fg(t.fg),
                    );
                    if let Some(k) = a.key {
                        let kl = k.label();
                        put(
                            buf,
                            ir.right().saturating_sub(kl.len() as u16 + 1),
                            iy,
                            &kl,
                            14,
                            Style::default().bg(bg).fg(t.accent),
                        );
                    }
                    if top {
                        hits.push((ir, Target::PaletteItem(idx)));
                    }
                }
                if filtered.is_empty() {
                    put(
                        buf,
                        r.x + 2,
                        r.y + 3,
                        "No matching commands",
                        r.width.saturating_sub(4),
                        Style::default().bg(t.panel).fg(t.faint),
                    );
                }
            }
            Overlay::Log { scroll } => {
                let r = centered(
                    area,
                    area.width.saturating_sub(8),
                    area.height.saturating_sub(4),
                );
                frame(buf, r, t, Some("Command log"));
                if top {
                    hits.push((r, Target::Overlay));
                }
                let mut lines: Vec<(String, Style)> = Vec::new();
                for e in &log {
                    let mark = if e.ok { "✓" } else { "✗" };
                    lines.push((
                        format!("{mark} {}", e.command),
                        Style::default()
                            .fg(if e.ok { t.fg } else { t.error })
                            .add_modifier(Modifier::BOLD),
                    ));
                    for l in e.output.lines().filter(|l| !l.trim().is_empty()).take(40) {
                        lines.push((format!("    {l}"), Style::default().fg(t.dim)));
                    }
                }
                if lines.is_empty() {
                    lines.push((
                        "Nothing has been run yet.".into(),
                        Style::default().fg(t.dim),
                    ));
                }
                let h = usize::from(r.height.saturating_sub(2));
                *scroll = (*scroll).min(lines.len().saturating_sub(h));
                for (i, (l, st)) in lines.iter().skip(*scroll).take(h).enumerate() {
                    put(
                        buf,
                        r.x + 2,
                        r.y + 1 + i as u16,
                        l,
                        r.width.saturating_sub(4),
                        st.bg(t.panel),
                    );
                }
            }
            Overlay::Message {
                title,
                body,
                scroll,
                error,
            } => {
                let w = 80u16.min(area.width.saturating_sub(4));
                let lines = wrap(body, usize::from(w.saturating_sub(4)));
                let max_body = area.height.saturating_sub(8);
                let body_h = (lines.len() as u16).min(max_body);
                let r = centered(area, w, body_h + 5);
                frame(buf, r, t, Some(title));
                if top {
                    hits.push((r, Target::Overlay));
                }
                *scroll = (*scroll).min(lines.len().saturating_sub(usize::from(body_h)));
                let color = if *error { t.error } else { t.fg };
                for (i, l) in lines
                    .iter()
                    .skip(*scroll)
                    .take(usize::from(body_h))
                    .enumerate()
                {
                    put(
                        buf,
                        r.x + 2,
                        r.y + 1 + i as u16,
                        l,
                        r.width.saturating_sub(4),
                        Style::default().bg(t.panel).fg(if i == 0 && *scroll == 0 {
                            color
                        } else {
                            t.fg
                        }),
                    );
                }
                draw_dialog_buttons(
                    t,
                    mouse,
                    buf,
                    hits,
                    r.right().saturating_sub(2),
                    r.bottom().saturating_sub(2),
                    vec![("OK", Cmd::CloseOverlay, Some(t.badge_head))],
                    top,
                );
            }
        }
    }
    if let Some(r) = input_area {
        app.areas.input = r;
    }
    cursor
}

#[allow(clippy::too_many_arguments)]
fn draw_dialog_buttons(
    t: &Theme,
    mouse: (u16, u16),
    buf: &mut Buffer,
    hits: &mut Hits,
    right: u16,
    y: u16,
    items: Vec<(&str, Cmd, Option<Color>)>,
    active: bool,
) {
    let total: u16 = items.iter().map(|(l, _, _)| width(l, 4) as u16 + 3).sum();
    let mut x = right.saturating_sub(total);
    for (label, cmd, accent) in items {
        let w = width(label, 4) as u16 + 2;
        let r = Rect::new(x, y, w, 1);
        let hov = mouse.0 >= r.x && mouse.0 < r.right() && mouse.1 == y;
        let bg = if hov {
            t.button_hover
        } else {
            accent.unwrap_or(t.button)
        };
        let fg = if accent.is_some() { t.badge_fg } else { t.fg };
        put(
            buf,
            x,
            y,
            &format!(" {label} "),
            w,
            Style::default().bg(bg).fg(fg),
        );
        if active {
            hits.push((r, Target::Button(cmd)));
        }
        x += w + 1;
    }
}

fn request_visible_highlights(app: &mut App) {
    let a = app.areas.doc;
    let mut files: Vec<(Section, usize)> = Vec::new();
    for row in app
        .detail
        .rows
        .iter()
        .skip(app.detail.scroll)
        .take(usize::from(a.height))
    {
        if let Some(f) = row.file()
            && !files.contains(&f)
        {
            files.push(f);
        }
    }
    for (sec, file) in files {
        app.request_highlight(sec, file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(text: &str, w: u16, skip: usize) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, w + 1, 1));
        buf[(0, 0)].set_char('|');
        draw_code(
            &mut buf,
            1,
            0,
            w,
            text,
            skip,
            Style::default(),
            None,
            Theme::default().syntax,
            None,
            Color::Reset,
            4,
        );
        buf.content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn combining_characters_join_the_previous_cell() {
        assert_eq!(code("e\u{301}x", 4, 0), ["|", "e\u{301}", "x", " ", " "]);
        assert_eq!(code("a\u{301}\u{323}", 2, 0), ["|", "a\u{301}\u{323}", " "]);
        assert_eq!(code("你\u{301}x", 4, 0), ["|", "你\u{301}", "", "x", " "]);
        // The last character that fits still gets its marks.
        assert_eq!(code("ae\u{301}x", 2, 0), ["|", "a", "e\u{301}"]);
    }

    #[test]
    fn emoji_sequences_take_one_wide_cell() {
        assert_eq!(code("❤\u{fe0f}x", 4, 0), ["|", "❤\u{fe0f}", "", "x", " "]);
        let family = "👨\u{200d}👧";
        assert_eq!(code(family, 4, 0), ["|", family, "", " ", " "]);
    }

    #[test]
    fn format_characters_are_dropped() {
        assert_eq!(code("a\u{202e}b\u{200b}c", 4, 0), ["|", "a", "b", "c", " "]);
    }

    #[test]
    fn control_characters_take_a_cell() {
        assert_eq!(code("a\x07b", 4, 0), ["|", "a", "�", "b", " "]);
        assert_eq!(code("\0\u{301}", 2, 0), ["|", "�", " "]);
    }

    #[test]
    fn combining_characters_of_hidden_characters_stay_hidden() {
        // The base character is scrolled off, so its mark must not join the
        // gutter or the next character.
        assert_eq!(code("e\u{301}x", 2, 1), ["|", "x", " "]);
        assert_eq!(code("\u{301}x", 2, 0), ["|", "x", " "]);
    }
}
