# Diffs and the detail pane

The detail pane on the right shows either a commit, with its message and
full diff, or the working directory, with its staged, unstaged,
untracked and conflicted files. This document explains where the diffs
come from, how the pane turns them into rows, and how word marks and
syntax colors are added.

The diff model and the parser for `git diff` output are in
`src/git/diff.rs`. Commit diffs are in `src/git/commit.rs`, word marks in
`src/git/words.rs`, and syntax highlighting in `src/highlight/`. The
rows are built in `src/app/doc.rs` and drawn by `draw_doc` in
`src/ui/draw.rs`. How the working directory is loaded is in
`staging.md`.

## One model for both kinds of diff

Commit diffs and working directory diffs end up in the same types:

- A `FileDiff` is one file. It has the old and new path, either of which
  is `None` when the file was added or deleted. It also has the status,
  the modes and blob ids, and flags for binary files and diffs that were
  too large to show.
- A `Hunk` has the line ranges from its header, the section name git
  prints after the second `@@`, and its lines.
- A `DiffLine` is context, added or removed. It has its line numbers on
  each side, and a flag for a last line without a newline.

A `DiffLine` holds its text for display, with bad UTF-8 replaced. If the
line wasn't valid UTF-8, it also keeps the original bytes in `raw`. Line
staging needs those to build a patch that matches the file.

## Commit diffs come from gitoxide

`commit::load` diffs a commit's tree against its first parent's tree,
or against the empty tree for a root commit. It uses gitoxide for the
tree diff, which can report renames and copies, and imara-diff for the
text. We do this in process rather than running `git show`, because
moving through the history loads a commit for every row the user
passes, and that has to be fast.

Renames follow `diff.renames` and `diff.renameLimit`, which gix reads
from the config the way git does. One setting differs. Gix doesn't pair
up empty files by default, but git does, so `rewrites` turns that on.
Git and gix measure how similar two files are in different ways, so they
sometimes disagree about a rename between files that both changed.

`diff_texts` splits both sides into lines, runs imara-diff's histogram
algorithm with its line post-processing, and groups the changes into
hunks with `context` lines around them. Two changes share a hunk when
the gap between them is at most twice the context, which is what git
does. Hunk headers and line numbers follow the same rules as in
`git diff`. The lines that count as changed can still differ from git's.
Git uses Myers by default, and even git's histogram algorithm sometimes
picks other lines than imara-diff's. For example, git turns `e c é`
into `é e é` by replacing the first two lines, while imara-diff adds one
line and removes another. A property test, `hunks_turn_old_into_new`,
checks that applying the hunks to the old text gives the new text.

Some files aren't diffed:

- A blob with a NUL byte in its first 8000 bytes is binary.
- A blob over 4 MiB (`MAX_BLOB`), or a diff over 20,000 lines
  (`DEFAULT_LINE_LIMIT`), is marked too large. The user can ask for it
  anyway with Enter on the notice, which loads the commit again without
  limits.
- A submodule shows up as `Subproject commit <id>`, as git shows it.

A stash made with `--include-untracked` keeps its untracked files in a
third parent, a root commit with only those files. Diffing against the
first parent would leave them out. So `stash_untracked` recognizes such
a stash by its parents and the third parent's message, and `load` adds
that commit's files as untracked, with status `?`.

Files are sorted by path. The full text of both sides is kept in
`FileTexts`, for syntax highlighting.

### Loading commits one at a time

Moving quickly through the history asks for a lot of commits, and only
the last one matters. `request_commit` sends each request over a Tokio
watch channel, which only keeps the newest value. `detail_worker` loads
one commit at a time, and whatever was asked for while it worked is
replaced by the newest request. It keeps one thread-local gitoxide
repository across loads, so the object cache carries over from one
commit to the next.

Each request has a number. When a result comes back, the app checks the
number and the commit id against what the pane shows now, and drops the
result if either differs.

## Working directory diffs come from git

The staged and unstaged diffs come from `git diff`, and `parse_unified`
reads its output. The parser works on bytes, because git prints file
contents and file names as they are. A `FileDiff` keeps its paths as
bytes too (see `running-git.md`).

