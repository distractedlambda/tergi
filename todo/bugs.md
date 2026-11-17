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

## Moving away from a commit that is loading leaves it loading

If you select a commit and then select the working directory before the
commit's details arrive, `detail.loading` stays true. The answer for the
commit is dropped because the detail pane no longer shows it, and that
answer was the only thing that cleared the flag. The app then keeps
ticking every 120 ms as if something were still running. To see it,
call `select_history(1)` and then `select_history(0)` in one go.

The cause is in `App::show_source`. `Detail::set_source` doesn't clear
`loading`, and only `request_commit` sets it again.
