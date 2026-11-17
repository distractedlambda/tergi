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

## Messages pick up git's comments after a rebase stops

When a rebase stops and the user continues it in tergi, the commit
message can end up full of lines that git meant as comments:

- After a conflict, the message gets `# Conflicts:` and the list of
  files.
- At an `edit` step with staged changes, continuing amends the commit,
  and the message gets git's whole template, from `# Please enter the
  commit message` down.
- When a fixup conflicts, the message becomes `# This is a combination
  of 2 commits.` followed by both messages.

To see it, stop at an edit step, stage a change, and continue.

The cause is `-c commit.cleanup=verbatim` in `REBASE` in
`git/rebase.rs`. Git writes these comments expecting them to be
stripped, and `GIT_EDITOR=true` accepts them as they are.

## Skipping a conflicted reword renames the commit before it

A reword is a `pick` followed by an `exec git commit --amend` line. When
the pick conflicts and the user skips it, git still runs the `exec`
line, which gives the new message to whatever commit came before. A
squash does the same with its combined message. The cause is in
`build_todo` and `skip_rebase` in `git/rebase.rs`.