The parser relies on the line counts in each hunk header to know where
a hunk ends. A file can contain a line that starts with `diff --git`, so
guessing from the text would split the file in the wrong place. When a
line inside a hunk doesn't start with `+`, `-`, a space or `\`, the diff
is malformed. The parser then ends the hunk early and skips that line.

Combined diffs, which git prints for conflicted files, have one column
per parent. A removed `- item` then reads `--- item` and would look like
a file header. The parser skips everything in a combined diff until the
next `diff --git`.

Paths in `diff --git a/x b/y` lines are ambiguous when they contain
spaces and aren't quoted. `split_git_paths` assumes both sides have the
same length when the file isn't renamed, which is what git does too.
The `---`, `+++` and rename lines then fill in the real paths. Quoted
paths are unquoted with git's C-style rules.

## Rows

The pane can show thousands of lines, and it has to scroll and redraw
quickly. So `Detail::rebuild` flattens everything into a list of `Row`s
once, and drawing only looks at the rows on screen. A row refers to its
file, hunk and line by index. It doesn't copy any text.

The kinds of row are:

- `Meta`: a line of the commit header, such as the subject, author or
  parents. Some of them can be clicked, for example to copy the hash or
  jump to a parent.
- `Section`: a heading such as "Staged changes".
- `File`: a file header.
- `Notice`: a line in place of a diff, such as "Binary file".
- `Hunk` and `Line`: a hunk header and one diff line.
- `Split`: one row of the side-by-side view, with a line on each side.
- `Blank` and `Empty`: spacing, and placeholders such as "Loading…".

In the side-by-side view, `push_split_rows` pairs each run of removed
lines with the run of added lines after it, in order. Context lines
appear on both sides.

### Folding

Files can be folded with `z`. Folds are kept by section and path in
`Detail::collapsed`, so they survive a reload. In the working directory
that's what you want, because the same files stay around. In commits it
isn't, because a file folded in one commit would stay folded in every
other commit that touches it. So `set_source` forgets the folds for
commits whenever the pane switches to another commit.

The first time a file is seen, it starts folded if its diff has more
than 400 lines, or if a commit changes more than 40 files.

### Keeping the cursor in place

After a reload, the files may be in a different order, and the indexes
in the rows may point somewhere else. Before it rebuilds, `rebuild`
works out a `RowKey` for the cursor's row, which names the file by path.
After the rebuild it looks for the row with the same key. If that line
is gone, it tries the same hunk, then the hunk before it, then the file.
So staging a hunk leaves the cursor near where it was, and not at the
top of the list.

The same lookup keeps the cursor in place when the user switches
between the unified and the side-by-side view. A row of the side-by-side
view can show a removed and an added line, and it matches the key of
either one.

The other end of a line selection, `Detail::anchor`, is a row index too.
`rebuild` finds its row again by key, the same way. If that row is gone,
the selection is dropped, because keeping the index would select other
lines than the ones the user picked.

## Word marks

Inside a changed line, the words that changed get a stronger background.
`hunk_marks` pairs each run of removed lines with the added lines that
follow, in order, the same way the split view does. For each pair,
`line_marks` splits both lines into words, spaces and single punctuation
characters, and diffs those with Myers. The words on either side that
don't match are marked.

When more than 60% of the line changed, the marks would only be noise,
so there are none. Lines over 2000 bytes are skipped too.

Marks are computed when the rows are built, and kept per file until the
files change.

## Syntax highlighting

Highlighting a large file takes too long to do while drawing. So it runs
in the background, in a worker that `Highlighter::new` returns.

Each language has its own lexer, written with Logos and kept in
`src/highlight/`, one file per language: `c.rs` for C and C++,
`rust.rs` for Rust, `racket.rs` for Racket and Scribble,
`shrubbery.rs` for Rhombus, and `toml.rs` for TOML. A lexer turns text into `Tokens`: byte
ranges, each with a `Kind` such as `Comment`, `Keyword` or `Escape`. Plain text, such as
most identifiers, has no token. The theme maps kinds to colors when the
line is drawn, so switching themes doesn't need to lex anything again.

Colors come from lexing alone. There is no parser, so the lexer can't
tell what an identifier names, and we don't guess from its spelling.
Identifiers are plain unless they're keywords. Only the `#[` that starts
an attribute is colored, because finding where an attribute ends would
take a parser. Escapes inside strings and characters are found with a
second, smaller lexer.

