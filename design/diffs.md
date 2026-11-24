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

## Commit diffs

`commit::load` diffs a commit against its first parent, or against the
empty tree for a root commit. It asks two git processes that keep
running between loads:

1. `git diff-tree --stdin` gets a commit id on a line, and prints the
   commit's parents, author, committer and message, followed by the
   files that changed, with their modes, blob ids, and renames and
   copies.
2. `git cat-file --batch` gets the id of every blob in the list and
   prints the blobs.

Then imara-diff diffs the texts, on a blocking thread. We don't use
the patch that `git show` would print, because we need the full text of
both sides anyway, for syntax highlighting, and because git's diff
settings would change what the patch looks like. Moving through the history loads a commit for
every row the user passes, so this has to be quick. Starting git takes
a few milliseconds, more on macOS and in repositories with many packs,
and once took up most of the time of a small commit. With the processes
already running, a small commit takes well under a millisecond on
Linux. Only the newest request is loaded (see below), so a slow disk
makes the pane lag behind, but never queues up work.

### The running git processes

`Git::start_process` starts a process like these, and `Git` keeps one
of each kind while it waits for the next request. A load takes it with
`take_process`, or starts one if there is none, and hands it back with
`put_process` only once it has read every answer in full. A load that
is dropped halfway, because a newer request came in, drops its process
too, which kills git. The next load then starts a new one. So no load
ever reads what was left of an earlier answer. Two loads at the same
time, such as the detail worker and "Show anyway", each get their own
process, and only one is kept afterwards.

`git cat-file --batch` starts each answer with the object's size, so we
know where it ends. `git diff-tree` doesn't mark the end of its file
list, and it prints nothing at all for a commit that isn't there. So
after each request, `diff_tree` sends the line `tergi`. Git prints
lines that don't start with an object id back as they are, and that
line marks the end of the answer. `read_answer` reads the answer by its
structure: the fields of the format, then file entries that each start
with `:`. Anything else must be the end line, so a path that contains
`tergi` can't be mistaken for it.

The requests take some care:

- A request with only the commit gives its parents as git sees them.
  A request that also names a commit to diff against makes git print
  that commit as the only parent. The commit object can list parents
  that git ignores, such as in a shallow clone, where the oldest commit
  is a root, so we never read the parents from the object.
- Git doesn't diff a merge against anything unless it is asked, so the
  first request gives a merge's parents and no files. A second request
  asks for the diff against the first parent.
- With `--always`, git prints a commit even when it changed nothing.
  With `--root`, it diffs a root commit against the empty tree.

Git reads the config once, when it starts. It also reads the mailmap
and the replace refs only once. `App::refresh` calls `stop_processes`,
which stops the waiting processes and makes sure the busy ones aren't
kept. The next load starts new ones. Every change in the repository
or the working tree leads to a refresh, and so does the terminal
getting focus, which covers the user's global config. Moving through
the history doesn't refresh, so that's when the processes get reused.
Objects that appear while git runs aren't a problem. When git can't
find an object, it looks for new packs.

A process's pipes belong to the Tokio runtime that started it. The app
has only one runtime. Tests that aren't async share one too, from
`common::runtime`, so a `Git` never moves between runtimes.

Each process collects its stderr in the background, because git can
write to it at any time, such as a warning about the rename limit, and
would block once the pipe is full. When an answer doesn't come as
expected, `Process::error` kills git and puts the end of its stderr in
the error.

We use `git diff-tree` rather than `git diff`, because only it can keep
running and answer one request after another. It reads
`diff.renameLimit` from the config, but not `diff.renames`. So
`start_diff_tree` reads that setting with `git config` and passes
`-M`, `-C` or `--no-renames`, the way `git diff` would use it. Renames
and copies then come out as `git show` shows them. `diff-tree` doesn't
read `diff.relative`, `diff.ignoreSubmodules` or `diff.orderFile`
either, which is what we want.

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
- A diff over 20,000 lines (`DEFAULT_LINE_LIMIT`) is marked too large.
  The user can ask for it anyway with Enter on the notice, which loads
  the commit again without the limit. Blobs of any size are diffed, so
  a big file with few lines, such as minified code, is shown.
- A submodule shows up as `Subproject commit <id>`, as git shows it.

A stash made with `--include-untracked` keeps its untracked files in a
third parent, a root commit with only those files. Diffing against the
first parent would leave them out. So when a commit has three parents,
the second request also asks about the second and third parents.
`is_stash` recognizes such a stash by their parents and the third
parent's message, and `load` adds the third parent's files as
untracked, with status `?`.

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
one commit at a time. When a new request comes in during a load, it
drops that load, which kills its git, and starts on the new one. The
diffing on the blocking thread can't be stopped, so it finishes, and
its result is thrown away. Requests only come when the selection or
the context setting changes, so a slow commit can't be stopped over and
over by refreshes.

Each request has a number. When a result comes back, the app checks the
number and the source against what the pane shows now, and drops the
result if either differs.

### Waiting for a commit

Most commits load in a few milliseconds. If the pane showed "Loading…"
as soon as the selection moved, it would flash that for a frame or two
on almost every move. So drawing keeps showing what the pane showed
before, until the new commit arrives or `DETAIL_GRACE` (150 ms) passes.
After that, the old contents stay but fade, which says they are out of
date. The working directory is the same at startup, before its first
load.

This only happens on screen. `keep_detail_picture` in `ui/draw.rs`
copies the pane's cells after each frame where the pane isn't waiting,
and draws them back over the pane while it waits. The rows and hits
already belong to the new commit, so clicks and keys can't act on the
old one. `App::update_detail_wait` notes when the wait started, and it
sends a tick when the grace period ends, so the fade shows up on time
even if nothing else happens. If the pane has moved or changed size
since the copy was made, the copy is no use. The pane then stays empty
during the grace period and shows "Loading…" after it.

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
- `HunkEnd`: a thin bar under each hunk, which the user drags to move
  the hunk's bottom edge. It isn't a stop for j and k, but a click puts
  the cursor there, and then it counts as part of the hunk.
- `Split`: one row of the side-by-side view, with a line on each side.
- `Blank` and `Empty`: spacing, and placeholders such as "Loading…".

In the side-by-side view, `push_split_rows` pairs each run of removed
lines with the run of added lines after it, in order. Context lines
appear on both sides.

### Folding

Files can be folded with Space. Folds are kept by section and path in
`Detail::collapsed`, so they survive a reload. Everything that folds or
unfolds a file goes through `App::set_folded`, which also saves the
change in the same database as the hidden branches (`src/state.rs`,
and "Saving them" in `history.md`). So folds survive a restart too.

Each commit has its own folds. A file folded in one commit isn't folded
in the other commits that touch it. `set_source` forgets the folds of
the old commit, and the new commit's folds arrive with the commit. The
detail worker reads them from the store right after it loads the
commit, so the files never show up unfolded first. A comparison of two
commits is kept under both ids.

