//! Measuring text and drawing it into the buffer.
//!
//! Everything that works out how wide text is, or where a piece of it goes
//! on screen, uses `cells`. `design/text-width.md` explains the rules and
//! why we chose them.

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::layout::Position;
use ratatui::style::Style;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

/// One grapheme of some text and the columns it takes up on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell<'a> {
    /// Byte offset of the grapheme in the text.
    pub byte: usize,
    pub text: &'a str,
    /// The column it starts at, counted from the start of the text.
    pub col: usize,
    /// How many columns it takes up, which is 1 or 2, or more for a tab.
    pub width: usize,
}

impl<'a> Cell<'a> {
    /// What to write into the first column of the cell.
    pub fn symbol(&self) -> &'a str {
        if self.text == "\t" {
            " "
        } else if self.text.contains(char::is_control) {
            "�"
        } else {
            self.text
        }
    }
}

/// Split text into graphemes and work out the columns each one takes up.
///
/// A tab reaches the next multiple of `tab` columns. A grapheme with a
/// control character in it is drawn as one replacement character. Other
/// graphemes are as wide as ratatui measures them, because its buffer diff
/// uses that width to decide which cells a wide character covers. We cap
/// that at 2, since a terminal never gives a grapheme more than two cells.
/// Graphemes with no width, such as a bidi override on its own, are left
/// out, so they never reach the terminal.
pub fn cells(text: &str, tab: usize) -> impl Iterator<Item = Cell<'_>> {
    let tab = tab.max(1);
    let mut col = 0;
    text.grapheme_indices(true).filter_map(move |(byte, g)| {
        let width = if g == "\t" {
            tab - col % tab
        } else if g.contains(char::is_control) {
            1
        } else {
            usize::from(g.cell_width().min(2))
        };
        if width == 0 {
            return None;
        }
        let cell = Cell {
            byte,
            text: g,
            col,
            width,
        };
        col += width;
        Some(cell)
    })
}

/// How many columns text takes up, with tabs reaching multiples of `tab`.
pub fn width(s: &str, tab: usize) -> usize {
    cells(s, tab).last().map_or(0, |c| c.col + c.width)
}

/// Cut a string to at most `max` columns, adding an ellipsis if it was cut.
/// It measures tabs the way `put` draws them.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s, PUT_TAB) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let end = cells(s, PUT_TAB)
        .find(|c| c.col + c.width >= max)
        .map_or(s.len(), |c| c.byte);
    format!("{}…", &s[..end])
}

/// The tab width for labels, names and messages, as opposed to file
/// contents, which use the configured tab width.
const PUT_TAB: usize = 4;

/// Draw text on row `y`, starting at `x`, and return the x just past the
/// last cell drawn.
///
/// The first `skip` columns of the text are scrolled off to the left, and
/// at most `max` columns are drawn. A wide grapheme that would only partly
/// fit at either edge is left out. `style` gives the style of each cell,
/// which is patched onto the style already in the buffer.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &str,
    skip: usize,
    max: usize,
    tab: usize,
    mut style: impl FnMut(&Cell) -> Style,
) -> u16 {
    if max == 0 || !buf.area.contains(Position::new(x, y)) {
        return x;
    }
    let right = usize::from(buf.area.right()).min(usize::from(x) + max);
    let mut end = x;
    for cell in cells(text, tab) {
        if cell.col < skip {
            continue;
        }
        let cx = usize::from(x) + cell.col - skip;
        if cx + cell.width > right {
            break;
        }
        let st = style(&cell);
        // The buffer is narrower than u16::MAX, so these columns fit.
        let cx = cx as u16;
        let w = cell.width as u16;
        let first = &mut buf[(cx, y)];
        first.set_symbol(cell.symbol()).set_style(st);
        // Ratatui works out from the symbol which cells a wide character
        // covers. Tell it the width we used when that differs.
        let forced = if cell.text != "\t" && first.symbol().cell_width() != w {
            NonZeroU16::new(w).map_or(CellDiffOption::None, CellDiffOption::ForcedWidth)
        } else {
            CellDiffOption::None
        };
        first.set_diff_option(forced);
        for k in 1..w {
            let c = &mut buf[(cx + k, y)];
            c.set_symbol(if cell.text == "\t" { " " } else { "" })
                .set_style(st);
            c.set_diff_option(CellDiffOption::None);
        }
        end = cx + w;
    }
    end
}

