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

### Comparing two commits

While several commits are selected in the history, the pane shows the
changes between the two ends of the selection, from the commit lower
down in the list to the one above it. The source is then
`Source::Compare`. `commit::compare` loads the upper commit with `base`
set to the lower one, and diffs their trees instead of diffing against
the parent. The two commits needn't be related, so this is how the
user compares branches. Sublime Merge does the same when two commits
are selected. The lower commit's own changes aren't in the diff, which
can surprise someone who selected a run of commits on one branch.

`App::show_selected` works out what the pane should show from the
history's selection. Everything that moves the selection or its anchor
goes through it. `on_history` calls it again when a walk ends while the
pane shows a comparison, because the ends may have gone or swapped
places.

### Loading commits one at a time

Moving quickly through the history asks for a lot of commits, and only
the last one matters. `request_commit` sends each request over a Tokio
watch channel, which only keeps the newest value. `detail_worker` loads
one commit at a time, and whatever was asked for while it worked is
replaced by the newest request. It keeps one thread-local gitoxide
repository across loads, so the object cache carries over from one
commit to the next.

Each request has a number. When a result comes back, the app checks the
number and the source against what the pane shows now, and drops the
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

The first time a file is seen, it starts folded if a commit changes
more than 40 files. Large diffs aren't folded, however long they are.

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

### Levels

The keyboard moves through the pane at one of three levels, which
`Depth` names. At the top, j and k stop on files, section headers, the
commit's subject and the lines of its details that do something on
Enter. One level in, they stop on hunks and on the notices of files
without hunks. At the bottom, they stop on the lines of one hunk and
don't leave it.

The level isn't stored anywhere. `Detail::depth` works it out from the
kind of row the cursor is on. So a click on any row puts the keyboard
at that row's level, and a rebuild that moves the cursor to a nearby
row keeps the level that row has. `level_target` finds where a move
ends up at the cursor's level, and `first_inside` and `parent_row`
find the rows that `l` and `h` go to.

Moving by hunks with j and k unfolds folded files on the way, so that
it can read a whole diff hunk by hunk. `App::step_hunk` does this. When
the search for the next hunk meets a folded file, it unfolds it and
rebuilds the rows. The rebuild keeps the cursor on its row, so the next
search starts from the same place and finds the hunks that just
appeared.

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
`cmake.rs` for CMake, `javascript.rs` for JavaScript, `json.rs` for
JSON, `rust.rs` for Rust, `racket.rs` for Racket and Scribble,
`shrubbery.rs` for Rhombus, `toml.rs` for TOML, `yaml.rs` for YAML,
`markdown.rs` for Markdown, and `gitignore.rs` for `.gitignore` files.
A lexer turns text into `Tokens`: byte ranges, each with a `Kind` such as `Comment`,
`Keyword` or `Escape`. Plain text has no token. An identifier that gets
no other color is an `Identifier` token, which gets a color from its
name unless the user turns off rainbow identifiers (see below). The theme maps kinds to
colors when the line is drawn, so switching themes doesn't need to lex
anything again.

Colors come from lexing alone. There is no parser, so the lexers can't
tell which names are functions, types, macros or constants. In Rust,
every name that isn't a keyword or part of an attribute's path is an
identifier, and gets a color from its name.

An attribute is colored from `#[` up to the end of its path, such as
`#[rustfmt::skip`. Finding where the attribute itself ends would take a
parser. Escapes inside strings and characters are found with a second,
smaller lexer.

The Racket lexer follows Racket's reader with the default readtable. It
was written from the reader's source in `racket/src/expander/read`
rather than the reference manual, which leaves out some details. For
example, a `;` comment only ends at a linefeed. Symbols are
identifiers, even `define` and `lambda`, because only the expander
knows what they mean.
Keywords such as `#:when` get the keyword color. A `#;` comment covers
the datum after it, so the lexer counts parentheses to find where that
datum ends.

