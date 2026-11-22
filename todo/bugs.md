# To fix

Known problems that nobody has fixed yet.

## Working tree texts skip git's clean filters

`read_text` in `src/git/workdir.rs` says it reads a file "as git would
store it", but it reads the raw bytes. It doesn't apply `core.autocrlf`,
the `text` and `eol` attributes, `working-tree-encoding`, `ident` or
filter drivers such as git-lfs. `git diff` compares the index with the
filtered file, so the new side of an unstaged diff and its text can
disagree.

With `core.autocrlf = input` and a file with CRLF line endings, every
line of the text ends in a carriage return, but the index lines don't.
`view` in `src/git/context.rs` then finds no lines that both sides
share, so the edges of the file's unstaged hunks can't move out past
git's context. Highlighting gets the CRLF text too. This was found by
reading the code and hasn't been reproduced yet.

## Copies are found from the wrong version of a changed file

With `diff.renames = copies`, a commit diff looks for copies among the
files that the commit changed. Git compares a new file with the old
version of each changed file. Gix compares it with the new version. So
tergi and git disagree about copies whenever the source of the copy
also changed.

To see it, commit a file `a`. Then in one commit, change `a` and add `b`
with the old content of `a`. Git shows `b` as a copy of `a`, and tergi
shows it as an added file. If `b` has the new content of `a` instead,
tergi shows a copy, and git only does if the old and new `a` are
similar enough. When both find a copy, tergi diffs `b` against the new
version of `a` rather than the old one. And when tergi finds the copy,
the change to `a` itself is missing from the list. Git lists `a` as
modified and `b` as a copy, but tergi only lists the copy.

The cause is in gix-diff 0.70. `Tracker::emit` matches copies by
`change.id()`, which for a modification is the new blob. We would need
a fix upstream, or our own copy detection. `tests/commit_props.rs`
leaves copies out until then.

## An empty root commit after resolving the first pick of a `--root` rebase

A rebase with `--root` replays the commits onto an empty commit that git
makes for the purpose. When the first pick applies cleanly, git makes
it a root commit. When the first pick conflicts and the user resolves
it and continues, git commits it on top of the empty commit instead. The
branch then starts with an empty commit that has an empty message.

To see it, make a repository with two commits where the second changes
a file that the first added. Then move the second commit down with
Ctrl+J, take either side and continue. This is git's behavior. The
command line does the same, and tergi doesn't work around it.
`tests/rebase_stop_props.rs` expects it.

## `G` starts a line selection in the detail pane

At the line level of the detail pane, `G` selects every line from the
cursor to where it moves, instead of only moving there.

To see it, select a commit or the working directory, go into the lines
of a hunk with `l` twice, and press `G`. The lines are marked as
selected, as with Shift+Down.

The cause is in `nav_key` in `src/app/input.rs`. It treats Shift with
any movement key as extending the selection. Crossterm reports an
uppercase letter with the Shift modifier, so `G` counts too. The
history only extends with Shift for keys that aren't letters.

## Changes in a nested repository that lost its `.git` folder

On Linux, the watcher doesn't watch nested repositories. When the
`.git` folder of one is deleted, the folder around it becomes an
ordinary untracked folder, but the watcher never starts watching it.
Changes in it go unnoticed until something else makes the watcher walk
the tree, such as a change to which folders git ignores.

To see it, make a folder `sub` in the working tree and run `git init`
in it. Start tergi, then delete `sub/.git` and write `sub/f`. The
status shows `sub/` as untracked after the next refresh, but later
writes in `sub` never cause a refresh.

The cause is in `src/watch.rs`. Events from inside `.git` folders are
always dropped, so the watcher doesn't see the deletion. And
`Watches::update` only walks the tree again when the ignored folders
change, which they don't here. macOS watches the whole tree, so it only
misses the deletion itself.

## A UI test of the message box can fail under load

`ui::commits_and_amends_from_the_message_box` in `tests/ui.rs` commits
from the message box, waits until the history shows three commits, and
then checks that the message box is empty. The message box is only
cleared when the app handles the commit's `OpDone` event
(`After::ClearMessage` in `on_op_done` in `src/app/mod.rs`). The file
watcher can see the new commit and reload the history before that
event arrives, so the check runs too early and fails. It failed once
in a full `cargo test` while other containers kept the machine busy,
and passed when run on its own. Waiting for the message box to empty,
or for `ops_done` to go up, would fix it.
