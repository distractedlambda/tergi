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
version of `a` rather than the old one.

The cause is in gix-diff 0.70. `Tracker::emit` matches copies by
`change.id()`, which for a modification is the new blob. We would need
a fix upstream, or our own copy detection. `tests/commit_props.rs`
leaves copies out until then.

## Remotes whose names contain a slash lose their branches

Git allows a remote named `a/b`. Its branches live under
`refs/remotes/a/b/`. `Refs::load` in `git/refs.rs` splits the ref name
at the first slash, so `refs/remotes/a/b/main` turns into branch
`b/main` of remote `a`. The sidebar lists the branches of each remote
from `git remote`, so `a/b` shows up with no branches, and `b/main`
doesn't show up anywhere because there is no remote `a`.

To see it, run `git remote add a/b <url>` and fetch from it.
