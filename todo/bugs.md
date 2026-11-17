# To fix

Known problems that nobody has fixed yet.

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

## Skipping a conflicted reword renames the commit before it

A reword is a `pick` followed by an `exec git commit --amend` line. When
the pick conflicts and the user skips it, git still runs the `exec`
line, which gives the new message to whatever commit came before. A
squash does the same with its combined message. The cause is in
`build_todo` and `skip_rebase` in `git/rebase.rs`.

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

## Aborting a fixup of staged changes leaves the fixup commit

`fixup_staged_into` commits the staged changes with `--fixup` and then
rebases. When that rebase stops on a conflict and the user aborts it,
git goes back to where the rebase started, which is the `fixup!`
commit. The changes the user had staged are now a commit on the
branch. If the rebase doesn't start at all, `fixup_staged_into` resets
the commit away, but `abort_rebase` doesn't know to do the same.