Whether an atom such as `1/2`, `+i` or `1e5x` is a number or a symbol
depends on Racket's long number syntax. `is_number` in `racket.rs`
follows `string->number` from the reader's `number.rkt`, with about one
state for each function there, but it only checks the syntax.

A `#lang` line at the top of a Racket file chooses the reader for the
rest. Only Racket comments can come before it. With `#lang at-exp`, the
lexer also reads @-forms such as `@foo[1]{text}`. With a Scribble
language such as `#lang scribble/manual`, the whole file is text with
@-forms in it. A `.scrbl` file is Scribble text even without a `#lang`
line, because a hunk from the middle of a document doesn't have one. A
hunk from the middle of an `at-exp` file is lexed as plain Racket, but
we almost always have the full text, which starts with the `#lang`
line.

`default_reader` in `racket.rs` lists languages that we know use the
default reader, such as `racket/base` and `typed/racket`. A language
that isn't on that list, and doesn't use shrubbery notation, Scribble or
`at-exp`, could have a reader of its own. So we read the file the way
its name suggests. A `.scrbl` file is Scribble text, a `.rhm` file is
shrubbery, and anything else gets the default reader.

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
file to it, so the language decides and not the file's name. So does a
`#lang` line in a `.rhm` file for a language we don't know. A `.rhm`
file without a `#lang` line is shrubbery, because it's a piece from the
middle of a file. The Rhombus Scribble languages, such as
`rhombus/scribble/manual`, read the whole file as text with @-forms in
it. Their files are `.scrbl` files like Racket's Scribble documents.
Without a `#lang` line, we read a `.scrbl` file as Racket Scribble.

Identifiers get no color of their own, like Racket's symbols, and
neither do `fun` and `def`, which are bound like any other name. Keywords such as `~into`
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

### gitignore

`gitignore.rs` reads `.gitignore` files the way git does. In git's
source, `add_patterns_from_buffer` and `parse_path_pattern` in `dir.c`
split the file into lines and find comments and negated patterns.
`dowild` in `wildmatch.c` reads each pattern. Templates such as
`Rust.gitignore` are gitignore files too.

A `#` starts a comment only at the very start of a line. A `!` there
negates the pattern and gets the keyword color. `*`, `**` and `?` get
the operator color, and `/` gets the punctuation color. A backslash and
the character it escapes get the escape color.

In a bracket expression such as `[a-z]`, the brackets get the
punctuation color. A `!` or `^` that negates it, and the `-` of a
range, get the operator color. A class such as `[:alpha:]` gets the
constant color. `dowild` has a few rules that the lexer copies:

- It reads one item before it looks for the closing `]`, so `[]]`
  holds a `]`.
- A `-` makes a range only between two items. At the start or the end
  it's plain, and so is a `-` right after a range or a class.
- A `[:` without a `:]` to end it is just a `[` and a `:`. So `[[:]`
  holds `[` and `:`.

git gives up on a pattern when a bracket expression isn't closed or
names a class that doesn't exist. The pattern then matches nothing, so
the rest of its line stays plain. A backslash at the end of a pattern
also makes it match nothing, and it stays plain too.

### CMake

`cmake.rs` follows CMake's own source. `cmListFileLexer.in.l` splits
the file into comments, brackets such as `[==[text]==]`, quoted
arguments and unquoted arguments. The parser in `cmListFileCache.cxx`
finds command names. `ExpandVariablesInStringImpl` in `cmMakefile.cxx`
reads escapes and variable references. Files named `CMakeLists.txt`,
and files ending in `.cmake` or `.cmake.in`, are CMake.

A command name can only come at the start of a line, before a `(` on
the same line. Most command names are identifiers, like function calls
in other languages. So are arguments that are only a name. The flow control commands, such as `if`, `foreach`
and `endfunction`, get the keyword color, because CMake doesn't let a
script define them again. Command names are case insensitive, so `IF`
is a keyword too. Inside `if`, `elseif` and `while`, an unquoted word
such as `AND`, `NOT` or `STREQUAL` gets the operator color. Those words
are case sensitive, and they mean nothing when quoted.

