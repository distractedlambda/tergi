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

`diff_texts` splits both sides into lines, runs imara-diff's histogram
algorithm with its line post-processing, and groups the changes into
hunks with `context` lines around them. Two changes share a hunk when
the gap between them is at most twice the context, which is what git
does. The hunks have the same ranges and line numbers that `git diff`
would print. A property test, `hunks_turn_old_into_new`, checks that
applying the hunks to the old text gives the new text.

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
contents as they are.

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
syntect with the syntaxes and themes from two-face.

The worker only highlights files that appear on screen.
`request_visible_highlights` runs while drawing and asks for every file
with a row on screen that hasn't been asked for yet. Each job carries the
file's hunks and, when we have it, the full text of each side.

With the full text, the worker highlights the whole file, up to the last
line the diff shows. That way a line in the middle of a multi-line string
or comment gets the right colors. Then it picks out the lines the diff
shows by their line numbers. Without the full text, or for a file over
20,000 lines, it highlights each hunk's old side and new side on its
own. Context lines use the new side's colors when it has them.

We have the full text of both sides of every diff. A commit diff has
them from gitoxide. For an unstaged file, the old side is in the index
and the new side is on disk. For a staged file, the old side is in HEAD
and the new side is in the index. `Workdir::load` reads the index and
HEAD versions with `read_blobs`, which runs `git cat-file` twice: once
with `--batch-check` to skip anything over `MAX_BLOB`, and once with
`--batch` to read the rest.

The syntax is chosen by file name, then extension, then the first line,
such as a `#!` line. Files that come out as plain text aren't
highlighted.

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
- `tests/ops.rs` has `a_stash_shows_its_untracked_files`.
- `tests/staging.rs` checks the texts that `Workdir::load` reads for
  staged and unstaged files.
- `src/git/words.rs` has property tests that the words cover the whole
  line, and that the unmarked text on both sides is the same.
- `tests/ui.rs` has `folds_do_not_carry_over_to_other_commits`.

## Known gaps

- A merge commit is diffed against its first parent only.
- Word marks pair removed and added lines in order. When lines were
  inserted between edited ones, a line can be paired with the wrong
  partner, and then gets no marks or odd ones.
