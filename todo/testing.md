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

## Random git settings

Everything we run through `git/cli.rs` reads the user's config, and so
does gix. The tests run with the dev image's config, so they never see
settings that change git's output. Give each case of the tests that use
the real git a random set of settings, written to the scratch
repository's config. Good ones to pick from are `diff.mnemonicPrefix`,
`diff.noprefix`, `diff.renames`, `diff.algorithm`, `diff.context`,
`diff.suppressBlankEmpty`, `color.diff = always`, `core.autocrlf`,
`core.commentChar`, `commit.cleanup`, `commit.verbose`,
`status.showUntrackedFiles`, `status.relativePaths`,
`rebase.abbreviateCommands`, `rebase.instructionFormat`,
`rebase.updateRefs`, `log.showSignature` and `i18n.logOutputEncoding`.
The checks stay the same, because tergi should behave the same with any
of them. `diff.mnemonicPrefix` alone already breaks the working
directory diffs, which `todo/bugs.md` describes.

The easiest start is a helper in `tests/common` that `Scratch` calls,
so `patch_props`, `path_props`, `commit_props`, `log_props` and
`rebase_props` all pick it up.

## Refs against `git for-each-ref`

`log_props` loads refs, but it only checks the tips. Build random refs
instead: branches with and without upstreams, upstreams that are ahead,
behind, both or gone, several remotes, remotes and branches whose names
hold slashes or bytes that aren't UTF-8, annotated and nested tags, and
a few stashes. Then check `Refs::load`, `sidebar_rows` and
`Refs::labels` against git:

- each ref shows up exactly once in the sidebar, in the right group and
  under the right remote,
- ahead and behind counts match `git rev-list --left-right --count`,
- each label sits on the commit that `git rev-parse <ref>^{commit}`
  gives,
- each stash row names the stash that `git stash list` puts there.

This would have found the bug with remote names that hold a slash.

## Random sessions in the UI

`app/cmd.rs`, `app/input.rs` and `ui/draw.rs` are the biggest files,
and only example tests in `tests/ui.rs` cover them. Use the `Ui` helper
there to send random keys, clicks on random entries in `App::hits`,
right clicks, drags, scrolls and pastes, and to resize the terminal.
Between steps, sometimes edit files in the scratch repository or run a
git command behind tergi's back, and sometimes wait for the work in the
background to finish.

Leave out commands that reach the network or open an external program,
such as push, fetch and the editor. Destructive commands are fine,
because the repository is a scratch one.

After each step, check that nothing panicked, that drawing works at
the current size, and that the state hangs together. The history
selection is a commit that's in the list, or nothing. The detail cursor
and line selection are inside the document. The sidebar selection is
one of its rows. Only the top dialog gets clicks. Every enabled action
in the palette can run without a panic.

This needs no model, so it's cheap. It mostly finds panics and cases
where a reload leaves something pointing at a row that's gone.

## Rebases that stop

`rebase_props` only builds histories that can't conflict. Make the
commits touch the same few lines of one file, so that reordering,
squashing and dropping can conflict, and mix in `edit` steps. When the
rebase stops, pick randomly between taking a side with `take_side`,
editing the file, skipping and aborting, then continue. Check that:

- `state_in` reports the right state at each stop,
- aborting gives back the original branch, index and working tree,
- a finished rebase has the commits and messages that the model says.

The single commit operations in `git/rebase.rs` (`reword`, `single`,
`move_commit` and `fixup_staged_into`) can share the same model.

## The watcher never misses a change

Run random file system operations in a scratch repository: create,
change, delete and rename files and folders, nested several levels deep
and in quick succession. Move folders in from outside, edit
`.gitignore` and `.git/info/exclude`, and run git commands such as
commit, checkout, stash and branch. After each burst, wait for the
watcher to go quiet. Whenever `git status` or `git for-each-ref` gives
a different answer than before the burst, the watcher must have sent
the matching flag.

On Linux the watcher adds a watch to each new folder, so a folder that
gets files right after it's created is the likeliest place to miss
something. `design/watching.md` explains why. The test needs a timeout
for "quiet", so it should run few cases.

## Stashes and conflicts

Stash operations name a stash by id and look up its current index with
`stash_ref`. Push and drop random stashes, in tergi and directly with
git, then apply, pop or drop one by its id. Check that it is always the
right stash, and that the others are still there.

For `take_side`, make each kind of conflict that a merge can leave:
both changed, both added, deleted by us, deleted by them, and a file on
one side against a symlink or a folder on the other. Check that the
file in the working tree and in the index matches the side we took,
and that the conflict is gone.

## Smaller ones

**Highlighting.** Feed `highlight_lines` and `highlight_job` random
text with CRLF line ends, no final newline, tabs and wide characters,
in a few syntaxes. Check that each line's spans follow each other
without gaps, cover the whole line, and start and end on character
boundaries. Check that there's one entry for each line that the hunks
refer to.

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