A commit with more than 40 files starts with all of them folded, and
other commits start with none (`doc::default_folds`). Large diffs
aren't folded, however long they are. The store only keeps a commit
while its folds differ from the ones it starts with. Otherwise every
commit the user looked at would stay in it. It keeps the whole set for
the commit in one row, because an empty set ("the user unfolded
everything") has to be told apart from no row at all.

The store saves in the background, so it may not have written a
commit's folds yet when the user comes back to that commit a moment
later. `App::commit_folds` keeps every set saved in this run, and it
wins over what the store reads.

In the working directory, each section has its own folds, so a file
can be folded in its unstaged changes and not in its staged ones. These
are read once at startup and saved one change at a time. When a file is
staged as a whole, with its file, its section or "Stage all", its
staged changes take the fold that its unstaged changes had, and the
unstaged changes are unfolded. Unstaging works the other way around.
The operation does this when it succeeds, through `After::MoveFolds`.
An unstaged file can end up in the unstaged or the untracked section,
so it gets the fold in both. Staging single hunks or lines leaves the
folds alone.

After each load of the working directory, `drop_stale_folds` unfolds
the files that are no longer in their section. A change that comes back
later starts out unfolded, and the store doesn't fill up with files
that were committed long ago. A load that started before an operation
finished may still show the files where they were before it, which
would drop the folds that `MoveFolds` just moved. So the app counts
finished operations, and only a load that started with no operation
running, and none finishing since, drops folds.

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

### Moving the edges of hunks

The user can move the top and bottom edges of a hunk, to show the
unchanged lines around it or to hide its context lines. `K` and `J`
move the top edge, and Ctrl+J and Ctrl+K move the bottom edge. With
the mouse, the user drags the hunk's header or its `HunkEnd` row. When
two hunks meet, the pane shows them as one.

This only changes what the pane shows. The diffs in the pane's data
stay as git gave them. Each hunk the pane shows is a run of whole hunks
of git's, and staging it stages those as they are. So a mistake here
can show the wrong lines, but it can't stage the wrong thing.

`view` in `src/git/context.rs` works out what the pane shows. It walks
the whole file in order. Changed lines always show up. The context
lines in git's diff show up unless the user hid them, and the lines
between git's hunks show up if the user chose them. Each run of lines
that show up and has a change in it becomes a `Group`, which is one
hunk in the pane. A group lists its lines as `Shown` values, which are
either a line of one of git's hunks or a line from the texts.

To keep git's hunks whole, the context lines between the first and last
change of a hunk always show up. A context line further out only
belongs to its hunk if every line between it and the hunk's changes
shows up too. Otherwise `view` treats it like a line from the texts,
so that it can join the next hunk instead.

The lines between git's hunks come from the full texts of both sides,
which we load for syntax highlighting anyway. A line is only used if
both texts have it with the same text and line ending, because the
texts are read separately from the diff and the file may have changed
in between. Without the texts, an edge can still move in, but it can
only move out as far as git's context went. A group takes the section
name of its first hunk of git's.

The choices are kept in `Detail::shown_lines`, by section and path
like folds, as a map from line numbers to shown or hidden. A line is
named by its number on the side of the diff that staging leaves alone.
That's the new side for unstaged changes and commits, and the old side
for staged changes. So staging one hunk of a file doesn't move the
choices for its other hunks. Untracked and conflicted files show every
line as added, so they have no edges to move. A file that leaves its
section loses its choices, and so does a commit when the pane moves to
another one.

Each rebuild of the rows works out the view of each file with choices
again and keeps it in `Detail::views`. Other files have no view, and
the pane shows their hunks as git gave them. The rows of a group look
like this:

- `Hunk`, `HunkEnd` and `Extra` rows name the group by its first hunk of
  git's. `Detail::hunk_at` gives that name for any row, and
  `Detail::shown_hunks` gives the range of git's hunks in a group.
- `Line` and `Split` rows still name one of git's hunks and a line in
  it. So line staging, word marks and syntax colors work as before.
- An `Extra` row is a line from the texts, by its index in
  `View::extra`. In the side-by-side view it fills both sides.

The highlighter gets a file's extra lines as one more hunk after git's,
so their colors come back at that index. When an edge moves, the syntax
colors of the file are dropped, because the extra lines changed. Word
marks stay, since they only cover git's lines. The rebuild keeps the
cursor on its row as usual. If the row went away, the cursor goes to
the header of the group that now holds its hunk of git's.

While the mouse drags the top edge, the pane scrolls by one row for
each line shown or hidden. The lines of the hunk then stay where they
are on the screen, and the header follows the mouse. A drag ends when
its hunk joins another one, because the edge it held is gone.

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
JSON, `python.rs` for Python, `rust.rs` for Rust, `racket.rs` for
Racket and Scribble, `shrubbery.rs` for Rhombus, `sh.rs` for shell
scripts, `fish.rs` for fish scripts, `dockerfile.rs` for Dockerfiles, `toml.rs` for TOML, `yaml.rs` for YAML, `zig.rs` for Zig,
`llvm.rs` for LLVM IR,
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
parser.

The Rust lexer follows rustc's own lexer, `rustc_lexer`, even where
rustc only reads something so that it can report an error. So `'ab'`
is a character, `2b` is a number with the suffix `b`, and `"a"x` is a
string with the suffix `x`. An emoji joins the names around it into
one invalid name, which gets no color. Reading mistakes the way rustc
does keeps them from changing the colors of the code after them. A
shebang line at the top is a comment. Escapes are read the way rustc
reads them, and one that it rejects, such as `\q`, is plain string.

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
rest. Only Racket comments can come before it. A `#!` line such as
`#!/usr/bin/env racket` is one of those comments. A script that starts
with one is Racket even without an extension, and a `#lang` line after
it still chooses the reader. With `#lang at-exp`, the lexer also reads @-forms such as `@foo[1]{text}`. With a Scribble
language such as `#lang scribble/manual`, the whole file is text with
@-forms in it. A `.scrbl` file is Scribble text even without a `#lang`
line, and so is a Markdown code block marked `scribble`.

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

A `#;` or `@;{` comment can cover several lines. One that the text
doesn't finish is an error, and it goes on to the end of the file, like
an unfinished `#|` comment.

### Rhombus

Rhombus and its relatives, such as Shplait, are written in shrubbery
notation. `shrubbery.rs` follows the lexer that DrRacket colors them
with, in `lex.rkt` from the `shrubbery-lib` package. A `#lang` line
for one of these languages makes the Racket lexer hand the rest of the
file to it, so the language decides and not the file's name. So does a
`#lang` line in a `.rhm` file for a language we don't know. A `.rhm`
file without a `#lang` line is shrubbery, and so is a Markdown code
block marked `rhombus`. The Rhombus Scribble languages, such as
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
break outside of brackets starts a new key. Each part of a key is an
identifier, so it gets a color from its name (see "Rainbow
identifiers"). The quotes of a quoted part are plain and aren't part
of its name, so `"name"` and `name` get the same color. The dots
between parts get the punctuation color.

A hunk from the middle of a multi-line array has values at the start
of its lines, where the lexer expects a key. So a word or a quoted
string is only a key when a `.` or `=` comes after it on its line, or
when it starts or ends with a dot, as `.b` and `a.` do in `a .b = 1`
and `a."b" = 1`. Otherwise it gets the color of the value it would be.

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

In a quoted argument, a variable reference or generator expression
can go on over several lines. A backslash at the end of a line joins
the lines there. CMake drops it before it reads references, so it can
even split the `${` of one.

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

A plain scalar or a quoted one is a key when a `:` follows it. Outside
of flow collections, the `:` has to be on the same line, within 1024
characters. Inside one, it can come any number of lines later. The
scanner finds keys the same way. It remembers where a key could start,
and when a `:` comes, it marks what came after that place as a key. A
`:` is only a value indicator when a space or the end of the line
follows it, except inside a flow collection, where a flow indicator
can follow it too. There, a `:` can also come right after a quoted
scalar or a closing bracket, as in `{"a":1}`. The first node of each
entry in a flow mapping is a key even without a `:`, and so is the node
after a `?`.

Keys are identifiers, as in TOML, and the quotes of a quoted key are
plain. A plain key that goes on over several lines is an identifier on
each line. A quoted one gets the string color instead, since its name
would be split over several tokens. Values get the color of their type
in YAML 1.2's core schema. So `1.5`, `0x1F` and `.inf` get the number
color, `true`, `null` and `~` get the constant color, and other plain
scalars get the string color. A plain scalar that goes on over several
lines is always a string, since its value joins the lines with spaces.
So in `a: 1` followed by an indented `b`, the `1` is a string.
Quoted scalars and the lines of block scalars get the string color,
and escapes such as `\n` in double quotes and `''` in single quotes get
the escape color. Indicators such as `-`, `:`, brackets, block scalar
headers and the document markers `---` and `...` get the punctuation
color. Anchors and aliases get the attribute color, tags get the type
color, and the names of directives such as `%YAML` get the keyword
color.

A quoted scalar that never closes goes on to the end of the file, or
to the next document marker.

### Markdown

`markdown.rs` follows CommonMark 0.31.2, along with the tables and task
lists of GitHub Flavored Markdown. Files ending in `.md`, `.markdown`,
`.mdown`, `.mkd`, `.mkdn` and `.mdwn` are Markdown.

Blocks are read a line at a time, the way `incorporateLine` in
commonmark.js, the reference parser, reads them. That includes its
column counting, where a tab stops every four columns and a container
can use up part of one. Headings, both the `#` kind and the setext
kind with its underline, get the keyword color. Code spans and code blocks
get the string color. The markers of block quotes and list items,
thematic breaks, fences, task markers and the pipes of tables get the
punctuation color. A fenced code block whose info string names a
language we have a lexer for, such as `rust` or `toml`, is lexed in
that language once the block ends. Its info string gets the attribute
color, and so do link destinations. HTML gets colors of its own, with
tag names as keywords, attribute names as attributes, values as
strings and comments as comments. Emphasis is plain, since colors
can't show it.

A paragraph keeps its lines until it ends, so that its text can be
read as a whole. Code spans, links, raw HTML and link reference
definitions can go on over several lines of a paragraph, and they get
their colors on each line they cover. Whether a backtick starts a code
span depends on whether a closing one comes later, and the lexer can
look there. In the same way, the text of a setext heading gets the
keyword color once its underline shows up. When a delimiter row turns
the last line of a paragraph into a table's header row, the lines
before it are lexed as a paragraph and the header as a row.

Whether `[text][label]`, `[label][]` or `[label]` is a link depends on
whether the document defines `label`, maybe further down. So the
lexer colors the definitions at the start of each paragraph when the
paragraph ends, but keeps the rest of its text, and the text of each
table cell, until the end of the document. By then it knows every
label. Labels match as CommonMark says, without regard to case or to
how much space they hold.

A backtick or a `<!--` that never closes would make the lexer read the
rest of the paragraph to look for its end. Many of them in a long
paragraph would take time that grows with the square of its length. So
the lexer remembers each closing text it didn't find and doesn't look
for it again.

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
them from `git cat-file`. For an unstaged file, the old side is in the index
and the new side is on disk. For a symlink, the text on disk is the path
it points to, which is what git stores. For a staged file, the old side
is in HEAD and the new side is in the index. `Workdir::load` reads the
index and HEAD versions with `read_blobs`, which runs
`git cat-file --batch`. It skips files that git's diff calls binary.
A submodule's id names a commit in the submodule, which git usually
reports as missing. It asks for blobs by the ids in
the diff's `index` lines, which `--full-index` makes complete. Asking by
path wouldn't work for every name. `git cat-file` reads one name per
line, and a path like `1:x` names a stage of the index.

The language is chosen by the file's extension, in
`Language::for_path`. Scripts often have no extension, so when the
name doesn't give a language, `Language::for_shebang` looks for a `#!`
line that runs Python, a shell in the sh family, Node or Racket.
Examples are `#!/usr/bin/env python3`,
`#!/usr/bin/env -S uv run --script`, `#!/bin/sh`, `#!/usr/bin/env node`
and `#!/usr/bin/env racket`. We check the first line of both
sides of the diff. Without the full text of a side, we only see its
first line if a hunk shows it. Files in other languages aren't
highlighted.

### JavaScript

`javascript.rs` follows the ECMAScript standard. Files ending in `.js`,
`.mjs` and `.cjs` are JavaScript, and so are scripts that start with
a `#!` line that runs `node`.

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

Keys are identifiers, as in TOML and YAML, and their quotes are plain.
In JSON every key is quoted, so coloring keys as strings would color
most of the file the same. A string is a key when a `:`
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

### Zig

`zig.rs` follows the tokenizer in Zig's standard library,
`lib/std/zig/tokenizer.zig`. Files ending in `.zig` are Zig, and so
are `.zon` files such as `build.zig.zon`, which hold Zig values.

Zig is easy to lex. It has no block comments, and no string can go on
to the next line. A multiline string is a run of lines that each start
with `\\`, and each of those lines gets the string color on its own.
So a line's colors never depend on the lines around it, and a hunk
gets the same colors whether we have the full text or not.

Comments, including doc comments that start with `///` or `//!`, get
the comment color. Strings and characters get the string color, and
escapes such as `\n`, `\x7f` and `\u{1F600}` get the escape color. A
backslash before any other character is plain string. A string without
its closing quote ends with its line, as it does in the tokenizer.

Keywords and builtins such as `@import` get the keyword color. Nobody
can define a builtin, so they are part of the language like keywords.
`true`, `false`, `null` and `undefined` are identifiers to the
tokenizer, but no declaration can have those names, so they get the
constant color. Every other name is an identifier, including types
such as `u8` and quoted names such as `@"with spaces"`. Identifiers are
ASCII only in Zig. Older versions of Zig had `async`, `await` and
`usingnamespace` as keywords, but they are names now. The `Io`
interface has methods called `async` and `await`, so they stay plain.

The tokenizer reads a number loosely and leaves checking it to the
compiler. After the first digit it takes any letters, digits and
underscores, a sign right after `e`, `E`, `p` or `P`, and one period
that has a letter or digit after it. The lexer reads numbers the same
way, so `0x1.8p+3` is one number, and `0..2` is `0`, `..` and `2`.
Brackets, `,`, `;`, `:` and `.` get the punctuation color. Other
symbols, such as `.*`, `?` and `+%=`, get the operator color. To the
tokenizer, `.?` is a `.` and a `?`, so it gets both colors.

### LLVM IR

`llvm.rs` follows LLVM's own lexer, `llvm/lib/AsmParser/LLLexer.cpp`.
Files ending in `.ll` are LLVM IR, and so are Markdown code blocks
marked `llvm`.

Names of values start with a sigil. Local values and basic blocks have
`%`, globals have `@`, comdats `$`, metadata `!`, attribute groups `#`
and summary entries `^`. These names are identifiers, and so are
numbered ones such as `%0` and `!12`. To LLVM, `!12` is a `!` followed
by a number, but it names a node the way `!foo` does. The `%` of a
local value isn't part of its identifier. A basic block's label leaves
out the `%`, as in `entry:`, so the label gets the same color as the
`%entry` in a branch to it. A name such as `!DILocation` right before
a `(` is a kind of node that only LLVM can define, so it gets the
keyword color.

Every bare word is a keyword, a type or a constant. LLVM has hundreds
of keywords, mostly attributes and calling conventions, and adds more
in most releases. A bare word that isn't one is an error, so we don't
list them. Types are words such as `ptr`, `float` and `i32`. Constants
are `true`, `false`, `null`, `none`, `undef`, `poison` and
`zeroinitializer`, and the values of enums in debug info, such as
`DW_TAG_member` and `DIFlagPrototyped`. Like LLVM, the lexer ends an
integer type at its last digit, so `i32x` is `i32` and `x`. It reads
`cc10` as the keyword `cc` and the number `10`.

To LLVM, any run of letters, digits, `-`, `$`, `.` and `_` followed by
a colon is a label. That includes the names of fields, as in
`!DILocation(line: 3)`. In `memory(argmem: read)` and a few other
attributes, the parser tells LLVM's lexer to read the name as a word
instead. We can't tell these apart the way the parser does, so we look
at where the label is. A label at the start of its line is a basic
block, unless it's inside parentheses. That's how a field looks in a
node that goes on over several lines. Any other label is a field, and
its name gets the colors it would have without the colon. So `line` and
`argmem` get the keyword color, and the `float` in
`denormal_fpenv(float: ieee)` gets the type color.

Comments start with `;`, or go between `/*` and `*/`. A string reads
up to the next quote, even on another line, and a backslash doesn't
stop a quote from ending it. The only escapes are `\\` and a backslash
followed by two hex digits, such as `\0A`. Quoted names such as
`@"a b"` are identifiers.

Numbers include floats written as their bits in hexadecimal, such as
`0x3FF0000000000000`. A letter after the `0x` gives the type, as in
`0xK4000C000000000000000`. Integers can be in hexadecimal too, as in
`u0xFF`, and floats can be `+inf`, `-qnan` or `+nan(0x8)`. Like LLVM,
the lexer reads a NaN's payload up to the `)`, even on another line. A
quoted label can go on over lines too.

`=`, `*` and `|` get the operator color. Other symbols get the
punctuation color.

### Python

`python.rs` follows CPython's tokenizer, `Parser/lexer/lexer.c`. Files
ending in `.py`, `.pyi` and `.pyw` are Python, and so are scripts that
start with a `#!` line that runs Python, and Markdown code blocks marked
`py`, `python` or `python3`.

Comments start with `#`. Strings can have a prefix such as `r`, `b` or
`rb`. Escapes such as `\n`, `\x7f` and `\N{DASH}` get the escape
color, except in raw strings. `\N`, `\u` and `\U` don't work in bytes,
so they are plain string there. A backslash before any other character
is plain string too. A string that isn't triple-quoted ends with its
line, unless a backslash joins the next line to it.

Since Python 3.12, the tokenizer splits the fields of an f-string into
tokens like any other code. A field can hold strings with the same
quotes as the f-string around it, and comments and line breaks, even
in an f-string that isn't triple-quoted. So the lexer keeps a stack of
the brackets and fields it's inside of, as the JavaScript lexer does
for template literals. The text of an f-string gets the string color,
and doubled braces such as `{{` get the escape color. The braces
around a field get the punctuation color, and the code in it gets the
usual colors. A `:` in a field, outside of any brackets, starts the
format spec, as in `{x:>10}`. That's true even in `{x:=5}`, where the
spec is `=5`. The spec is text, except that a `{` in it starts another
field. A conversion such as `!r` gets the operator color. T-strings,
which start with `t`, are lexed the same way.

`True`, `False` and `None` get the constant color. So do `...`,
`NotImplemented`, `Ellipsis` and `__debug__`, because Sublime Text
colors them that way. Other keywords get the keyword color. Every
other name is an identifier, including `self` and built-in functions
such as `print`. A name after `.` is always an identifier.

`match`, `case`, `type` and `lazy` are soft keywords. They are keywords
in some statements and names everywhere else. The lexer guesses which
from the other tokens in the statement. A statement goes on over
several lines when brackets or a backslash join them, and the lexer
looks at all of them.

- `match` and `case` are keywords at the start of a statement, when
  what follows them can start an expression or a pattern. That's a
  name, a literal, a bracket, `-`, `*` or `not`. Then the statement
  must end with a `:` outside of brackets, for `match`, or have one,
  for `case`, since a `case` can have its body on the same line. So
  `match x:` and `match(x):` have keywords, and `match = 1`,
  `match.group()` and `match(x)` don't.
- `type X = int` makes a type alias. Two names in a row are an error
  anywhere else, so `type` is a keyword when a name follows it.
- `lazy` is a keyword when `import` or `from` follows it, as in
  `lazy import json`. Python 3.15 added these lazy imports.

A decorator is a `@` at the start of a statement. The `@` and the
dotted name after it, as in `@functools.cache`, get the attribute
color. A decorator can be any expression, so a keyword ends the name,
as in `@False or f`. Any other `@` multiplies matrices and gets the
operator color.

The tokenizer stops a number where it can't go on, and so does the
lexer. So `1if` is the number `1` and the keyword `if`, and `0x1for`
is `0x1f` and `or`. Brackets, `,`, `;`, `:` and `.` get the punctuation
color. Other symbols, such as `->`, `:=` and `**=`, get the operator
color.

### Shell scripts

`sh.rs` follows the parser of bash 5.3, mostly `read_token`,
`read_token_word` and `parse_matched_pair` in `parse.y`. Files ending
in `.sh` and `.bash` are shell scripts, and so are bash's startup files
such as `.bashrc` and `.profile`, and the `PKGBUILD` and `APKBUILD`
files of Arch Linux and Alpine. So are scripts that start with a `#!`
line that runs sh, bash, dash, ash, ksh, mksh or zsh, and Markdown code
blocks marked `sh`, `bash` or `shell`. Scripts for POSIX sh, dash and ksh mostly use
a part of bash's syntax, so they get the same colors.

Whether a word is a reserved word, such as `if` or `done`, depends on
the tokens before it. So `echo if` has no keyword, and `case x in`
has two. Bash decides from the last two tokens and a few flags, such
as whether it's reading the patterns of a `case`, in
`special_case_tokens` and `CHECK_FOR_RESERVED_WORD`. `State` keeps the
same tokens and flags. A word with a quote, a backslash or a `$` in
it is never a reserved word. `{`, `}` and `!` are reserved words too,
but they get the punctuation and operator colors.

The name of a command, such as `grep` or `./configure`, is an
identifier when it's a plain word, like a function call in other
languages. Its arguments are plain. A parameter's name is an
identifier, with the `$` before it as punctuation. So are the names
in assignments such as `x=1` and `a[i]+=2`, the variable of a `for`
loop, and names in arithmetic. So `x` gets the same color in `x=1`,
`$x`, `${x:-y}` and `$((x + 1))`. Bash reads assignments before the
name of a command, and as arguments of builtins such as `local` and
`export`. The arguments of `local`, `export`, `declare`, `readonly`
and `typeset` that are names are identifiers too.

Quoted strings get the string color. In double quotes, a backslash
before `$`, `` ` ``, `"`, `\` or a line break is an escape, and other
backslashes are plain string. Outside of quotes, a backslash and the
character after it are an escape. In `$'...'`, escapes such as `\n`,
`\x41` and `\cA` get the escape color, as `ansicstr` in
`lib/sh/strtrans.c` reads them. Expansions inside double quotes keep
their colors. `$(...)`, backquotes and `<(...)` hold commands, which
are lexed like the rest of the file. Numbers only get a color in
arithmetic, and in redirections such as `2>&1`. Redirections and the
operators between commands, such as `|` and `&&`, get the operator
color, and `;`, `;;` and parentheses get the punctuation color.

The delimiter of a here-document and the line that ends its body get
the attribute color. The body gets the string color. If no part of
the delimiter is quoted, the body's expansions keep their colors, and
`\$`, `` \` `` and `\\` are escapes.

In `[[ ... ]]`, words such as `-f`, `==` and `=~` get the operator
color where `cond_term` expects an operator. So in `[[ -f == ]]`, the
`==` is the operand of `-f`. A regular expression after `=~` can hold
`(` and `|`.

Bash has some surprises that the lexer copies:

- `((` starts an arithmetic command, unless the `)` that matches the
  second `(` has no `)` right after it. Then it's a subshell inside a
  subshell. We only look for that `)` on the same line, and read
  `((` as arithmetic when it isn't there.
- Where a command starts, brackets after a name belong to the word
  even when no `=` follows, so `a[1 2]` is one word. The brackets can
  go on to later lines. We only color a subscript that ends on its
  line.
- The first `}` ends `${...}` unless it's quoted or in another
  expansion, so `${x:-{a}}` is `${x:-{a}` followed by a `}`. Inside
  `${...}`, `$'...'` is read even in double quotes.
- In `$((...))` and `$[...]`, bash only nests quotes and `$(...)`, so a
  `)` inside a `${...}` can end the expression. We read the `${` and
  `}` there as tokens of their own. In an extended pattern such as
  `@(a|b)`, and in the parentheses of a regular expression, only
  quotes nest.
- A here-document that is still open when its `$(...)` ends starts on
  the line after the `)`, before the here-documents outside. Inside
  `$(...)`, a line that starts with the delimiter and has a `)` later
  on it ends the body, as in `EOF)`. Bash keeps that for old scripts.
- Extended patterns such as `@(a|b)` are part of a word. Bash only
  reads them with the `extglob` option, and without it they're an
  error, so we always read them. The exception is `!(` where a command
  starts, which without `extglob` is a `!` before a subshell.

The colors of a line never depend on the lines after it. The lexer
only looks ahead on the current line, for `((` and for subscripts.
Expansions and quotes can nest, and each level takes some stack, so
beyond 64 levels the lexer reads them as plain text.

### Dockerfiles

`dockerfile.rs` follows BuildKit's Dockerfile parser in
`frontend/dockerfile/parser`, and the way BuildKit then reads the
arguments of each instruction. Files named `Dockerfile` or
`Containerfile`, variants such as `Dockerfile.dev` and `dev.Dockerfile`,
and files ending in `.dockerfile` or `.containerfile` are Dockerfiles.
So are Markdown code blocks marked `dockerfile`, `containerfile` or
`docker`.

A line that ends with the escape character goes on on the next one.
BuildKit skips comment lines and blank lines inside such an
instruction, even inside quotes. So the lexer first gathers the lines
of an instruction, colors its comment lines, and joins the rest with
line feeds. Then it lexes the joined text, and maps the colors back to
the lines they came from. The escape character is `\` unless an
`escape` directive at the top of the file makes it `` ` ``. BuildKit
only takes it as a line continuation when something other than the
escape character comes before it, so a line that ends in `\\` doesn't
go on. It also removes all carriage returns from the end of a line, and
all whitespace, even U+00A0, from the ends of an instruction.

Instructions such as `FROM` and `RUN` get the keyword color, and so do
`AS` in `FROM`, and `CMD` and `NONE` in `HEALTHCHECK`. Instructions
that BuildKit doesn't know are plain. Flags such as `--mount=...` get
the attribute color. The names that `ARG`, `ENV` and `LABEL` set are
identifiers, and so are variables such as `$x` and `${x:-y}`, so a
variable gets the same color where it's set, where the Dockerfile uses
it and where a shell command uses it. Images and stage names in `FROM`
and `--from` are identifiers too, so a stage gets the same color where
it's named and where it's used. Quotes, escapes and variables are read
the way BuildKit's `shell` package reads them, and ports in `EXPOSE`
get the number color. Parser directives such as
`# syntax=docker/dockerfile:1` get the attribute color, and only count
at the top of the file, before any other line.

`RUN`, `CMD`, `ENTRYPOINT` and `HEALTHCHECK CMD` hold commands. In the
shell form, they're lexed with the shell lexer. We give it the joined
lines with a `\` and a line feed between them, so that it reads them as
one command with line continuations. A command in the JSON form, such
as `CMD ["a", "b"]`, is lexed as JSON. BuildKit takes the arguments as
JSON when they read as a JSON array of strings, and as a shell command
otherwise, as in `RUN [ -f a ] && b`. We only look at the first line of
the array, and take it as JSON if it's fine up to the end of that line.
When the instruction ends on that line too, the array has to end there.
A `SHELL` instruction that names a shell outside the sh family, such as
`powershell` or `cmd`, makes the commands after it plain until the next
`FROM`. We can't tell whether an image is for Windows, where the shell
is `cmd`. But Dockerfiles for Windows almost always make `` ` `` the
escape character, so with that, commands are plain until a `SHELL`
names a shell in the sh family.

`RUN`, `COPY` and `ADD` can read here-documents. BuildKit splits the
whole instruction into words with its `shell` package, and takes each
word that starts with `<<`, such as `<<EOF`, `<<-EOF` or `<<"EOF"`, as
one. The package keeps the spaces after `<<` in the word, so
`cat << EOF` has a here-document, but `cat<<EOF` doesn't. Two quirks of
the package matter here, and we follow both. When it can't read the
instruction, for example because a quote isn't closed, BuildKit finds
no here-documents at all. Then we still color the words as
here-documents, but no lines become their bodies, so that the colors of
the instruction don't depend on its later lines. And at a `${` with an
operator, as in `<<EOF${x:-y}`, the package forgets the part of the
word before it, so that word doesn't start with `<<` any more.

The bodies start on the line after the instruction. They are colored as
follows:

- When a `RUN` is just one here-document, BuildKit runs its body as a
  script. We lex it as shell code, or in the language that its `#!`
  line names, such as Python.
- Otherwise the shell reads the bodies of a `RUN`'s here-documents, so
  the shell lexer gets them along with the command, and colors them as
  it does in a script.
- The bodies in a `COPY` or an `ADD` are the contents of files.
  BuildKit splits the arguments at blanks, and only copies a body for
  a word that is a here-document on its own. So in `COPY << EOF /x`,
  the body is lost, and `<<` and `EOF` are the names of files, which is
  how we color them. When there's one body, and the name of the file
  it goes to says what language it's in, as in
  `COPY <<EOF /app/main.py`, the body is lexed in that language.
  Otherwise it gets the string color. When the delimiter isn't quoted,
  BuildKit expands the variables in the body and removes the `\` from
  escapes such as `\$`, but leaves quotes alone. So the variables and
  escapes get their own colors.

The line that ends a body gets the attribute color, as in a shell
script. After `<<-`, BuildKit removes the tabs at the start of each
line of a body, and so do we before we lex it in another language.

BuildKit joins the lines of an instruction with nothing between them,
so a word can go on on the next line. We join them with a line feed, so
that a line's colors don't depend on the line after it. For the same
reason, a few decisions only look at the first line:
whether `ENV` uses its old form, `ENV name value`, depends on whether
its first word has an `=` on that line, and a delimiter with a quote
that goes on to the next line isn't one. Whether a `RUN` is just one
here-document can depend on later lines, so the shell lexer colors the
command either way, and only the body's colors depend on it.

### fish scripts

`fish.rs` follows fish 4.9: its tokenizer in `src/tokenizer.rs`, its
parser in `src/ast.rs`, and the way it expands words. Files ending in
`.fish` are fish scripts, and so are scripts with a `#!` line that runs
fish, and Markdown code blocks marked `fish`.

The tokenizer finds where a word ends by counting the parentheses,
braces and brackets in it, and by skipping its quotes. Inside them,
blanks, line feeds and operators don't end the word. So `(a; b)c` and
`{a, b}` are one word each. One that never closes takes the word to the
end of the text, as it does in fish. A `{` where a command starts opens
a block instead, as in `{ echo a; }`. The tokenizer decides where that
is, and it doesn't always agree with the parser, so we follow it.

Whether a word is a keyword depends on where the parser is, which
`Parser` keeps. `end`, `else` and `case` end a list of jobs, `and` and
`or` start a job, `time` starts a pipeline, and `not` and `!` start a
statement. `command`, `builtin` and `exec` decorate the command after
them. Fish reads a keyword without its quotes and escapes, so `'if'` is
the keyword `if`. Some keywords also depend on the token after them.
Before `-h` or `--help`, or at the end of the text, `if`, `while`,
`begin`, `function` and `switch` run the builtin of that name, which
prints its help. `for`, `not` and `!` do that before any word that
starts with `-`. `command`, `builtin` and `exec` only decorate a word
that doesn't start with `-`, and an assignment such as `a=1` needs a
word after it. `!` gets the operator color, and the other keywords the
keyword color.

The name of a command is an identifier when it's a plain word, as in
the shell lexer, and so is a variable's name after its `$`. The names
in assignments, the variable of `for`, the name of a function, the
variable that `set` sets and the names after `function -a` are
identifiers too. So `x` gets the same color in `set x 1`, `for x in`,
`$x` and `$x[1]`. The `$` and the brackets of a slice get the
punctuation color, and in the slice, numbers get the number color and
`..` the operator color.

In single quotes, only `\\` and `\'` are escapes. In double quotes,
`\\`, `\"`, `\$` and a backslash before a line feed are, and variables
and `$(...)` keep their colors. Outside of quotes, every backslash
escapes the character after it, and escapes such as `\x41`, `\u00e9`
and `\cA` are read the way fish's `unescape_one` reads them.
`(...)` and `$(...)` hold commands, which are lexed like the rest of the
file. In double quotes, a `$(...)` ends the string for a while, and the
string goes on after its `)`. The braces of a brace expansion and the
commas between its items get the punctuation color, but fish only
expands braces with a `,` or a variable right inside them, so `{}` and
`{a}` are plain. Redirections such as `2>`, `>?`, `&>` and `>&2`, and
pipes such as `|`, `&|` and `2>|`, get the operator color, with the
number of a file descriptor in the number color. With the
`ampersand-nobg-in-token` feature, which fish turns on by default, a
`&` right before more of a word is part of the word, as in `a&b`.

Fish has some surprises that the lexer copies:

- A `)` that closes nothing, or a `)` or `}` that closes the wrong
  bracket, is an error. Fish drops the part of the word before it, so
  that part is plain.
- Inside braces and brackets, a word can have blanks in it. Fish skips
  a comment after one when it looks for the end of the word and for
  command substitutions. But the comment is still part of the word, so
  its quotes and variables expand. A `}` in it without its `{` makes
  fish's count of braces drop below zero, and the braces after it in
  the word don't expand.
- Fish removes line continuations before it expands variables. So
  `$`, a backslash and a line feed, and then `x` on the next line is
  the variable `x`. A continuation can also split a name, or come
  between a name and its slice.
- After a line continuation, a comment line doesn't end the command.
  The words after it are still its arguments.
- The tokenizer stops at a null character, so nothing after one gets a
  color.

The colors of a line never depend on the lines after it. Where fish
looks at the token after a word, the lexer only looks at that token's
first line, and after a line continuation it assumes a word that
doesn't start with `-`. Fish reads a keyword that goes on over a line
continuation, such as `i` and a backslash on one line and `f` on the
next, but the lexer doesn't. A slice only gets its colors when its `]`
is on the same line as its `[`, and whether braces expand is decided
from the text up to the end of the `{`'s line. Command substitutions
can nest, and each level takes some stack, so beyond 64 levels the
lexer reads them as plain text.

### Rainbow identifiers

With `rainbow_identifiers` on, which is the default, each `Identifier`
token gets a color
picked from its name, so every use of a name has the same color and
different names usually don't. Drawing hashes the token's text and
uses the hash to pick one of the colors in `Theme::identifiers`.
The lexers only mark identifiers that get no other color, so keywords,
and words such as `AND` in a CMake `if`, keep their usual colors.

The C, C++, CMake, JavaScript, Python, Rust, shell, fish, Zig, LLVM IR,
Racket, Scribble and Rhombus lexers mark identifiers. The Dockerfile
lexer marks variables, the names that `ARG`, `ENV` and `LABEL` set,
and images and stages. The TOML, YAML and JSON
lexers mark the names in keys, so each key in a config file has the
same color everywhere it appears. Markdown and gitignore have nothing
that is really a name, so their lexers don't mark anything.

We make the colors in Oklab, where the distance between
two colors matches how different they look much better than in sRGB.
A name's color should be easy to tell apart from keywords, strings and
the other syntax colors, and from plain text. The colors should also
look alike apart from their hue. When some are brighter or more vivid
than others, those names catch the eye more, and the muted ones look
like text in the wrong color.

So `identifier_colors` gives them all the same lightness and chroma.
The lightness is the average of the theme's string, number, keyword,
type and function colors, so that the colors look like they belong to
the theme. The chroma is 80% of their average, so identifiers stand
out a little less than other tokens, but at least 0.08. Nord's colors
are so muted that its hues would be hard to tell apart without that
floor. Identifiers are most of the text in code. So if some of the
colors would be harder to read than the text, they all move toward
the text's lightness together until they are about as easy to read.

There are 16 hues, spread evenly around the color wheel. Some of them
always land near a hue the theme already uses, so we leave out every
color within 0.04 of a syntax color or the text color. We turn the
wheel to the angle that leaves out the fewest. A theme ends up with
between 11 and 16 colors. Catppuccin, which uses almost every hue for
something, has the fewest.

Finding the colors takes a lot of arithmetic, so the compiler does it.
`identifier_colors` and everything it calls are `const fn`s, and the
static `IDENTIFIERS` holds the colors for each palette. The standard
library can't take square roots, powers or sines in a `const fn` yet,
so `theme.rs` has its own. It converts between sRGB and linear values
with tables, because the compiler runs const code slowly. Even so,
working out the colors adds over a second to a build after `theme.rs`
changes. The compiler also refuses to spend more than about two
million steps on one constant, so each palette gets its own `const`
block.

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
- `tests/ops.rs` has `a_stash_shows_its_untracked_files`,
  `an_empty_file_can_be_renamed` and
  `a_shallow_commit_shows_as_a_root`.
  `commit_loads_keep_git_running` checks that the running git sees new
  commits, and sees a config change after `stop_processes`.
  `a_dropped_commit_load_does_not_confuse_the_next` drops loads at
  different points and checks that the loads after them are right.
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
- `tests/ui.rs` has
  `the_detail_pane_keeps_its_old_contents_while_a_commit_loads`, which
  checks that the old contents stay during the grace period and fade
  after it.
- `tests/ui.rs` has `folds_do_not_carry_over_to_other_commits`,
  `folds_are_saved_for_each_commit_and_section` and
  `a_large_commit_remembers_the_files_unfolded_in_it`. `src/state.rs`
  checks that the store keeps folds apart by repository, section and
  commit.
- `src/git/context.rs` has examples of hunks that grow, shrink and
  join, and a property test that views from random choices keep git's
  hunks whole and in order, show every change, still turn the old text
  into the new one, and never touch. Without choices, a view shows git's
  hunks as they are.
- `tests/hunk_edges.rs` moves edges with the keys and the mouse in the
  whole app, joins two hunks and stages them, stages a hunk from a line
  the texts gave, and checks that the choices survive a reload and go
  with a reset.
- Each lexer that marks identifiers has a test called
  `marks_plain_identifiers`. The other lexer tests leave identifiers
  out of the tokens they compare, which keeps them readable.
- `src/theme.rs` checks that identifier colors differ from each other,
  from the other syntax colors and from the text color, that they are
  about as easy to read as the text and differ only in hue, and that
  a name always
  gets the same color. `tests/ui.rs` has
  `identifiers_can_get_colors_by_name`.
- `src/highlight/rust.rs` has examples for each kind of Rust token,
  such as nested comments, raw strings, escapes, characters next to
  lifetimes, and characters and escapes that rustc rejects.
  `dev/reference/rust` runs rustc's lexer, as rust-analyzer publishes
  it, over the 39,044 `.rs` files in Rust 1.100.0's source and over
  random texts. It checks the color of each byte, escapes included,
  and where each identifier starts and ends. The lexer agrees with it
  on all of them, and on 600,000 random texts when we added the check.
  The check skips the frontmatter of cargo scripts. Before the check,
  the lexer split floats such as `1.` in two and only knew some
  suffixes, so `1f16` was a number and a name. It also read shebang
  lines as code, and read characters and escapes that rustc rejects
  differently, which could change the colors of the rest of the line.
- `src/highlight/zig.rs` has examples for each kind of Zig token, such
  as multiline strings, quoted identifiers, escapes, and numbers that
  only the compiler rejects. `dev/reference/zig` runs the tokenizer
  from Zig 0.17.1 over the 887 `.zig` and `.zon` files in its own `lib`
  directory, and checks the color of each byte and where each
  identifier starts and ends. The lexer agrees with it on all of them.
- `src/highlight/llvm.rs` has examples for each kind of LLVM IR token,
  such as names, labels, fields, strings and floats in hexadecimal.
  `dev/reference/llvm` builds a program that runs `LLLexer` from LLVM
  23.1.6 over the 44,904 `.ll` files in LLVM's own `test` directory.
  It gives each byte the color that its token calls for, and checks
  that against the lexer's colors and where each identifier starts and
  ends. LLVM's lexer reports errors in 575 of the files, which test
  errors or are templates that the tests fill in. The lexer agrees
  with it on every byte of the other 44,329.
- `src/highlight/python.rs` has examples for each kind of Python
  token, such as strings with prefixes, f-strings with fields and
  format specs, and soft keywords. `dev/reference/python` runs the
  tokenizer from Python 3.15.2 over 6,657 `.py` files from CPython's
  source, Django, NumPy, pandas, requests and SymPy. It checks whether
  each character is in a comment, a string, a number, a name or an
  operator, and where each name starts and ends. It also checks each
  `match`, `case`, `type` and `lazy` against the statements that
  Python's parser finds. The lexer agrees on all of them, except two
  of CPython's tests of how it handles errors.
- `src/highlight/racket.rs` has examples for each kind of Racket token,
  such as `#;` comments, here strings, characters, and atoms that are
  numbers and atoms that aren't. It also has examples of @-forms in
  Scribble and `#lang at-exp`, with other delimiters, escapes and
  comments.
- `dev/reference/racket` checks the lexer against Racket 9.3's reader,
  and against a copy of the @-reader that marks where each piece of
  text came from. It reads random atoms, random texts in plain Racket,
  `#lang at-exp` and Scribble, and the Scribble, `at-exp` and Racket
  files in Racket's own source. Every number, string, character,
  boolean and keyword must get its color, symbols and text must get
  none, and comments must cover nothing that the reader read. An atom
  must get the number color exactly when Racket reads it as a number.
  When we wrote the lexer, this agreed on about 240,000 random atoms
  and 320,000 random texts. The only differences are here strings,
  where Racket counts the linefeed after the terminator and the lexer
  doesn't, which changes no colors.
- `src/highlight/shrubbery.rs` has examples for each kind of shrubbery
  token, such as numbers and operators after a term, group comments,
  quotes, Racket data, @-forms and Rhombus Scribble text.
  `dev/reference/shrubbery` checks each character's color against
  DrRacket's lexer on random texts and on the 1,069 Rhombus files in
  the Rhombus repository, and they agree on all of them. When we wrote
  the lexer, they agreed on about 90,000 random texts. The check
  leaves out random texts that make the real lexer fail, for example
  on a `²` followed by a letter, which no rule of its lexer matches.
- `src/highlight/c.rs` has examples for each kind of C and C++ token,
  such as line splices, raw strings, digit separators, header names
  and `#if 0`. `dev/reference/c` compares it with clang's raw lexer
  (`clang -cc1 -dump-raw-tokens`) on random texts, each read as C23 and
  as C++23. It checks whether each character is in a comment, a
  string, a number or none of these. They only differ on splices
  inside a token, on raw string delimiters that hold a quote, on how
  clang recovers from errors such as a bad delimiter or a stray `\`
  before a `u`, and on numbers like `1p-1`. In C++, clang only
  reads that as one number when it's hexadecimal, but the standard
  says it's always one.
- `src/highlight/cmake.rs` has examples for comments, brackets,
  command names, `if` operators, escapes, variable references,
  generator expressions and the arguments that old versions allowed.
  `dev/reference/cmake` builds `cmListFileLexer.c` from CMake's source
  into a program that prints its tokens, and follows the steps of
  CMake's parser on them. It checks that each byte has the color its
  token calls for, on random texts and on the 12,885 CMake files in
  CMake's own source. For escapes and variable references, it runs
  `message("…")` with `cmake -P` on random arguments, some of them
  over several lines. With no variables defined, the colors say
  whether CMake gives an error, and what it prints if it doesn't. They
  agreed on all of them, on 25,000 random texts and 15,000 arguments
  when we wrote the lexer.
- `src/highlight/sh.rs` has examples for each kind of shell token,
  such as reserved words in and out of place, quotes and escapes,
  parameter expansions, command substitutions, arithmetic,
  here-documents, assignments, redirections, `case` patterns and
  conditional commands. `dev/reference/sh` builds bash 5.3 with a
  patch that makes its parser report where each token, comment and
  here-document is. It parses bash's own tests and examples, git's
  shell scripts, bash-completion and random texts with `bash -n`. It
  checks that reserved words, operators, comments, the names in
  assignments, arithmetic commands and the lines that end
  here-documents get their colors, and that nothing else gets the
  keyword or comment color. Bash only parses what's inside backquotes
  and a here-document's body when it runs them, so the check skips
  those. It also skips files where a line continuation joins two
  parts of a word, because bash's positions slip there and the lexer
  ends a plain word at a continuation. When we wrote the lexer, it
  agreed on all 2,310 scripts and on about 73,000 random texts.
- `src/highlight/dockerfile.rs` has examples for each instruction,
  directives, line continuations with comment lines inside them, the
  shell and JSON forms of commands, `SHELL`, variables, stages and
  here-documents in `RUN` and `COPY`. Its property test builds random
  Dockerfiles from instructions, flags, words, line continuations,
  comments and here-documents. It checks that cutting the text after
  any line doesn't change the colors of the lines before it. That
  found the shell lexer's problem with a line of a here-document that
  ends in `\`, which `todo/bugs.md` describes. The test leaves those
  lines out until it's fixed. `dev/reference/dockerfile` checks the
  lexer against BuildKit 0.35.1's parser on the 440 Dockerfiles in
  BuildKit's source and a few other projects, and on random texts.
  BuildKit's parser only says which lines each instruction and
  here-document covers, so the check follows its steps to find which
  bytes it read as what. It checks the colors of directives, comments,
  escape characters that join lines, instructions, flags, the ends of
  here-documents and JSON arrays. It also checks that the variables in
  each word that BuildKit expands are the ones that BuildKit's `shell`
  package finds there. The check skips what follows from the known
  differences above, such as a word that goes on over a line, and the
  ends of here-documents that the shell lexer reads as the shell
  would, for example after a `#` that starts a comment or when bash
  can't parse the command. When we added the check, the lexer took a
  line ending in `\\` as one that goes on, and didn't see `<< EOF`. It
  read flags with `\` as the escape character even when the file chose
  `` ` ``, and colored `\$x` in a `COPY` body as a variable. The check
  also found here-documents where the `shell` package fails or forgets
  the start of a word. After fixing those, it agreed on all the
  Dockerfiles and on about 340,000 random texts.
- `src/highlight/fish.rs` has examples for keywords in and out of
  place, `and`, `or`, `not` and `time`, the names that `set`, `for`
  and `function` take, comments, quotes and escapes, variables and
  slices, command substitutions, brace expansions, redirections and
  pipes. `dev/reference/fish` builds a program inside fish 4.9.3's
  source that uses fish's own parser and highlighter. It runs on the
  1,594 `.fish` files in fish's source and on random texts. It takes
  the colors of keywords, operators, statement braces, redirections and
  plain command names from fish's parse tree. It takes the colors
  inside each word from fish's highlighter, with corrections where the
  highlighter doesn't do what fish does, such as quotes in a comment
  inside a command substitution. It also checks that the identifiers
  are the same, in order. It stops comparing a file at fish's first
  parse error, and at the known differences above, such as a keyword
  that goes on over a line continuation. When we added the check, the
  lexer missed a variable name after a line continuation, or after
  several, and a slice after one. It let a `#` inside braces hide
  variables and quotes, miscounted a stray `}`, read `$(` in a comment
  inside braces, and took `0&|` as a pipe from file descriptor 0.
  After fixing those, it agreed on all the files and on 200,000 random
  texts.
- `src/highlight/javascript.rs` has examples for each kind of
  JavaScript token, and a list of places where a `/` divides or starts
  a regular expression. `dev/reference/javascript` parses the 6,513
  JavaScript files in some popular npm packages, such as lodash,
  three, d3, webpack and prettier, with acorn. It checks whether each
  character is in a comment, a string, a template literal, a regular
  expression or a number. The lexer agrees with acorn on all of them.
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
  key is an identifier, apart from its quotes.
- `src/highlight/yaml.rs` has examples for each kind of YAML token,
  such as keys that look like values, plain scalars over several
  lines, escapes, block scalars with indicators, and tags. Its property
  test builds random streams of documents from nested block
  collections, flow collections, scalars of every style, block
  scalars, anchors, aliases, comments and directives. It reads them
  with `saphyr-parser`, which passes the YAML test suite, and checks
  each scalar against the parser's events. Keys must be identifiers,
  apart from their quotes, other plain scalars must have the color of their type, other scalars
  the string or escape color, aliases the attribute color, and nothing
  outside of a scalar may be a string, a number or a constant. It
  agreed on a million streams while we wrote the lexer. We also ran
  the same check on the inputs of the YAML test suite and on the YAML
  files of Ansible, the Kubernetes examples and the OpenAPI
  specification, and they agreed on all of them. `dev/reference/yaml`
  runs that check on the 2,985 of those files that saphyr-parser reads.
- `src/highlight/markdown.rs` has examples for each kind of Markdown
  block and inline construct. Its property test builds random documents
  from nested block quotes, lists, tables, code blocks, HTML and text
  full of code spans, links and tags. It parses them with
  `pulldown-cmark` and checks that code spans and code blocks have the
  string color, headings the keyword color, rules and task markers the
  punctuation color, and so do the brackets of links. Text is plain
  and raw HTML isn't. It checks every line of a code span, tag or
  heading that covers several. They agreed on about 1.2 million
  documents while we wrote the lexer, and on 200,000 more once it lexed
  whole paragraphs and looked up labels. The generator defines some of
  the link labels that its pieces can make and leaves out others, and
  it ends each table with a blank line. After a table, pulldown-cmark
  reads an indented line as code if it has a pipe, and as a row if it
  doesn't. We always read it as a row.
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

- Folds are kept by path. A file that is renamed after it was folded in
  the working directory starts out unfolded under its new name. The
  folds of a repository that moves to another folder are lost, as its
  hidden branches are.
- A merge commit is diffed against its first parent only.
- Only C, C++, CMake, JavaScript, JSON, Python, Rust, shell scripts,
  fish scripts, Dockerfiles, Zig, LLVM IR, Racket, Rhombus, Scribble,
  TOML, YAML, Markdown and `.gitignore` files are highlighted. That leaves out JSX, TypeScript
  and zsh.
- The JavaScript lexer can guess wrong about a `/`. In a statement
  such as `a ? b : {} / 2`, it reads the `{}` as a block and the `/`
  as the start of a regular expression.
- A hunk from the middle of a JavaScript template literal that we
  don't have the full text for reads the template's text as code.
- The JavaScript lexer reads `<!--` and `-->`, which start comments
  in old scripts, as operators.
- The frontmatter of a cargo script, which is still unstable in Rust,
  is lexed as Rust code.
- In Markdown, named entities such as `&foo;` get the escape color
  whether or not HTML defines them.
- A hunk from the middle of a multi-line TOML array that we don't have
  the full text for reads its values as keys. Strings keep their
  color, but numbers, `true` and `false` are plain. A
  nested array that starts a line there, as in `[1, 2],`, gets the
  color of a table header.
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
- Python only reads `case` as a keyword in the body of a `match`
  statement. The lexer doesn't keep track of those, so it reads the
  annotation `case[x]: int` as the start of a `case`.
- A hunk from the middle of a shell here-document, or of a quoted
  string over several lines, that we don't have the full text for
  reads the text as commands.
- A hunk from the middle of a Dockerfile instruction that goes on over
  several lines, or from a here-document's body, that we don't have
  the full text for reads each line as an instruction. Lines such as
  `&& make` don't start with one, so they're plain.
- A Dockerfile's colors come from its own instructions. A `SHELL` in
  the base image, or a Windows image without `` ` `` as the escape
  character, can make `RUN` use a shell other than the one we assume.
- A Dockerfile command in the JSON form whose first line reads as JSON
  but whose later lines don't is still lexed as JSON. BuildKit reads it
  as a shell command.
- Bash expands aliases while it reads, and an alias can change how
  the rest of a line reads. The shell lexer doesn't know them.
- Bash removes a line continuation before it reads words, so a word
  can go on on the next line. The shell lexer ends a plain word at a
  continuation, as in `then\`, so that its color doesn't depend on the
  next line. When that line starts with something other than a blank,
  bash reads one longer word. In the same way, the lexer reads `>` at
  the end of a line and `&` at the start of the next as two operators,
  where bash reads `>&`.
- A shell script that turns on `extglob` can run a command whose name
  is a pattern such as `!(a)`. The lexer reads `!` and a subshell
  there.
- Where fish decides what a word is from the token after it, the fish
  lexer only looks at that token's first line. After a line
  continuation, it assumes a word that doesn't start with `-`. So in
  `exec` followed by a backslash, and `-c x` on the next line, it takes
  `exec` as a keyword, where fish runs the builtin `exec`. A word that
  only fails on a later line, such as a `(` whose word ends in an
  error there, can also change what fish makes of the word before it.
- Fish reads a keyword that goes on over a line continuation, but the
  fish lexer doesn't, so the words after it can read differently.
- A fish slice whose `]` is on a later line than its `[` gets no
  colors. Braces whose `,` only comes on a later line don't get the
  colors of an expansion.
- A hunk from the middle of a fish string, command substitution or
  brace expansion over several lines that we don't have the full text
  for reads the text as commands.
- Other Racket languages with a reader of their own, such as
  `#lang datalog`, are lexed the way the file's name suggests. In a
  `.rkt` file, that's the default reader. `#reader scribble/reader`
  always gets the default reader.
- Rainbow identifiers only look at the name. Two different variables
  with the same name get the same color. With 16 colors or fewer, two
  different names often share one too.
- A hunk that grew upwards keeps the section name git gave it, even
  when the line that name came from is now inside the hunk. We don't
  know which line git took it from, or what `diff.<driver>.xfuncname`
  would pick for the new start.
- The choices for moved hunk edges name lines by number. When a file
  in the working directory gets lines inserted or removed above a moved
  edge, the choice stays on the old number, and the edge shows up in
  another place.
- Word marks pair removed and added lines in order. When lines were
  inserted between edited ones, a line can be paired with the wrong
  partner, and then gets no marks or odd ones.
