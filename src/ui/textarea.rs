//! A small text editing widget for commit messages, names and prompts.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

use crate::util::char_width;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The key changed the text or moved the cursor.
    Handled,
    Submit,
    Cancel,
    /// Not a key the text area uses. The caller may handle it.
    Ignored,
}

#[derive(Debug, Clone, Default)]
pub struct TextArea {
    lines: Vec<String>,
    /// Cursor row and column, with the column counted in chars.
    row: usize,
    col: usize,
    scroll_row: usize,
    scroll_col: usize,
    pub multiline: bool,
    pub masked: bool,
    pub placeholder: String,
}

fn byte_idx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

impl TextArea {
    pub fn new(multiline: bool) -> Self {
        TextArea {
            lines: vec![String::new()],
            multiline,
            ..Default::default()
        }
    }

    pub fn with_text(multiline: bool, text: &str) -> Self {
        let mut t = Self::new(multiline);
        t.set_text(text);
        t
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = if self.multiline {
            text.split('\n').map(str::to_string).collect()
        } else {
            vec![text.replace('\n', " ")]
        };
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
        self.scroll_row = 0;
        self.scroll_col = 0;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = s.split('\n');
        if let Some(first) = parts.next() {
            self.insert_plain(first);
        }
        for p in parts {
            if self.multiline {
                self.newline();
            } else {
                self.insert_plain(" ");
            }
            self.insert_plain(p);
        }
    }

    fn insert_plain(&mut self, s: &str) {
        let line = &mut self.lines[self.row];
        let i = byte_idx(line, self.col);
        line.insert_str(i, s);
        self.col += s.chars().count();
    }

