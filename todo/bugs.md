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

## Ref names that aren't UTF-8 are read as text

Git allows ref names with bytes that aren't UTF-8, such as a branch
named `caf\xe9` ("café" in Latin-1). `Refs::load` in `git/refs.rs` reads
`git for-each-ref` as text, so such a byte turns into U+FFFD. The
sidebar shows the branch, but every command that names it, such as
checkout, merge, delete or push, passes the replaced name to git. Git
then fails because no such ref exists. Two such names can also look
like one ref.

To see it, run `git branch "$(printf 'caf\xe9')"` and check the branch
out from the sidebar.

Paths are kept as bytes (see `design/running-git.md`). Ref names would
need the same: `BString` in `Refs`, `Label` and the commands that carry
a ref name, and `OsStr` arguments when they go to git. `Status::branch`
and the rebase state's `head_name` have the same problem.
