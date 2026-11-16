# History and the commit graph

The middle pane lists every commit reachable from a ref, newest first,
with a graph of how they connect. This document explains where the list
comes from, how the graph is laid out, and how the selection survives a
reload.

The refs are read in `src/git/refs.rs`. The walk is in `src/git/log.rs`
and the graph layout in `src/git/graph.rs`. The app's side is
`start_history_walk` and `on_history` in `src/app/mod.rs`, and
`draw_history` in `src/ui/draw.rs`.

## Refs

`Refs::load` asks git for the branches, remote branches and tags with
one `git for-each-ref`. It reads the target of each ref, the commit an
annotated tag points at, the upstream with its ahead and behind counts,
and which branch is checked out. Then it lists the remotes and the
stashes and resolves HEAD. A remote's `HEAD` ref is left out, because it
only repeats one of the remote's branches.

From the refs the app makes two things:

- `labels`, the names drawn next to each commit in the history and in
  the commit details.
- `tips`, the commits the walk starts from. These are HEAD, every
  branch, every remote branch and every tag, without duplicates.
  Stashes aren't tips. They show up in the sidebar instead.

`on_refs` only starts a new walk when the set of tips changed, or when
there hasn't been a walk yet. For example, creating a branch where
another ref already points changes no tips. Then the history stays as it
is and only the labels change.

## The walk

`log::walk` runs on a blocking thread through `util::blocking`. It uses
gitoxide's topological walk, with the commit-graph file if the repository
has one, and gives the repository a 64 MiB object cache.

Some tags point at trees or blobs rather than commits. git.git has one
for a GPG key, for example. The walk refuses to start from those, so we
leave them out first.

Commits go back to the app in `Batch`es over the event channel. The
first batch has 200 commits, so the first screen appears quickly. Later
batches have 5,000. Each commit is decoded into a `CommitInfo` with its
parents, summary, author and author date. The graph row for each commit
is worked out on the same thread, as part of the walk.

### Replacing a walk

A new walk can start while an old one is still running, for example
after a fetch. Walks are numbered with `util::Generation`. The running
walk holds a clone of the counter and checks it before each commit. Once
a newer walk has started, the old one stops. `on_history` also drops any
batch whose number isn't current, in case one was already on its way.

The old list stays on screen until the new walk's first batch arrives.
`replace_on_next` tells `on_history` to clear the list at that point.
Without it, the history would flash empty on every refresh.

## The graph

`GraphBuilder` lays out the graph one row at a time, in the order the
walk gives the commits. It keeps a list of lanes. Each active lane waits
for one particular commit to show up.

When a commit arrives:

1. It takes the leftmost lane that waits for it. If no lane does, it is
   a branch tip, and it takes the first free lane.
2. Any other lanes waiting for the same commit join it on this row and
   become free. This is where branches fork, read from top to bottom.
3. The first parent continues in the commit's own lane.
4. Each other parent joins a lane that already waits for it, or gets a
   new lane. This is where a merge draws a line out to the side.

A row is two cells per lane. The first cell is the lane itself, and the
second is the gap to its right that horizontal lines pass through. Each
cell holds direction bits (`UP`, `DOWN`, `LEFT`, `RIGHT` and `NODE`) and
a color index. `Cell::glyph` turns the bits into a box-drawing
character, such as `│`, `╭`, `┴` or `●`.

Here is a merge of a feature branch, as the tests draw it:

```text
●─╮ 5
│ ● 3
● │ 4
●─╯ 2
● 1
```

### Colors

There are eight lane colors, and each theme supplies its own eight. A new
lane takes the next color in turn. A first parent keeps its child's
color, so a branch stays one color all the way down. Where a line from
another lane meets a lane that carries on past that row, the meeting
cell keeps the lane's own color. That way the lane doesn't change color
for one row where a branch forks off or merges in.

### Width

A row is cut off after 48 lanes (`MAX_LANES`). A graph that wide isn't
readable anyway, and the limit keeps memory bounded on huge
repositories. When drawing, the graph column is as wide as the widest
row on screen, up to a third of the pane. So a stretch of history with
one lane doesn't waste space because of a wide stretch elsewhere.

## The selection

Row 0 is the working directory, and row `i + 1` is `commits[i]`. The
selection is kept as a `Selected` value, which is either the working
directory or a commit id, not a row number. A reload can move rows
around, and a commit id still names the same commit afterwards.

`History::index` maps each loaded commit's id to its place in the list.
While the selected commit isn't loaded yet, for example early in a new
walk, keys move from `last_row`, the row the selection was last seen
in. When a batch brings the commit in, `on_history` updates `last_row`
and scrolls to it.

When the walk is done and the selected commit isn't there, it is
usually because a rebase rewrote it. Then `on_history` selects
whatever is in the same row now, which keeps the user near where they
were.

Clicking a parent in the commit details runs `jump_to`. If the parent is
loaded, it gets selected. If not, the detail pane shows it anyway, and
the status bar says it isn't in the history yet.

## Drawing

`draw_history` only draws the rows that are visible, so a long history
costs nothing to draw. Each row has the graph, the ref labels, the
summary, and, if the pane is at least 70 columns wide, the author and
the date. Dates are relative ("3 days ago") unless `relative_dates` is
off. Then they are the day, in the commit's own time zone.

## Tests

- `src/git/graph.rs` has snapshot tests for small histories, such as a
  branch and merge, two tips, an octopus merge, and a merge into a lane
  that already exists.
- `lines_connect` in the same file is a property test over random
  histories. Some parents are never listed, as in a shallow clone. It
  checks that each row has one node, that a node has a line above it
  exactly when a newer commit has it as a parent, that horizontal lines
  join up, and that every lane going down carries on in the next row in
  the same color.
- `tests/log_props.rs` builds random histories with merges, several
  roots and dates out of order. It points branches, remote branches and
  tags at them, including tags of tags and tags of trees and blobs. Then
  it checks that `Refs::load` and `log::walk` find the same commits as
  `git rev-list --all`, with the right parents, and children before
  their parents. Some cases have a commit-graph file that covers all of
  the history or only part of it.
- `tests/ui.rs` has `selection_follows_its_commit_through_a_reload` and
  `first_load_starts_at_the_top`.

## Known gaps

- There is no search, and no history for a single file.
- Rows wider than 48 lanes are cut off, so lines past that point are
  missing rather than squeezed.
- A commit outside the loaded part of the history can be shown, but not
  selected in the list, until the walk reaches it.
