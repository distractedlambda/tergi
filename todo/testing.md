# Randomized tests to write

Ideas for more property tests, best first. The first two cover the code
that can lose someone's work, so start there.

These already have property tests: graph lanes (`git/graph.rs`), text
width (`ui/text.rs`), the text area (`ui/textarea.rs`), word marks
(`git/words.rs`), the imara hunks (`git/commit.rs`), diff parsing and
path quoting (`git/diff.rs`), status parsing (`git/status.rs`), dates
and fuzzy matching (`util.rs`), and staging, unstaging or discarding
lines in a single step (`tests/patch_props.rs`).

## Rewriting history

The planner and `rebase::run` rewrite commits, and the number of ways to
combine actions is large. Nothing tests them with random input yet. The
test would work like `tests/patch_props.rs`:

1. Make a straight history of N commits. Commit *k* only touches
   `f<k>.txt`, so no reordering can conflict.
2. Make a `Planner` from `plan_from`. Apply random `move_row` and
   `move_selected` calls and random actions. Give rewords and squashes
   messages with `#` lines, blank lines, quotes and Unicode.
3. If `to_plan` accepts the plan, run `rebase::run`. Compare `git log`
   with a model that knows each new commit's message, which files it
   touches, and the final tree. Check that `.git/tergi/` is gone.
4. If `to_plan` refuses the plan, check that the model also says it is
   invalid.

This covers a fixup after a reword, chains of squashes, a squash or
fixup that ends up as the oldest commit, and melding into a dropped
commit. The model for messages: a pick keeps its message, a reword or
squash uses the new one after whitespace cleanup, and a fixup keeps
whatever the commit it melds into ended up with.

A cheap pure test can sit next to it in `app/planner.rs`. Any sequence
of moves leaves a permutation of the rows. `build_todo` accepts every
plan that `to_plan` accepts.

## Staging in several steps

`tests/patch_props.rs` does one operation on a file that has only one
kind of change. A stateful version would run a random sequence of
stage, unstage and discard over several files. Each step would pick
lines, a hunk or a whole file, and reload `Workdir` before the next
step. A model tracks HEAD, the index and the worktree.

This reaches cases the single step can't:

- Files that are both staged and modified (`MM` in the status).
- Discarding unstaged lines while the file also has staged changes. The
  base is then the index, not HEAD.
- Unstaging part of a file that an earlier step only partly staged.
- The hunk and file paths in `git/ops.rs`, not only `apply_lines`.

## Odd file names with the real git

The current path tests only read quoting that the tests produce
themselves. Run real git on random names instead. Use spaces, `"`, `\`,
tabs, newlines, ` b/` and `->` inside names, a leading `-`, trailing
spaces, and non-ASCII text. Add, edit, delete and rename files, change
modes, and include binary files and symlinks. Then check that:

- every file in `Workdir` has the path that `git status -z` reports,
- each file's `FileTexts` matches the real blob,
- staging one line in each file works,
- the paths in `commit::load` match `git diff-tree -z`.

The second check matters because `read_blobs` in `git/workdir.rs`
skips names that contain a newline. If that skipping goes wrong, the
files after such a name get the wrong text.

## Commit diffs against git

`commit::load` diffs trees with gix, not with git. Build random pairs
of trees and compare its file list, statuses and binary flags with
`git diff-tree -r -z`. Include files that turn into directories and
back, symlinks, empty files, binary files, and stashes with untracked
files. Then check that applying each file's hunks to the parent blob
gives the child blob. The existing test in `git/commit.rs` only checks
this for one pair of texts at a time.

## Rows of the detail pane

This one is pure and fast. Build a `Detail` from random diffs, in both
unified and split mode, and check that:

- every changed line appears in exactly one row, in order,
- selecting a whole hunk gives exactly its changed lines,
- calling `rebuild` again with the same data leaves the cursor where it
  was,
- after files disappear, the cursor is still in range, and it stays on
  the same path if that file is still there.

The code is in `app/doc.rs`.

## Smaller ones

**Dates never panic.** This one is a real bug. In debug builds these
calls panic with "attempt to add with overflow" or "attempt to subtract
with overflow":

- `format_date(i64::MAX - 100, 50400)`, at `secs + i64::from(offset)`
  in `util.rs`
- `format_date(i64::MIN, -43200)`, at the same place
- `short_date(i64::MIN, now)`, at `now - secs`

Dev builds use `panic = "abort"`, so a commit with a crafted timestamp
would crash a debug build. Release builds wrap instead. Fix it with
saturating or checked arithmetic. Then add a test that calls
`format_date`, `format_day` and `short_date` with any `i64` and `i32`.

**The history walk against `git rev-list`.** Make random histories with
merges, several branches, and tags, including tags that point at trees.
Check that `log::walk` returns the same commits as
`git rev-list --all`, that children come before their parents, and
that the parents match.

**Saving the config.** Start from a random TOML file with comments,
other keys and tables. Save random changes with `Config::save`. Check
that parsing the result gives the new config, and that everything else
in the file is still there.

**`shell_quote` through a real shell.** Run the quoted string through
`sh` and check that it gives back the original. This would also show a
problem in `build_todo` in `git/rebase.rs`. If the path to `.git`
contains a newline, the quoted `-F` argument of an `exec` line spans
two lines of the todo list, and the rebase fails. That is unlikely in
practice, but cheap to cover.