The Racket lexer follows Racket's reader with the default readtable. It
was written from the reader's source in `racket/src/expander/read`
rather than the reference manual, which leaves out some details. For
example, a `;` comment only ends at a linefeed. Symbols are plain, even
`define` and `lambda`, because only the expander knows what they mean.
Keywords such as `#:when` get the keyword color. A `#;` comment covers
the datum after it, so the lexer counts parentheses to find where that
datum ends.

Whether an atom such as `1/2`, `+i` or `1e5x` is a number or a symbol
depends on Racket's long number syntax. `is_number` in `racket.rs`
follows `string->number` from the reader's `number.rkt`, with about one
state for each function there, but it only checks the syntax.

A `#lang` line at the top of a Racket file chooses the reader for the
rest. With `#lang at-exp`, the lexer also reads @-forms such as
`@foo[1]{text}`. With a Scribble language such as `#lang
scribble/manual`, the whole file is text with @-forms in it. A `.scrbl`
file is Scribble text even without a `#lang` line, because a hunk from
the middle of a document doesn't have one. A hunk from the middle of an
`at-exp` file is lexed as plain Racket, but we almost always have the
full text, which starts with the `#lang` line.

The @-reader is `scribble/reader.rkt` in the `at-exp-lib` package, and
the lexer follows its source. An @-form mixes Racket data and text,
and each can hold the other, so the lexer keeps a stack of `Frame`s for
what it's inside of. The `@` and a command that is a name, as in
`@title`, get the keyword color, and the brackets and braces around the
parts get the punctuation color. Text is plain. An `@;` comment gets the
comment color, including its text in braces. The reader has a few
surprises that the lexer copies:

- The command is read with a readtable where `|` ends a symbol, so
  `@foo|{text}|` has text after `foo`. That readtable only applies to
  the outermost datum of the command. But escapes such as `@|x|` use it
  for everything inside them. In `#lang at-exp`, so do data in brackets
  inside an escape. The Scribble languages read those with the usual
  readtable.
- The data in `@foo[...]` and `@|...|` are read with
  `read-syntax/recursive`, which returns comments instead of skipping
  them. So a `#;` there takes the next comment as its datum, as in
  `@foo[#; ; c` followed by a line with `1`, where `1` is not commented
  out. Inside parentheses, or after a prefix such as `'`, a `#;` skips
  comments as usual.
- Outside of text, `@|{x}|` is text in other delimiters. Inside text,
  it's an escape holding the list `{x}`.

A `#;` or `@;{` comment can cover several lines. Its color leaves out
the whitespace at the end of each line. Whether the comment covers that
whitespace can depend on the next line, as with a `#;` at the end of a
line and a `)` on the next one, which ends the comment early. Colors
must not depend on what comes after a line, or a hunk lexed on its own
would get different colors.

### Rhombus

Rhombus and its relatives, such as Shplait, are written in shrubbery
notation. `shrubbery.rs` follows the lexer that DrRacket colors them
with, in `lex.rkt` from the `shrubbery-lib` package. A `#lang` line
for one of these languages makes the Racket lexer hand the rest of the
file to it, so the language decides and not the file's name. A `.rhm`
file without a `#lang` line is shrubbery, because it's a piece from the
middle of a file. The Rhombus Scribble languages, such as
`rhombus/scribble/manual`, read the whole file as text with @-forms in
it. Their files are `.scrbl` files like Racket's Scribble documents.
Without a `#lang` line, we read a `.scrbl` file as Racket Scribble.

Identifiers are plain, like Racket's symbols, and so are `fun` and
`def`, which are bound like any other name. Keywords such as `~into`
get the keyword color, and so do the `@` of an @-form and the names in
its command, as in `@rhombus` or `@foo.bar`. The operators get the
operator color. `:`, `|`, `\`, `,` and `;` mean something to the parser,
so they get the punctuation color, like brackets and `'` quotes.

Some rules of the lexer are surprising:

- After a term, a `+`, `-` or `.` doesn't start a number, so `x-1` is
  `x`, `-` and `1`. After a string it does.
- An operator can't end with `/` or `:`, unless it's all colons. But a
  `/` right after an operator joins it, unless a `/` or `*` follows,
  so `+/` is one operator and `+:` is two tokens.