    fn newline(&mut self) {
        let line = &mut self.lines[self.row];
        let i = byte_idx(line, self.col);
        let rest = line.split_off(i);
        self.row += 1;
        self.lines.insert(self.row, rest);
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let i = byte_idx(line, self.col - 1);
            line.remove(i);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let line = &mut self.lines[self.row];
            let i = byte_idx(line, self.col);
            line.remove(i);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut c = self.col;
        while c > 0 && !chars[c - 1].is_alphanumeric() {
            c -= 1;
        }
        while c > 0 && chars[c - 1].is_alphanumeric() {
            c -= 1;
        }
        c
    }

    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut c = self.col;
        while c < chars.len() && !chars[c].is_alphanumeric() {
            c += 1;
        }
        while c < chars.len() && chars[c].is_alphanumeric() {
            c += 1;
        }
        c
    }

    fn delete_range(&mut self, from: usize, to: usize) {
        let line = &mut self.lines[self.row];
        let a = byte_idx(line, from);
        let b = byte_idx(line, to);
        line.replace_range(a..b, "");
        self.col = from;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_ = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter if ctrl || alt || super_ || !self.multiline => return Outcome::Submit,
            KeyCode::Char('s') if ctrl => return Outcome::Submit,
            KeyCode::Enter => self.newline(),
            KeyCode::Char('j') if ctrl && self.multiline => self.newline(),
            KeyCode::Backspace if ctrl || alt => {
                let to = self.col;
                let from = self.word_left();
                self.delete_range(from, to);
            }
            KeyCode::Char('w') if ctrl => {
                let to = self.col;
                let from = self.word_left();
                self.delete_range(from, to);
            }
            KeyCode::Backspace | KeyCode::Char('h') if key.code == KeyCode::Backspace || ctrl => {
                self.backspace();
            }
            KeyCode::Delete | KeyCode::Char('d') if key.code == KeyCode::Delete || ctrl => {
                self.delete();
            }
            KeyCode::Char('u') if ctrl => {
                let to = self.col;
                self.delete_range(0, to);
            }
            KeyCode::Char('k') if ctrl => {
                let from = self.col;
                let to = self.line_len();
                if from == to {
                    self.delete();
                } else {
                    self.delete_range(from, to);
                }
            }
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::Char('e') if ctrl => self.col = self.line_len(),
            KeyCode::Char('b') if alt => self.col = self.word_left(),
            KeyCode::Char('f') if alt => self.col = self.word_right(),
            KeyCode::Char(c) if !ctrl && !super_ => {
                let mut b = [0u8; 4];
                self.insert_plain(c.encode_utf8(&mut b));
            }
            KeyCode::Left if ctrl || alt => self.col = self.word_left(),
            KeyCode::Right if ctrl || alt => self.col = self.word_right(),
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.line_len();
                }
            }
            KeyCode::Right => {
                if self.col < self.line_len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.multiline && self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(self.line_len());
            }
            KeyCode::Down if self.multiline && self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(self.line_len());
            }
            KeyCode::Home if ctrl => {
                self.row = 0;
                self.col = 0;
            }
            KeyCode::End if ctrl => {
                self.row = self.lines.len() - 1;
                self.col = self.line_len();
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(),
            _ => return Outcome::Ignored,
        }
        Outcome::Handled
    }

    /// Move the cursor to a click at `(x, y)` relative to the widget.
    pub fn click(&mut self, x: u16, y: u16) {
        let row = (self.scroll_row + usize::from(y)).min(self.lines.len() - 1);
        self.row = row;
        let target = self.scroll_col + usize::from(x);
        let mut w = 0;
        let mut col = 0;
        for c in self.lines[row].chars() {
            let cw = char_width(self.display(c), w, 4);
            if w + cw > target {
                break;
            }
            w += cw;
            col += 1;
        }
        self.col = col;
    }

    fn display(&self, c: char) -> char {
        if self.masked { '•' } else { c }
    }

    /// Draw the text and return where the terminal cursor should go.
    pub fn render(&mut self, buf: &mut Buffer, area: Rect, style: Style, placeholder_style: Style) -> Position {
        // The area can be off screen when the window is very small.
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return Position::new(area.x, area.y);
        }
        let h = usize::from(area.height);
        let w = usize::from(area.width);
        if self.row < self.scroll_row {
            self.scroll_row = self.row;
        }
        if self.row >= self.scroll_row + h {
            self.scroll_row = self.row + 1 - h;
        }
        let cursor_x = {
            let mut x = 0;
            for c in self.lines[self.row].chars().take(self.col) {
                x += char_width(self.display(c), x, 4);
            }
            x
        };
        if cursor_x < self.scroll_col {
            self.scroll_col = cursor_x;
        }
        if cursor_x >= self.scroll_col + w {
            self.scroll_col = cursor_x + 1 - w;
        }
        buf.set_style(area, style);
        if self.lines.len() == 1 && self.lines[0].is_empty() && !self.placeholder.is_empty() {
            buf.set_stringn(area.x, area.y, &self.placeholder, w, placeholder_style);
        }
        for (i, line) in self.lines.iter().skip(self.scroll_row).take(h).enumerate() {
            let y = area.y + i as u16;
            let mut col = 0;
            for c in line.chars() {
                let shown = self.display(c);
                let cw = char_width(shown, col, 4);
                if col >= self.scroll_col && col + cw <= self.scroll_col + w {
                    let x = area.x + (col - self.scroll_col) as u16;
                    if shown == '\t' {
                        for k in 0..cw {
                            if let Some(cell) = buf.cell_mut((x + k as u16, y)) {
                                cell.set_char(' ');
                            }
                        }
                    } else if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_char(shown);
                    }
                }
                col += cw;
            }
        }
        Position::new(
            area.x + (cursor_x - self.scroll_col) as u16,
            area.y + (self.row - self.scroll_row) as u16,
        )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn editing() {
        let mut t = TextArea::new(true);
        for c in "hello".chars() {
            t.handle_key(key(KeyCode::Char(c)));
        }
        t.handle_key(key(KeyCode::Enter));
        t.insert_str("wörld");
        assert_eq!(t.text(), "hello\nwörld");
        t.handle_key(key(KeyCode::Home));
        t.handle_key(key(KeyCode::Backspace));
        assert_eq!(t.text(), "hellowörld");
        t.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "wörld");
        assert_eq!(t.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)), Outcome::Submit);
    }

    #[test]
    fn single_line() {
        let mut t = TextArea::new(false);
        t.insert_str("a\nb");
        assert_eq!(t.text(), "a b");
        assert_eq!(t.handle_key(key(KeyCode::Enter)), Outcome::Submit);
    }

    #[test]
    fn click_in_masked_text() {
        // Every char is shown as one dot, whatever its real width.
        let mut t = TextArea::with_text(false, "\t日x");
        t.masked = true;
        t.click(2, 0);
        assert_eq!(t.col, 2);
    }

    /// A simpler text area for comparison: the text is one list of chars,
    /// with newlines in it, and the cursor is an index into it.
    #[derive(Debug)]
    struct Model {
        text: Vec<char>,
        cur: usize,
        multiline: bool,
    }

    impl Model {
        fn line_start(&self, at: usize) -> usize {
            self.text[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1)
        }

        fn line_end(&self, at: usize) -> usize {
            self.text[at..].iter().position(|&c| c == '\n').map_or(self.text.len(), |i| at + i)
        }

        fn word_left(&self) -> usize {
            let start = self.line_start(self.cur);
            let mut c = self.cur;
            while c > start && !self.text[c - 1].is_alphanumeric() {
                c -= 1;
            }
            while c > start && self.text[c - 1].is_alphanumeric() {
                c -= 1;
            }
            c
        }

        fn word_right(&self) -> usize {
            let end = self.line_end(self.cur);
            let mut c = self.cur;
            while c < end && !self.text[c].is_alphanumeric() {
                c += 1;
            }
            while c < end && self.text[c].is_alphanumeric() {
                c += 1;
            }
            c
        }

        fn insert(&mut self, s: &str) {
            let s = s.replace("\r\n", "\n").replace('\r', "\n");
            let s = if self.multiline { s } else { s.replace('\n', " ") };
            for c in s.chars() {
                self.text.insert(self.cur, c);
                self.cur += 1;
            }
        }

        /// Move to the same column on the line that starts at `start`.
        fn move_to_line(&mut self, start: usize) {
            let col = self.cur - self.line_start(self.cur);
            self.cur = start + col.min(self.line_end(start) - start);
        }

        fn key(&mut self, key: KeyEvent) {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let (start, end) = (self.line_start(self.cur), self.line_end(self.cur));
            match key.code {
                KeyCode::Enter if !ctrl && !alt && self.multiline => self.insert("\n"),
                KeyCode::Char('a') if ctrl => self.cur = start,
                KeyCode::Char('b') if alt => self.cur = self.word_left(),
                KeyCode::Char(c) if !ctrl => self.insert(c.encode_utf8(&mut [0; 4])),
                KeyCode::Backspace if ctrl || alt => {
                    let from = self.word_left();
                    self.text.drain(from..self.cur);
                    self.cur = from;
                }
                KeyCode::Backspace if self.cur > 0 => {
                    self.cur -= 1;
                    self.text.remove(self.cur);
                }
                KeyCode::Delete if self.cur < self.text.len() => {
                    self.text.remove(self.cur);
                }
                KeyCode::Char('u') if ctrl => {
                    self.text.drain(start..self.cur);
                    self.cur = start;
                }
                KeyCode::Char('k') if ctrl => {
                    if self.cur == end {
                        if self.cur < self.text.len() {
                            self.text.remove(self.cur);
                        }
                    } else {
                        self.text.drain(self.cur..end);
                    }
                }
                KeyCode::Left if ctrl || alt => self.cur = self.word_left(),
                KeyCode::Right if ctrl || alt => self.cur = self.word_right(),
                KeyCode::Left => self.cur = self.cur.saturating_sub(1),
                KeyCode::Right => self.cur = (self.cur + 1).min(self.text.len()),
                KeyCode::Up if start > 0 => {
                    let prev = self.line_start(start - 1);
                    self.move_to_line(prev);
                }
                KeyCode::Down if end < self.text.len() => self.move_to_line(end + 1),
                KeyCode::Home if ctrl => self.cur = 0,
                KeyCode::End if ctrl => self.cur = self.text.len(),
                KeyCode::Home => self.cur = start,
                KeyCode::End => self.cur = end,
                _ => {}
            }
        }

        fn row_col(&self) -> (usize, usize) {
            let row = self.text[..self.cur].iter().filter(|&&c| c == '\n').count();
            (row, self.cur - self.line_start(self.cur))
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Key(KeyEvent),
        Paste(String),
    }

    /// Text with wide chars, tabs and word boundaries.
    const CHARS: &str = "[ab1_ .é日\t]";

    fn op() -> impl Strategy<Value = Op> {
        let code = prop_oneof![
            4 => CHARS.prop_map(|s| KeyCode::Char(s.chars().next().unwrap_or('a'))),
            1 => prop::sample::select(vec![
                KeyCode::Char('u'),
                KeyCode::Char('k'),
                KeyCode::Enter,
                KeyCode::Backspace,
                KeyCode::Delete,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Home,
                KeyCode::End,
            ]),
        ];
        let mods = prop::sample::select(vec![KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT]);
        prop_oneof![
            8 => (code, mods).prop_map(|(c, m)| Op::Key(KeyEvent::new(c, m))),
            1 => "([ab1_ .é日\t]|\r|\n){0,6}".prop_map(Op::Paste),
        ]
    }

    proptest! {
        #[test]
        fn edits_like_a_flat_string(
            multiline in any::<bool>(),
            masked in any::<bool>(),
            ops in prop::collection::vec(op(), 0..60),
            (w, h) in (1u16..12, 1u16..5),
        ) {
            let mut t = TextArea::new(multiline);
            t.masked = masked;
            let mut m = Model { text: Vec::new(), cur: 0, multiline };
            let area = Rect::new(2, 1, w, h);
            let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
            for op in ops {
                match &op {
                    Op::Key(k) => {
                        // Keys that submit or cancel don't change anything.
                        if !matches!(t.handle_key(*k), Outcome::Submit | Outcome::Cancel) {
                            m.key(*k);
                        }
                    }
                    Op::Paste(s) => {
                        t.insert_str(s);
                        m.insert(s);
                    }
                }
                prop_assert_eq!(t.text(), m.text.iter().collect::<String>(), "after {:?}", op);
                prop_assert_eq!((t.row, t.col), m.row_col(), "after {:?}", op);

                // The cursor is drawn inside the area, and clicking there
                // puts it where it was drawn.
                let at = t.render(&mut buf, area, Style::default(), Style::default());
                prop_assert!(area.contains(at), "cursor {:?} outside {:?}", at, area);
                t.click(at.x - area.x, at.y - area.y);
                prop_assert_eq!(t.render(&mut buf, area, Style::default(), Style::default()), at);
                prop_assert_eq!((t.row, t.col), m.row_col(), "after clicking at the cursor");
            }
        }
    }
}
