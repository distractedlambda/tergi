# Text width

tergi has to know how many terminal columns each piece of text takes up.
It needs that to line up the history columns, to cut long names short, to
place the cursor in a text box and to work out which character a click
hit. If tergi's count differs from the terminal's, the rest of the line
moves over, or old characters are left on screen.

This document explains the rules tergi uses and why. All of the code is
in `src/ui/text.rs`. Anything that measures text, or decides where it
goes on screen, should use the functions there.

## Which terminal we assume

We assume a terminal that measures text one grapheme at a time, the way
Ghostty does by default (its `grapheme-width-method` setting is
`unicode`). A grapheme is what a reader sees as one character. It can be
several code points, such as a letter with a combining accent, or a
family emoji made of three people joined by zero-width joiners.

tmux 3.5 measures the common emoji sequences the same way. That is the
tmux in the dev image, so it works for driving tergi in tests and by
hand.

Terminals that measure each code point on its own will misalign rows
that contain emoji sequences. We don't try to support them.

## The rules

`cells(text, tab)` splits text into graphemes with unicode-segmentation
and gives each one a width:

- A tab reaches the next multiple of `tab` columns. File contents use the
  configured tab width. Labels, names and messages use 4.
- A grapheme with a control character in it is drawn as one `�`.
- A grapheme that is only a format character, such as a bidi override or
  a zero-width space, is left out. It never reaches the terminal, so it
  can't reorder or hide the text after it. A combining accent with no
  letter before it, at the start of a line, is left out too.
- Any other grapheme is as wide as ratatui's `cell_width` says, but never
  more than 2.

Some examples:

| Text | Graphemes | Columns |
|---|---|---|
| `e` + U+0301 (combining accent) | 1 | 1 |
| `日本` | 2 | 4 |
| `❤` + U+FE0F (emoji presentation) | 1 | 2 |
| `👍🏽` (thumb and skin tone) | 1 | 2 |
| `👨‍👩‍👧` (three people and two joiners) | 1 | 2 |
| `🇺🇸` (two regional indicators) | 1 | 2 |
| `a` + U+202E (bidi override) | 2 | 1 |

`cell_width` is unicode-width's width of the whole grapheme, plus a fix
for halfwidth katakana sound marks. We use ratatui's function rather than
calling unicode-width ourselves because ratatui's buffer diff uses it.
When the diff writes a wide character, it measures the symbol to decide
how many of the following cells the character covers, and skips them. If
we used a different width, the diff would skip a cell we drew or redraw
one that the wide character covers.

A terminal gives a grapheme one or two cells, never more. unicode-width
can say more for unusual clusters. A letter followed by a skin tone comes
out as 3, and three Hangul leading consonants in a row come out as 4. We
cap those at 2. When we do, `draw` sets `CellDiffOption::ForcedWidth` on
the cell, so the diff uses our width too.

## Measuring

`width(s, tab)` adds up the widths of the cells. `truncate(s, max)` cuts a
string at a grapheme boundary so it fits in `max` columns with an
ellipsis. Both measure tabs the way `put` draws them.

Layout code should measure with `width`, not `len()` or
`chars().count()`. The exceptions are labels that are always ASCII, such
as key names. The commit summary counter also counts characters on
purpose, because that's how the usual 50 and 72 character limits are
counted.

## Drawing

`draw` writes a line of text into the buffer. It puts each grapheme in the
cell where it starts and sets the cells it covers to an empty symbol. A
tab becomes one space per column. It can skip columns at the start, for
scrolling, and stop at a maximum width. A wide grapheme that only partly
fits at either edge is left out, and its columns stay blank.

Everything that draws text goes through `draw`:

- `put` draws a string in one style. Most of the UI uses it.
- `put_line` draws a ratatui `Line` span by span. The commit details use
  it.
- `draw_code` in `ui/draw.rs` draws a line of a diff, with a style for
  each grapheme from the syntax colors and word marks.
- `TextArea::render` draws commit messages and prompts.

Don't use ratatui's `set_stringn`, `set_line` or `Paragraph` for text that
comes from the repository or the user. They measure graphemes the same
way, but they drop tabs and control characters instead of drawing them.
Text measured with `width` would then come out the wrong size. Block
titles still go through ratatui. They are tergi's own words and ref
names, and git doesn't allow control characters in ref names.

If the terminal does disagree about a width, the damage stops at the next
cell. Ratatui's crossterm backend moves the cursor explicitly whenever a
cell doesn't follow straight on from the one it wrote before, and that
includes every cell after a wide character. The rest of the line lands
where tergi put it, and at worst one column next to the grapheme is
wrong.

## The text area

`TextArea` keeps its cursor as a byte offset into the line, and the
offset always sits at a grapheme boundary. Left and Right move over a
whole grapheme, and Backspace and Delete remove one. So the cursor never
stops between a letter and its accent, or in the middle of an emoji.
Up and Down keep the number of graphemes before the cursor.

An edit can join the text on both sides of the cursor into one grapheme.
For example, deleting a tab between a letter and a combining accent puts
the accent on the letter. The cursor then moves to the end of that
grapheme. Pasted text is inserted as a whole before it is split into
lines, so it joins with the text around it the same way however it is
broken up.

Masked text shows one dot for each grapheme. Clicks and the cursor count
the dots, not the real widths.

A grapheme that draws nothing, such as a lone bidi override, leaves two
cursor positions in the same column. Clicking that column puts the
cursor before the next visible grapheme.

## Tests

The unit tests in `ui/text.rs` include property tests over text full of
tabs, control and format characters, accents, wide characters and parts
of emoji sequences:

- `cells_cover_the_text` checks that the cells follow each other, that
  their widths add up, and that the text they skip over has no width.
- `drawing_fills_the_columns_we_count` draws text and then walks the
  buffer the way ratatui's diff does. It checks that the walk lands on the
  start of every cell we drew.
- `truncate_fits` checks that cut strings fit and keep the start of the
  original.

`edits_like_a_flat_string` in `ui/textarea.rs` compares the text area
against a simple model, using the same kinds of characters. `ui/draw.rs`
has examples for `draw_code`.

To check a change against a real terminal, make a repository whose commit
summaries contain emoji sequences, accents, CJK and tabs. Run tergi on it
in tmux, capture the pane, and check that every history row has the same
width between the borders.

## Known gaps

- Terminals that measure code points one at a time draw emoji sequences
  wider than we expect, so they cover the text that follows them.
- Unusual clusters that we cap at 2 may get another width in the
  terminal. tmux 3.5 gives a letter followed by a skin tone 3 columns, so
  it covers the cell after it.
- East Asian ambiguous characters count as one column. A terminal set to
  draw them two columns wide, as some CJK setups are, will misalign. Some
  of tergi's own symbols, such as `…`, `▸` and `─`, are ambiguous.
- unicode-width's tables and the terminal's can come from different
  Unicode versions, so very new emoji may be measured differently.
- `put_line` measures tabs from the start of each span, not the start of
  the line.
- `wrap` in `ui/draw.rs` breaks text at spaces only. A word longer than
  the line is cut off.