- A string can't hold a line break. A string that isn't finished, or
  has a bad escape, is an error that runs to its closing quote or the
  end of the line. We color it as a string anyway. An unfinished `/*`
  comment gets the comment color, for the same reason as in Racket.
- An identifier can contain emoji, but a single emoji character like
  `©` that isn't shown as an emoji by default is an operator. The
  lexer has a list of emoji sequences from an older version of the
  Unicode standard. We use Unicode's emoji properties instead. They
  mostly agree, but can differ on newer emoji and unusual sequences.
- `#{...}` holds Racket data, which the Racket lexer reads up to the
  `}`. In `~#{...}`, only a symbol right after the brace gets a color.

A `#//` comments out the group after it, which can go on over several
lines. Finding where that group ends takes the parser, so DrRacket
guesses from the indentation and the brackets, in `lex-comment.rkt`.
`Groups` does the same. That code keeps its own count of lines and
columns, and it doesn't count lines inside `#{...}` or a tab inside a
token. We copy that, so that the colors are the same as DrRacket's.
The guess only depends on what came before a token, so a line's colors
never depend on the lines after it.

DrRacket's full shrubbery lexer adds `lex-open-close.rkt` on top. That
layer reports the invisible parentheses that indentation makes, and it
doesn't change any colors, so we leave it out.

### C and C++

`c.rs` lexes both C and C++. They differ in their keywords, and only
C++ has raw strings. Files ending in `.c` are C. Headers ending in `.h`
are read as C++, as Sublime Text does. A C++ header read as C would
lose the colors of `class`, `template` and `namespace`. A C header read
as C++ only colors the odd name, such as a variable called `new`.

The lexer follows the translation phases of the C and C++ standards.
A backslash at the end of a line joins it to the next, before anything
else is read. So a `//` comment or a directive goes on to the next line
when its line ends in a backslash. C++23 and the compilers allow spaces
between the backslash and the line break, and so do we. Because
splices come before escapes, a string with `\\` at the end of a line
goes on to the next line too.

A `#` at the start of a line begins a directive, even after a comment.
The lexer keeps track of which directive a line is in, because some
tokens read differently there:

- After `#include`, `<stdio.h>` is a header name and gets the string
  color. Backslashes in `"..."` are part of the path, not escapes.
- The message of `#error` and `#warning` is text, which may hold a
  lone quote, so all of it gets the string color.
- In `#if` and `#elif`, `defined` is a keyword.

The code between `#if 0` and its `#else`, `#elif` or `#endif` gets the
comment color, as it does in most editors. Only a condition that is
just `0` counts. The preprocessor still splits skipped code into
tokens, so the lexer reads it as usual, and a `/*` in it can hide the
`#endif`. It counts the conditionals inside to find the one that
matches. The lexer colors the skipped lines once it finds the end, but
which lines those are only depends on the lines before them, so a hunk
lexed on its own gets the same colors.

A number is a preprocessing number, as the standard defines it. It
takes in letters, dots, digit separators, and signs after `e` or `p`,
so `0x1e+1` and `12_km` are each one number, as they are for the
compiler. A raw string such as `R"x(...)x"` ends at the first `)x"`.
Its delimiter can't hold spaces, parentheses or backslashes, and we
also leave out quotes. In C, which has no raw strings, the `R` is a
name and the quote starts an ordinary string. Without quotes in the
delimiter, that string can't end inside the token that Logos matched.

### TOML

`toml.rs` follows TOML 1.1, which adds the `\e` and `\xHH` escapes and
allows line breaks, comments and a final comma inside inline tables.
Files ending in `.toml` are TOML, and so are `Cargo.lock`, `Pipfile`,
`poetry.lock` and `uv.lock`.

A word can be a key or a value depending on where it is. `true = 1`,
`123 = 1` and `2024-01-01 = 1` all have bare keys. So the lexer keeps a
stack of the arrays and inline tables it's inside of, and it remembers
whether a key or a value comes next. A key comes first on a line
outside of brackets, and after `{` or a comma in an inline table. A
value comes after `=`, and after `[` or a comma in an array. A line
break outside of brackets starts a new key. Keys are plain, except that
quoted keys get the string color and the dots between parts get the
punctuation color.

