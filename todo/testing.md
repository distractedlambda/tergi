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
(`tests/commit_props.rs`), rebases that stop on conflicts and edit
steps, along with the single commit operations
(`tests/rebase_stop_props.rs`), the history walk against `git rev-list`
(`tests/log_props.rs`), refs, the sidebar and labels against git
(`tests/refs_props.rs`), random sessions in the UI
(`tests/ui_props.rs`), the file watcher (`tests/watch_props.rs`), the
rows of the detail pane (`app/doc.rs`), stash actions by id
(`tests/stash_props.rs`), taking a side in each kind of conflict
(`tests/conflict_props.rs`), and syntax highlighting
(`highlight.rs`). That last test leaves JavaScript out until the hang
in `todo/bugs.md` is fixed.
The tests that use the real git run each case with random git settings
from `tests/common/settings.rs`.

## Smaller ones

**Too large files.** Load a working directory with a random small line
limit. For each file marked `too_large`, `load_full_diff` or
`load_full_untracked` should give the same diff as loading everything
without a limit.

**The sequence editor.** Give `seq_editor_main` our todo list and one
that looks like git's, with abbreviated ids, short verbs, comment lines
with other comment characters, blank lines and `update-ref` lines. It
should accept them when they name the same commits and refuse them
otherwise, including when one list repeats a commit.

**Saving the config.** Start from a random TOML file with comments,
other keys and tables. Save random changes with `Config::save`. Check
that parsing the result gives the new config, and that everything else
in the file is still there.

**`shell_quote` with random bytes.** `quoting_survives_the_shell` in
`git/cli.rs` checks a fixed list of words. Run random byte strings
without NUL through the same check.
