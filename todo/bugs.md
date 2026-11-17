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

## A click on a stale detail row puts the cursor past the end

Clicks are looked up in `App::hits` from the last frame. The main loop
handles every queued event before it draws again, so a click can hit a
row that a reload or an earlier click in the same batch has removed.
For detail rows, `click` and `right_click` in `app/input.rs` set
`detail.cursor` to the row's index without checking that the row still
exists. To see it, double click a commit link in the header of a commit.
The first click switches to the other commit, which shows a single
"Loading…" row, and the second click puts the cursor on the link's old
row.