A `#` starts a comment anywhere outside of quotes, even in the middle
of an argument, and `#[[` starts a bracket comment. Brackets and quoted
arguments get the string color. They can go on over several lines, and
one that never closes goes on to the end of the file. A `[` in the
middle of an unquoted argument doesn't start a bracket.

For compatibility with old versions, an unquoted argument can hold text
in quotes, as in `-DA="b c"`, and `$(NAME)`. The quotes there hold
spaces and don't end the argument, and they get the string color. A
quote at the start of an argument starts a quoted argument instead.

In quoted and unquoted arguments, escapes such as `\n` and `\;` get
the escape color. A backslash before a letter or digit other than `t`,
`n` or `r` is an error, and it stays plain. A variable reference such
as `${NAME}`, `$ENV{NAME}` or `$CACHE{NAME}` gets the attribute color
as a whole, so that it stands out inside a string. A name can hold
escapes and other references, as in `${a_${b}}`. A reference with
another character in its name, or one that doesn't close, is an error,
and it stays plain. CMake only escapes the first byte of the character
after a backslash, so in a name, `\é` is an error too.

The `$<` and `>` of a generator expression get the punctuation color.
CMake reads generator expressions later, and only in some commands,
but the lexer can't tell which, so it colors them in every argument.
A `$<` only gets a color when its `>` comes later in the same argument.

The colors of a line never depend on the lines after it. So a variable
reference or generator expression only gets a color when it ends on
the line it starts on. CMake allows a line break inside a reference in
a quoted argument, but nobody writes one.

### YAML

`yaml.rs` follows YAML 1.2.2. It reads a file the way the scanner of
saphyr-parser does. That scanner is a port of libyaml's, fixed to pass
the YAML test suite. Files ending in `.yaml` or `.yml` are YAML, and so
are `.clang-format` and `.clang-tidy`.

Like the scanner, the lexer keeps a stack of the block collections it's
inside of, each with the column it starts in, and a stack of the flow
collections. It needs the block columns for two things. A plain scalar
goes on to the next line only if that line is indented more than the
collection the scalar is in. And a block scalar such as `run: |` holds
the lines that are indented more than its parent. Its first line that
isn't empty sets how far, unless the header gives a number, as in `|2`.
At the top level, saphyr-parser and libyaml count that number from
column 0, and so do we. The spec counts it from column -1.

A plain scalar or a quoted one is a key when a `:` follows it on the
same line. The scanner finds keys the same way. It remembers where a
key could start, and when a `:` comes, it marks what came after that
place as a key. A `:` is only a value indicator when a space or the
end of the line follows it, except inside a flow collection, where a
flow indicator can follow it too. There, a `:` can also come right
after a quoted scalar or a closing bracket, as in `{"a":1}`. The first
node of each entry in a flow mapping is a key even without a `:`, and
so is the node after a `?`.

Keys are plain, like bare keys in TOML, and quoted keys get the string
color. Values get the color of their type in YAML 1.2's core schema. So
`1.5`, `0x1F` and `.inf` get the number color, `true`, `null` and `~`
get the constant color, and other plain scalars get the string color.
Quoted scalars and the lines of block scalars get the string color,
and escapes such as `\n` in double quotes and `''` in single quotes get
the escape color. Indicators such as `-`, `:`, brackets, block scalar
headers and the document markers `---` and `...` get the punctuation
color. Anchors and aliases get the attribute color, tags get the type
color, and the names of directives such as `%YAML` get the keyword
color.

The colors of a line never depend on the lines after it. A key has to
end on the line it starts on, so that rule mostly holds by itself. The
exception is a plain scalar that goes on over several lines. Its value
joins the lines with spaces, so it's always a string, but we can't
know that on its first line. So the first line gets the color of its
own text, and the other lines get the string color. A quoted scalar
that never closes goes on to the end of the file, or to the next
document marker.