A `[` where a key would start, outside of brackets, starts a table
header. The whole header, such as `[server]` or `[[bin]]`, gets the
attribute color, so that the sections of a file stand out. A `]` that
closes nothing is punctuation and changes nothing. That's what a hunk
from the middle of a multi-line array has, and the header after it
still reads as a header.

Basic and literal strings can't go on to the next line, so one without
its closing quote ends with its line. Multi-line strings end at three
quotes, but up to two more quotes just before them belong to the
string, so `'''a'''''` is one string. Numbers, dates and times, `inf`
and `nan` get the number color, and `true` and `false` get the
constant color. A date and a time can be separated by a space, so the
lexer reads `1979-05-27 07:32:00` as one token. Anything else that
isn't a space or a bracket is one plain word, so that `xtrue` isn't
partly colored.

The worker only highlights files that appear on screen.
`request_visible_highlights` runs while drawing and asks for every file
with a row on screen that hasn't been asked for yet. Each job carries the
file's hunks and, when we have it, the full text of each side.

With the full text, the worker lexes the whole file, up to the last line
the diff shows. That way a line in the middle of a multi-line string or
comment gets the right colors. Then it splits the tokens up by line and
picks out the lines the diff shows by their line numbers. Lexing is fast
enough that files of any size we load are lexed whole. Without the full
text, it lexes each hunk's old side and new side on its own. It joins
the side's lines with newlines, leaving one off only where the file has
none, so a hunk at the top of a file gets the same colors either way.
Context lines use the new side's colors when it has them.

We have the full text of both sides of every diff. A commit diff has
them from gitoxide. For an unstaged file, the old side is in the index
and the new side is on disk. For a symlink, the text on disk is the path
it points to, which is what git stores. For a staged file, the old side
is in HEAD and the new side is in the index. `Workdir::load` reads the
index and HEAD versions with `read_blobs`, which runs `git cat-file`
twice: once with `--batch-check` to skip anything over `MAX_BLOB`, and
once with `--batch` to read the rest. It asks for blobs by the ids in
the diff's `index` lines, which `--full-index` makes complete. Asking by
path wouldn't work for every name. `git cat-file` reads one name per
line, and a path like `1:x` names a stage of the index.

The language is chosen by the file's extension, in
`Language::for_path`. Files in other languages aren't highlighted.

### Dropping stale results

Highlights are keyed by section and file index, and those indexes mean
something else after a reload. So `Detail` has a `Generation` that the
highlighter shares. It moves on whenever the files change. Jobs from an older generation are skipped before the worker
starts them, and results from one are dropped when they arrive.

## Drawing a line

`draw_code` draws a diff line one grapheme at a time through
`text::draw` (see `text-width.md`). It takes the line's background from
whether it's added, removed or context, the foreground from the syntax
colors, and the stronger background from the word marks. A trailing
carriage return is left out, so CRLF files don't show a stray symbol.
Selected lines get the selection background instead.

## Tests

- `src/git/diff.rs` has examples for the parser, including content that
  looks like a header, combined diffs, the line limit and quoted paths.
  Property tests check that parsing never panics on diff-like input,
  that quoting round-trips, and that `diff --git` paths split correctly.
- `src/git/commit.rs` checks that `diff_texts` matches git's output
  style, and has the property test `hunks_turn_old_into_new`.
- `tests/commit_props.rs` builds random pairs of trees and stashes, and
  checks what `commit::load` gives against `git diff-tree`. It checks
  the files, renames, modes, binary flags and texts, and that the hunks
  turn the old text into the new text.
- `tests/ops.rs` has `a_stash_shows_its_untracked_files` and
  `an_empty_file_can_be_renamed`.
- `tests/staging.rs` checks the texts that `Workdir::load` reads for
  staged and unstaged files.
- `src/git/words.rs` has property tests that the words cover the whole
  line, and that the unmarked text on both sides is the same.
- `src/app/doc.rs` has property tests that build the rows from random
  commits and working directories, in both views. They check that each
  file shows its notice or its hunks, that every line shows up once and
  in order, and that selecting a whole hunk selects exactly its changed
  lines. They also check that the cursor and the line selection stay on
  their rows when the pane is rebuilt, when the view changes, and when
  files change, go away or appear.