/// Draw a string in one style. See `draw`.
pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max: u16, style: Style) -> u16 {
    draw(buf, x, y, s, 0, usize::from(max), PUT_TAB, |_| style)
}

/// Draw a ratatui `Line` span by span. Tabs in each span are measured from
/// the start of that span. The first `skip` columns of the whole line are
/// scrolled off to the left, as in `draw`.
pub fn put_line(buf: &mut Buffer, x: u16, y: u16, line: &Line, skip: usize, max: u16) -> u16 {
    let mut end = x;
    let mut col = 0;
    for span in &line.spans {
        let w = width(&span.content, PUT_TAB);
        let start = col;
        col += w;
        if col <= skip {
            continue;
        }
        // Place each span by the widths before it rather than by where the
        // last one stopped drawing. A wide grapheme cut off at the left edge
        // draws nothing, but it still takes up its columns.
        let offset = start.saturating_sub(skip);
        if offset >= usize::from(max) {
            break;
        }
        let sx = x + offset as u16;
        let next = draw(
            buf,
            sx,
            y,
            &span.content,
            skip.saturating_sub(start),
            usize::from(max) - offset,
            PUT_TAB,
            |_| line.style.patch(span.style),
        );
        end = end.max(next);
    }
    end
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::layout::Rect;
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;

    use super::*;

    fn pieces(s: &str) -> Vec<(usize, &str, usize)> {
        cells(s, 4).map(|c| (c.byte, c.text, c.width)).collect()
    }

    #[test]
    fn widths() {
        assert_eq!(width("a\tb", 4), 5);
        assert_eq!(width("日本", 4), 4);
        assert_eq!(width("a\x07\0b", 4), 4);
        assert_eq!(
            pieces("e\u{301}\u{323}x"),
            [(0, "e\u{301}\u{323}", 1), (5, "x", 1)]
        );
        assert_eq!(pieces("\u{202e}a"), [(3, "a", 1)]);
        // Emoji sequences are one grapheme two columns wide.
        assert_eq!(pieces("❤\u{fe0f}"), [(0, "❤\u{fe0f}", 2)]);
        assert_eq!(pieces("👍🏽"), [(0, "👍🏽", 2)]);
        assert_eq!(
            pieces("👨\u{200d}👩\u{200d}👧"),
            [(0, "👨\u{200d}👩\u{200d}👧", 2)]
        );
        // unicode-width measures these as 3 and 4.
        assert_eq!(pieces("a🏽"), [(0, "a🏽", 2)]);
        assert_eq!(
            pieces("\u{1100}\u{1100}\u{1161}"),
            [(0, "\u{1100}\u{1100}\u{1161}", 2)]
        );
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hi", 6), "hi");
        assert_eq!(truncate("ab👍🏽cd", 4), "ab…");
    }

    fn drawn(text: &str, skip: usize, w: u16) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
        draw(&mut buf, 0, 0, text, skip, usize::from(w), 4, |_| {
            Style::default()
        });
        buf.content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn drawing() {
        assert_eq!(drawn("a\tb\x07", 0, 7), ["a", " ", " ", " ", "b", "�", " "]);
        assert_eq!(drawn("e\u{301}👍🏽x", 0, 5), ["e\u{301}", "👍🏽", "", "x", " "]);
        // A wide grapheme cut by either edge is left out.
        assert_eq!(drawn("日本x", 1, 4), [" ", "本", "", "x"]);
        assert_eq!(drawn("ab日", 0, 3), ["a", "b", " "]);
    }

    #[test]
    fn line_scrolled() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("日"), Span::raw("cd")]);
        let drawn = |skip, w| {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
            put_line(&mut buf, 0, 0, &line, skip, w);
            buf.content()
                .iter()
                .map(|c| c.symbol().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(drawn(0, 4), ["a", "b", "日", ""]);
        assert_eq!(drawn(1, 4), ["b", "日", "", "c"]);
        // The cut-off half of 日 still pushes the next span right.
        assert_eq!(drawn(3, 4), [" ", "c", "d", " "]);
        assert_eq!(drawn(4, 4), ["c", "d", " ", " "]);
        assert_eq!(drawn(9, 4), [" ", " ", " ", " "]);
    }

    #[test]
    fn forced_width_when_capped() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        draw(&mut buf, 0, 0, "a🏽b", 0, 4, 4, |_| Style::default());
        assert_eq!(
            buf[(0, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap_or(NonZeroU16::MIN))
        );
        assert_eq!(buf[(2, 0)].symbol(), "b");
        let diff: Vec<_> = Buffer::empty(buf.area)
            .diff(&buf)
            .into_iter()
            .map(|(x, _, c)| (x, c.symbol().to_string()))
            .collect();
        assert_eq!(diff, [(0, "a🏽".to_string()), (2, "b".to_string())]);
    }

    /// Characters that are tricky to measure: tabs, control and format
    /// characters, combining accents, wide characters, and the parts of
    /// emoji sequences.
    const TRICKY: &str =
        "[a \t\u{7}é日❤👨👩\u{1f3fd}\u{301}\u{200b}\u{200d}\u{202e}\u{fe0f}\u{1100}\u{1161}]{0,20}";

    proptest! {
        #[test]
        fn cells_cover_the_text(s in TRICKY) {
            let mut col = 0;
            let mut end = 0;
            for c in cells(&s, 4) {
                prop_assert!(c.byte >= end && s[c.byte..].starts_with(c.text), "{:?} in {:?}", c, s);
                prop_assert_eq!(c.col, col);
                prop_assert!(c.width >= 1 && (c.width <= 2 || c.text == "\t"), "{:?}", c);
                // Everything we skip over draws nothing.
                prop_assert_eq!(s[end..c.byte].width(), 0, "{:?} before {:?}", &s[end..c.byte], c);
                col += c.width;
                end = c.byte + c.text.len();
            }
            prop_assert_eq!(col, width(&s, 4));
        }

        #[test]
        fn drawing_fills_the_columns_we_count(s in TRICKY, skip in 0usize..6) {
            let w = 12u16;
            let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
            let end = draw(&mut buf, 0, 0, &s, skip, usize::from(w), 4, |_| Style::default());
            // Walking the buffer the way ratatui's diff does lands on the
            // start of every cell we drew, and every column we skip over
            // holds something one column wide.
            let drawn: Vec<Cell> = cells(&s, 4)
                .filter(|c| c.col >= skip && c.col + c.width <= skip + usize::from(w))
                .collect();
            let mut x = 0;
            for c in &drawn {
                let start = (c.col - skip) as u16;
                while x < start {
                    prop_assert_eq!(buf[(x, 0)].cell_width(), 1);
                    x += 1;
                }
                prop_assert_eq!(x, start, "the diff skips past {:?}", c);
                x += buf[(x, 0)].cell_width().max(1);
            }
            let want = drawn.last().map_or(0, |c| c.col - skip + c.width);
            prop_assert!(drawn.is_empty() || usize::from(end) == want, "end {} but cells end at {}", end, want);
        }

        #[test]
        fn truncate_fits(s in "[ab \t日é👍🏽]{0,20}", max in 0usize..25) {
            let t = truncate(&s, max);
            prop_assert!(width(&t, 4) <= max, "{:?} is wider than {}", t, max);
            if width(&s, 4) <= max {
                prop_assert_eq!(t, s);
            } else if max > 0 {
                let kept = t.strip_suffix('…');
                prop_assert!(kept.is_some_and(|k| s.starts_with(k)), "{:?} from {:?}", t, s);
            }
        }
    }
}