### Markdown

`markdown.rs` follows CommonMark 0.31.2, along with the tables and task
lists of GitHub Flavored Markdown. Files ending in `.md`, `.markdown`,
`.mdown`, `.mkd`, `.mkdn` and `.mdwn` are Markdown.

Blocks are read a line at a time, the way `incorporateLine` in
commonmark.js, the reference parser, reads them. That includes its
column counting, where a tab stops every four columns and a container
can use up part of one. Headings, both the `#` kind and the underline
of a setext heading, get the keyword color. Code spans and code blocks
get the string color. The markers of block quotes and list items,
thematic breaks, fences, task markers and the pipes of tables get the
punctuation color. A fenced code block whose info string names a
language we have a lexer for, such as `rust` or `toml`, is lexed in
that language once the block ends. Its info string gets the attribute
color, and so do link destinations. HTML gets colors of its own, with
tag names as keywords, attribute names as attributes, values as
strings and comments as comments. Emphasis is plain, since colors
can't show it.

Markdown mostly lets a line's colors depend only on the lines before
it, but a few things don't:

- The text of a setext heading only becomes a heading at its
  underline, and a table's header row only becomes one at the
  delimiter row after it. So only the underline and the delimiter row
  get colors.
- Code spans, links, raw HTML and link reference definitions can go
  on over several lines of a paragraph, and whether a backtick starts
  a code span depends on whether a closing one comes later. So each
  line of a paragraph is lexed as if the paragraph ended with it. A
  code span that covers several lines only gets its color on the line
  where it ends, and the same goes for links and tags.

Lexing each line of a paragraph from the start would take time that
grows with the square of the paragraph's length. So the paragraph
remembers the last line where nothing was left open, such as a
backtick without its closing one or a `[` without its `]`, and lexes
from there. When something stays open, it still lexes at most the last
64 lines.

Whether `[text][label]` is a link depends on whether the document
defines `label`, maybe further down. We can't look there, so we take
every label as defined. The one exception is a link like `[foo]` that
is only a label. Text in brackets is often not a link at all, so that
stays plain.

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

### JavaScript

`javascript.rs` follows the ECMAScript standard. Files ending in `.js`,
`.mjs` and `.cjs` are JavaScript.

A `/` can start a regular expression or divide, and only a parser can
tell which. The lexer guesses from the token before it. After a name,
a literal or a `]`, a `/` divides. After an operator or most keywords,
or where a statement starts, it starts a regular expression. `++` and
`--` don't change the guess, because they come right after an operand
or right before one. Two closing brackets need more care:

- A `)` ends an operand, except after the condition of `if`, `while`,
  `for` or `with`. A statement comes after that one, so
  `if (x) /a/.test(y)` holds a regular expression.
- A `}` that closes a block is followed by a statement, and one that
  closes an object literal ends an operand. A `{` starts an object
  literal after an operator or a keyword such as `return`, and a block
  anywhere else. After a `:`, it starts a block when the `:` ends a
  label or a case. We guess that from whether we're inside an object
  literal or other brackets.

So the lexer keeps a stack of the brackets it's inside of. The same
stack holds the `${` of each template literal, and the `}` that closes
one goes back to the template's text. A `)` or `]` never closes a
`${`. A closing bracket that matches nothing changes nothing, since a
hunk can start in the middle of a block.

The reserved words get the keyword color, and so do `let`, `static`,
`async` and `await`. Any word after `.` or `?.` names a property, and
so does a word followed by `:` or `(` where a property starts in an
object literal. Those are identifiers, so `x.default` and `{new: 1}`
don't have keywords in them. `as` and `from` are keywords only in
`import` and `export` declarations, and `of` only after the variable
in the parentheses of `for`. `true`, `false` and `null` get the
constant color. So do `undefined`, `NaN` and `Infinity`, which are
really global variables, because Sublime Text colors them that way.
Private names such as `#x` are identifiers. The `@` of a decorator and
the name after it, as in `@a.b`, get the attribute color.

