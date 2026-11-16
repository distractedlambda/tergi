# Randomized tests to write

Ideas for more property tests, best first.

These already have property tests: graph lanes (`git/graph.rs`), text
width (`ui/text.rs`), the text area (`ui/textarea.rs`), word marks
(`git/words.rs`), the imara hunks (`git/commit.rs`), diff parsing and
path quoting (`git/diff.rs`), status parsing (`git/status.rs`), dates,
including extreme ones, and fuzzy matching (`util.rs`), staging,
unstaging and discarding, both in a single step and in sequences of
steps over several files (`tests/patch_props.rs`), rewriting history
through the planner (`tests/rebase_props.rs`), odd file names with the
real git (`tests/path_props.rs`), commit diffs against `git diff-tree`
(`tests/commit_props.rs`), the history walk against `git rev-list`
(`tests/log_props.rs`), and the rows of the detail pane (`app/doc.rs`).

## Smaller ones

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
