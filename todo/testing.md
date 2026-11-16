# Randomized tests to write

Ideas for more property tests, best first.

These already have property tests: graph lanes (`git/graph.rs`), text
width (`ui/text.rs`), the text area (`ui/textarea.rs`), word marks
(`git/words.rs`), the imara hunks (`git/commit.rs`), diff parsing and
path quoting (`git/diff.rs`), status parsing (`git/status.rs`), dates
and fuzzy matching (`util.rs`), staging, unstaging and discarding,
both in a single step and in sequences of steps over several files
(`tests/patch_props.rs`), rewriting history through the planner
(`tests/rebase_props.rs`), odd file names with the real git
(`tests/path_props.rs`), and commit diffs against `git diff-tree`
(`tests/commit_props.rs`).

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