A string can't go on to the next line unless a backslash comes before
the line break, so one without its closing quote ends with its line.
In strings and template literals, a backslash and the character after
it are an escape, except for a `\x` or `\u` without the hex digits it
needs. Old octal escapes such as `\012` count too. A regular expression
gets the string color, and a backslash and the character after it get
the escape color. A `/` inside a class such as `[/]` doesn't end it.
One without its closing `/` ends with its line. A `#!` line at the
very start of a file is a comment.

### JSON

`json.rs` follows RFC 8259. Files ending in `.json`, `.jsonc`,
`.jsonl`, `.ndjson`, `.geojson`, `.ipynb` and `.webmanifest` are JSON,
and so are `composer.lock`, `flake.lock` and `Pipfile.lock`. Many
files that end in `.json`, such as `tsconfig.json` and VS Code's
settings, have `//` and `/* */` comments, so the lexer reads those in
every JSON file. Nothing else in JSON can start with a `/`.

Keys are plain, like bare keys in TOML and YAML, so that the values
stand out. In JSON every key is quoted, so coloring keys as strings
would color most of the file the same. A string is a key when a `:`
follows it on the same line, with nothing but spaces and tabs between
them. That works in a hunk from the middle of an object too, where a
stack of brackets wouldn't know that it's in an object.

Strings get the string color, and escapes such as `\n` and `\u00e9`
get the escape color. A backslash before any other character is plain
string. A string can't go on to the next line, so one without its
closing quote ends with its line. Numbers get the number color, and
`true`, `false` and `null` get the constant color. Anything else that
isn't a space, a bracket or a quote is one plain word, so `truex` and
`01` aren't partly colored. That includes the extra values of JSON5,
such as `NaN` and `0x1F`.

### Rainbow identifiers

With `rainbow_identifiers` on, which is the default, each `Identifier`
token gets a color
picked from its name, so every use of a name has the same color and
different names usually don't. Drawing hashes the token's text and
uses the hash to pick one of the 16 colors in `Theme::identifiers`.
The lexers only mark identifiers that get no other color, so keywords,
and words such as `AND` in a CMake `if`, keep their usual colors.

The C, C++, CMake, JavaScript, Rust, Racket, Scribble and Rhombus
lexers mark identifiers. TOML, YAML, JSON, Markdown and gitignore have
nothing that is really a name in code, so they don't.

`Theme::build` makes the 16 colors in Oklab, where the distance
between two colors matches how different they look much better than
in sRGB. The hues are spread evenly around the color wheel. The
lightness is the average of the theme's string, number, keyword, type
and function colors, so that the colors look like they belong to the
theme. The chroma is 80% of their average, so identifiers stand out a
little less than other tokens, but at least 0.08. Nord's colors are so
muted that its hues would be hard to tell apart without that floor.
Identifiers are most of the text in code, so a color that would be
harder to read than the text moves toward the text's lightness until
it's about as easy to read.

The hash is FNV-1a followed by the final mixing step of MurmurHash3.
FNV-1a on its own leaves the low bits of the hash depending only on
the low bits of each byte, and the color comes from the low bits.

### Dropping stale results

Highlights are keyed by section and file index, and those indexes mean
something else after a reload. So `Detail` has a `Generation` that the
highlighter shares. It moves on whenever the files change. Jobs from an older generation are skipped before the worker
starts them, and results from one are dropped when they arrive.

## Drawing a line

`draw_code` draws a diff line one grapheme at a time through
`text::draw` (see `text-width.md`). It takes the line's background from
whether it's added, removed or context, the foreground from the syntax
colors, and the stronger background from the word marks. The color of
an identifier depends on its text, so `draw_code` finds it once for each
token rather than for each cell. A trailing
carriage return is left out, so CRLF files don't show a stray symbol.
Selected lines get the selection background instead.

