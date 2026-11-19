# Diffs and the detail pane

The detail pane on the right shows either a commit, with its message and
full diff, or the working directory, with its staged, unstaged,
untracked and conflicted files. This document explains where the diffs
come from, how the pane turns them into rows, and how word marks and
syntax colors are added.

The diff model and the parser for `git diff` output are in
`src/git/diff.rs`. Commit diffs are in `src/git/commit.rs`, word marks in
`src/git/words.rs`, and syntax highlighting in `src/highlight.rs`. The
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
in the background, in a worker that `Highlighter::new` returns, using
tree-sitter and the parsers and highlight queries from each language's
grammar crate.

The worker only highlights files that appear on screen.
`request_visible_highlights` runs while drawing and asks for every file
with a row on screen that hasn't been asked for yet. Each job carries the
file's hunks and, when we have it, the full text of each side.

Tree-sitter parses a whole text at once. Text further down can change
how a line reads, and there is no state to pick up partway through. So
with the full text, the worker parses all of it and goes through the
highlights up to the last line the diff shows. Then it picks out the
lines the diff shows by their line numbers. Without the full text, it
highlights each hunk's old side and new side as a text of its own.
Tree-sitter makes what it can of such a fragment. Context lines use the
new side's colors when they have them.

Parsing is fast enough that there is no size limit beyond `MAX_BLOB`. A
4 MB file takes between a fifth and half a second in a release build.
Markdown, which parses its inline text and code blocks separately,
takes about a second. A job that runs over five seconds is stopped, and
so is one whose generation is no longer current (see below). Both work
through tree-sitter's cancellation flag, which the worker sets while the
job runs on a blocking thread.

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

### Languages

`src/highlight/languages.rs` lists the languages. Each one has its
names, extensions, whole file names such as `Makefile`, and the programs
a `#!` line can name. A file's language is found by its name, then its
extension, then its `#!` line. Files in other languages aren't
highlighted.

Each grammar crate ships the queries its authors wrote for
`tree-sitter highlight`. We use them as they are, with a few
exceptions:

- TypeScript and TSX build on the JavaScript queries, and C++ on the C
  ones. As in the grammars' `tree-sitter.json`, the base queries come
  first, because the last pattern that matches a node wins.
- The PHP crate leaves out the query that parses the HTML around the
  code, so we add it.
- In the Markdown queries we take out two patterns that hide the colors
  of code blocks. We also hand the grammars that Markdown injects the
  punctuation that the block grammar keeps as separate nodes, or they
  would parse code with its `=` and `;` cut out. The comments on
  `markdown_highlights` and `markdown_injections` explain more.

Injections highlight one language inside another, such as the script in
an HTML page or a code block in Markdown. The worker finds the injected
language by name with `languages::named`, which also accepts
extensions, so a code block marked `py` works.

Compiling a language's queries takes a while, so it happens the first
time a file in that language is highlighted.

### Colors

The queries give each piece of code a name, such as `keyword`,
`string.escape` or `function.method`. Older queries use older names,
such as `conditional` or `text.title`. `NAMES` in `src/highlight/mod.rs`
lists the names we recognize and the name each one goes by in themes.
`config-and-themes.md` explains where each theme's colors come from.

`Colors::new` turns a theme into a color for each of those names. A
name the theme doesn't list takes the color of the longest name it does
list that starts it, so `keyword` colors `keyword.return`. With no such
name, a piece of code takes the color of the highlight around it, such
as the string around an escape, or else the theme's plain foreground.
`none` and `embedded` reset to the foreground. Queries use them for the
code inside a template string, for example.

The worker turns tree-sitter's stream of start, end and text events into
colored byte ranges for each line, with `Lines`. The ranges cover each
line with no gaps, and one byte more for its newline, even on a last
line that has none.

### Dropping stale results

Highlights are keyed by section and file index, and those indexes mean
something else after a reload. So `Detail` has a `Generation` that the
highlighter shares. It moves on whenever the files change or the theme
changes. Jobs from an older generation are skipped before the worker
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
- `src/highlight/mod.rs` has property tests that every line of random
  texts in a dozen languages gets colors from its start to past its end,
  that asking for fewer lines gives the same colors for those lines,
  and that every line of a diff gets the colors of the side it comes
  from. Other tests check a few colors, Markdown code blocks,
  cancelling, and lines that used to hang syntect.
- `src/highlight/languages.rs` checks that every language's queries
  compile, how files are matched to languages, and that the Markdown
  patterns we change are still there.

## Known gaps

- A merge commit is diffed against its first parent only.
- Word marks pair removed and added lines in order. When lines were
  inserted between edited ones, a line can be paired with the wrong
  partner, and then gets no marks or odd ones.