- `tests/ui.rs` has `folds_do_not_carry_over_to_other_commits`.
- `src/highlight/rust.rs` has examples for each kind of Rust token,
  such as nested comments, raw strings, escapes, and characters next to
  lifetimes.
- `src/highlight/racket.rs` has examples for each kind of Racket token,
  such as `#;` comments, here strings, characters, and atoms that are
  numbers and atoms that aren't. It also has examples of @-forms in
  Scribble and `#lang at-exp`, with other delimiters, escapes and
  comments. The dev image has no Racket, so nothing runs the real
  reader automatically. When the lexer was written, we checked
  `is_number` against Racket's reader on about 240,000 random atoms.
  We also checked the whole lexer against `read-syntax` on about
  120,000 random texts that Racket reads without an error, and they
  agreed on all of them.
- When the lexer learned @-forms, we checked it against a copy of the
  @-reader that marked where each piece of text came from. On about
  200,000 random texts that Racket reads without an error, in plain
  Racket, `#lang at-exp` and Scribble, every number, string, character,
  boolean and keyword got its color, symbols and text got none, and
  comments covered nothing that the reader read. We checked the 1,300
  Scribble and `at-exp` files in Racket's own source the same way. The
  only differences were here strings, where Racket counts the linefeed
  after the terminator and the lexer doesn't, which changes no colors.
- `src/highlight/shrubbery.rs` has examples for each kind of shrubbery
  token, such as numbers and operators after a term, group comments,
  quotes, Racket data, @-forms and Rhombus Scribble text. Like the
  Racket lexer, nothing runs the real one automatically. When we wrote
  it, we checked each character's color against DrRacket's lexer on
  about 90,000 random texts and on the 1,091 Rhombus files in Racket's
  source, and they agreed on all of them. We left out about 1,000
  random texts that made the real lexer fail, for example on a `²`
  followed by a letter, which no rule of its lexer matches.
- `src/highlight/c.rs` has examples for each kind of C and C++ token,
  such as line splices, raw strings, digit separators, header names
  and `#if 0`. The dev image has no C compiler, so nothing compares it
  with one automatically. When we wrote it, we compared it with clang's
  raw lexer (`clang -cc1 -dump-raw-tokens`) on about 3,000 random
  texts, each read as C23 and as C++23. We checked whether each
  character was in a comment, a string, a number or none of these.
  They only differed on splices inside a token, on raw string
  delimiters that hold a quote, on how clang recovers from errors such
  as a bad delimiter, and on numbers like `1p-1`. In C++, clang only
  reads that as one number when it's hexadecimal, but the standard
  says it's always one.
- `src/highlight/toml.rs` has examples for each kind of TOML token. Its
  property test builds random valid documents from tricky pieces, such
  as keys that look like values and strings that hold brackets. It
  parses them with `toml_edit`, which already reads the config file,
  and checks that each value has the color of its type and that each
  key is plain, or a string if it's quoted.
- `src/highlight/mod.rs` has property tests that lexers never panic and
  give tokens in order on character boundaries, and that every hunk line
  gets the same colors with or without the full text, where it should.

## Known gaps

- A merge commit is diffed against its first parent only.
- Only C, C++, Rust, Racket, Rhombus, Scribble and TOML files are
  highlighted.
- A hunk from the middle of a multi-line TOML array that we don't have
  the full text for reads its values as keys. Strings keep their
  color, but numbers, `true` and `false` are plain. A
  nested array that starts a line there, as in `[1, 2],`, gets the
  color of a table header.
- In C and C++, a splice in the middle of a token, as in `L\` at the
  end of a line followed by `'x'`, splits the token in two. The
  compiler joins them.
- C and C++ read `<:` and `:>` as `[` and `]`. We read them as two
  tokens, which only changes the color of the `<` or `>`.
- A hunk from the middle of a Rhombus Scribble document that we don't
  have the full text for is lexed as Racket Scribble.
- Other Racket languages with a reader of their own, such as
  `#lang datalog`, are lexed with the default reader. So is
  `#reader scribble/reader`.
- A hunk from the middle of a `#lang at-exp` file that we don't have the
  full text for is lexed as plain Racket.
- Word marks pair removed and added lines in order. When lines were
  inserted between edited ones, a line can be paired with the wrong
  partner, and then gets no marks or odd ones.