In the unified view, a bar in the column between the line numbers and
the code marks added and removed lines, in green or red. Context lines
leave that column blank.

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
- Each lexer that marks identifiers has a test called
  `marks_plain_identifiers`. The other lexer tests leave identifiers
  out of the tokens they compare, which keeps them readable.
- `src/theme.rs` checks that identifier colors differ from each other
  and are about as easy to read as the text, and that a name always
  gets the same color. `tests/ui.rs` has
  `identifiers_can_get_colors_by_name`.
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
- `src/highlight/cmake.rs` has examples for comments, brackets,
  command names, `if` operators, escapes, variable references,
  generator expressions and the arguments that old versions allowed.
  The dev image has no CMake, so nothing compares it with the real one
  automatically. When we wrote it, we built `cmListFileLexer.c` from
  CMake's source into a program that prints its tokens, and simulated
  CMake's parser on them. We checked that each byte had the color its
  token called for, on 25,000 random texts and on the 12,885 CMake
  files in CMake's own source, and they agreed on all of them. For
  escapes and variable references, we ran `message("…")` with `cmake
  -P` on 15,000 random arguments. With no variables defined, the colors
  say whether CMake gives an error, and what it prints if it doesn't.
  They agreed on all of them.
- `src/highlight/javascript.rs` has examples for each kind of
  JavaScript token, and a list of places where a `/` divides or starts
  a regular expression. The dev image has no JavaScript parser, so
  nothing compares the lexer with one automatically. When we wrote it,
  we parsed the 6,513 JavaScript files in some popular npm packages,
  such as lodash, three, d3, webpack and prettier, with acorn. We
  checked whether each character was in a comment, a string, a
  template literal, a regular expression or a number. The lexer agreed
  with acorn on all of them.
- `src/highlight/json.rs` has examples for each kind of JSON token,
  such as keys, escapes, comments and words that only start like
  values. Its property test builds random valid documents, with
  comments between tokens and strings that hold brackets, colons and
  comment markers. It builds the expected color of each byte along
  with the text, and checks the lexer against it.
- `src/highlight/toml.rs` has examples for each kind of TOML token. Its
  property test builds random valid documents from tricky pieces, such
  as keys that look like values and strings that hold brackets. It
  parses them with `toml_edit`, which already reads the config file,
  and checks that each value has the color of its type and that each
  key is plain, or a string if it's quoted.
- `src/highlight/yaml.rs` has examples for each kind of YAML token,
  such as keys that look like values, plain scalars over several
  lines, escapes, block scalars with indicators, and tags. Its property
  test builds random streams of documents from nested block
  collections, flow collections, scalars of every style, block
  scalars, anchors, aliases, comments and directives. It reads them
  with `saphyr-parser`, which passes the YAML test suite, and checks
  each scalar against the parser's events. Plain keys must be plain,
  other plain scalars must have the color of their type, other scalars
  the string or escape color, aliases the attribute color, and nothing
  outside of a scalar may be a string, a number or a constant. It
  agreed on a million streams while we wrote the lexer. We also ran
  the same check on the inputs of the YAML test suite and on the YAML
  files of Ansible, the Kubernetes examples and the OpenAPI
  specification, 5,438 files that saphyr-parser reads, and they agreed
  on all of them. On those files, cutting the text after a line never
  changed the colors of the lines before it.
- `src/highlight/markdown.rs` has examples for each kind of Markdown
  block and inline construct. Its property test builds random documents
  from nested block quotes, lists, tables, code blocks, HTML and text
  full of code spans, links and tags. It parses them with
  `pulldown-cmark` and checks that code spans and code blocks have the
  string color, headings the keyword color, rules and task markers the
  punctuation color, and that text is plain. It skips the lines that a
  code span, link or tag goes on from. They agreed on about 1.2 million
  documents while we wrote the lexer. The generator defines every link
  label that its pieces can make, and it ends each table with a blank
  line. After a table, pulldown-cmark reads an indented line as code if
  it has a pipe, and as a row if it doesn't. We always read it as a
  row.
- `src/highlight/gitignore.rs` has examples for comments, negation,
  escapes and bracket expressions. Its property test builds random
  patterns, many of them with tricky bracket expressions. From the
  colors alone, it works out a path that the pattern should match, and
  asks `git check-ignore` whether it does. When the pattern has no `*`,
  each part of it matches one byte. So the test also changes one byte
  of the path at a time to one that the colors say its part doesn't
  match, and checks that git doesn't match the result. They agreed on
  about 200,000 patterns while we wrote the lexer. We ask git rather
  than gix's port of `wildmatch.c`, because the port reads some
  bracket expressions differently, such as `[:aa[:]` and
  `[\*-[:alpha:]`.
- `src/highlight/mod.rs` has property tests that lexers never panic and
  give tokens in order on character boundaries, and that every hunk line
  gets the same colors with or without the full text, where it should.

## Known gaps

- A merge commit is diffed against its first parent only.
- Only C, C++, CMake, JavaScript, Rust, Racket, Rhombus, Scribble,
  TOML, YAML, Markdown and `.gitignore` files are highlighted. That
  leaves out JSX and TypeScript.
- The JavaScript lexer can guess wrong about a `/`. In a statement
  such as `a ? b : {} / 2`, it reads the `{}` as a block and the `/`
  as the start of a regular expression.
- A hunk from the middle of a JavaScript template literal that we
  don't have the full text for reads the template's text as code.
- The JavaScript lexer reads `<!--` and `-->`, which start comments
  in old scripts, as operators.
- A hunk from the middle of a CMake `if` whose arguments go on over
  several lines doesn't know it's in an `if`, so `AND` and the other
  operators there are plain.
- In Markdown, the text of a setext heading and a table's header row
  are colored as paragraph text, and a code span, link or tag that
  goes on over several lines only gets its color on its last line.
- A Markdown reference link whose label isn't defined, such as
  `[a][b]`, still gets the colors of a link. Named entities such as
  `&foo;` get the escape color whether or not HTML defines them.
- A hunk from the middle of a multi-line TOML array that we don't have
  the full text for reads its values as keys. Strings keep their
  color, but numbers, `true` and `false` are plain. A
  nested array that starts a line there, as in `[1, 2],`, gets the
  color of a table header.
- A YAML plain scalar that goes on over several lines gets the color of
  its first line's text there. So in `a: 1` followed by an indented
  `b`, the `1` is a number, although the value is the string `1 b`.
- YAML 1.1 readers, such as PyYAML and older versions of go-yaml, read
  `yes`, `no`, `on` and `off` as booleans. YAML 1.2 doesn't, and we
  follow it, so they get the string color.
- A hunk from the middle of a YAML block scalar or a quoted scalar
  over several lines that we don't have the full text for is read as
  ordinary YAML. So `a: b` in a shell script under `run: |` reads as a
  key and a value.
- In C and C++, a splice in the middle of a token, as in `L\` at the
  end of a line followed by `'x'`, splits the token in two. The
  compiler joins them.
- C and C++ read `<:` and `:>` as `[` and `]`. We read them as two
  tokens, which only changes the color of the `<` or `>`.
- A hunk from the middle of a Rhombus Scribble document that we don't
  have the full text for is lexed as Racket Scribble.
- Other Racket languages with a reader of their own, such as
  `#lang datalog`, are lexed the way the file's name suggests. In a
  `.rkt` file, that's the default reader. `#reader scribble/reader`
  always gets the default reader.
- A hunk from the middle of a `#lang at-exp` file that we don't have the
  full text for is lexed as plain Racket.
- Rainbow identifiers only look at the name. Two different variables
  with the same name get the same color, and with 16 colors, two
  different names often share one too.
- Word marks pair removed and added lines in order. When lines were
  inserted between edited ones, a line can be paired with the wrong
  partner, and then gets no marks or odd ones.
