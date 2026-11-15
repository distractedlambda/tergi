# The event loop and background work

tergi never waits for git while it handles a key or a click. Everything
slow runs in the background, and its results come back as events. This
document explains the loop, the kinds of background work, and the rules
that keep the UI right when results arrive late or out of order.

The loop is `run` in `src/main.rs`. Everything else is in
`src/app/mod.rs`, apart from `util::blocking` and `util::Generation` in
`src/util.rs`.

## The loop

`run` repeats these steps:

1. Draw a frame. The frame is wrapped in the terminal's synchronized
   update sequences, so the terminal shows it all at once and never a
   half-drawn screen.
2. Set the mouse pointer shape, if it changed.
3. Wait for a terminal event or an `AppEvent` from `App::next`.
4. Handle it with `App::handle`.
5. Handle everything else that is already queued, up to 1000 events,
   before drawing again. A burst of mouse moves then costs one redraw,
   not one per event.
6. Quit, or suspend for an editor or Ctrl+Z, if a handler asked for it.

`App::handle` and everything it calls are plain functions. They change
the app's state and start background work, but they never wait. Work
that takes a moment, such as reading a small file in the git directory
or saving the config, is fine there. Running git is not.

### Ticks

A task sends `AppEvent::Tick` so the screen updates even when nothing
else happens. While something is loading or running, it ticks every
120 ms, which drives the spinner. Otherwise it ticks about once a second,
which is enough to expire status messages and to poll the working
directory when file watching failed.

### Suspending

Opening a file in the editor and Ctrl+Z both need the real terminal. A
handler can't do that itself, so it sets `App::suspend`, and the loop
does the rest. It drops the terminal event stream, so crossterm stops
reading keys meant for the editor. Then it restores the terminal, runs
the editor or stops the process, sets up the terminal again and
refreshes everything.

## Background tasks

All background work runs as Tokio tasks in one `JoinSet` that the `App`
owns. `App::spawn` adds one. Dropping the App aborts them all.

Tasks send their results over one unbounded channel as `AppEvent`s. If a
task panics, `App::next` and `App::try_recv` raise the same panic in the
caller. A panic in the background then stops the app with the real
message, instead of leaving it waiting for an event that never comes.

Blocking work, such as the gitoxide history walk or parsing a large
diff, goes through `util::blocking`. It runs the closure on Tokio's
blocking pool, and a panic in it carries on in the task that waited
for it.

When tergi quits, `main` shuts the runtime down without waiting for
blocking work that is still running. A history walk of a huge
repository would otherwise keep the process alive after the screen is
gone.

### Kinds of background work

There are a few kinds, and each has its own rule for what happens when
a new request arrives while an old one is still running.

**Loads that run one at a time.** The refs and the working directory are
loaded by tasks that `Reload` keeps to one copy at a time. A request
that comes in while a load runs makes it run once more afterwards, so
any number of requests during a load add up to one more load. Running
two copies at once could deliver their results in the wrong order.
Cancelling the running copy instead could mean that, while files keep
changing, a load never gets to finish.

**Requests where only the newest matters.** The history walk and syntax
highlighting number their requests with `util::Generation`. A result is
only used if its number is still current. Clones of a `Generation` share
the counter, so the running walk can hold one and stop early once a
newer walk starts. Commit details go through a watch channel that only
keeps the newest request (see `diffs.md`).

**Reads.** `App::spawn_read` reads something from the repository and then
runs a closure on the event loop with the result, through
`AppEvent::ReadDone`. It is how a handler that needs an answer from git
gets one without waiting, for example reading a commit message before
opening the reword dialog. By the time the closure runs, the user may
have moved on. So it has to check that what it is about to change is
still there, and that the user isn't doing something else. Most of them
check that no dialog has opened in the meantime.

**Operations.** Anything that changes the repository goes through
`App::spawn_op`. Only one runs at a time. While one runs, `App::busy`
holds its label, the top bar shows a spinner, and starting another one
only shows "Please wait". An operation can report progress lines, which
the top bar shows. When it finishes, `on_op_done` shows the result,
applies its `After` action, such as clearing the commit message, and
refreshes everything.

File watcher events are ignored while an operation runs. Git changes
many files during a rebase or a checkout, and the refresh at the end
picks up all of it at once.

## Keeping state across reloads

Results can arrive at any time, and a reload can change every list in
the app. So anything that has to outlive the event that created it
names its target by something that stays the same across a reload, not
by a row number or an index.

- The history selection is a commit id, kept in `Selected`. See
  `history.md`.
- The sidebar finds its selected ref again by name in
  `rebuild_sidebar`. If the ref is gone, the same row stays selected.
- The detail pane finds its cursor again by the file's path, hunk and
  line, through `RowKey`. See `diffs.md`.
- A staging `Unit` names its file by path and keeps the diff its hunks
  and lines came from. See `staging.md`.
- Planner rows waiting for a message are found again by commit id. See
  `rewriting-history.md`.
- "Show anyway" for a large working directory file finds the file again
  by its paths when the full diff arrives.

When you add a feature that starts background work and then changes
something when it comes back, follow the same rule.

## Refreshing

`App::refresh` takes flags for what to reload: the refs, the working
directory, or both. A refs load can lead to a new history walk, if the
tips changed (see `history.md`). Every refresh also reads the rebase or
merge state again.

Refreshes come from several places:

- The file watcher, for changes in the working tree or the git
  directory. See `watching.md`.
- The end of every operation.
- The terminal window getting focus.
- A tick every three seconds when file watching failed.
- Changes to `context_lines`, which reload the working directory and the
  commit shown.

## Tests

`tests/ui.rs` drives the whole app. Its `Ui` helper makes the `App`
inside a multi-threaded Tokio runtime, so the app's tasks run on the
runtime's threads while the test stays synchronous. `wait` handles
queued events with `try_recv` and redraws until a condition holds, with
a 15 second timeout. On a timeout it prints the screen.

- `a_panic_in_the_background_reaches_the_caller` checks that a panic in
  a task comes back out of `try_recv`.
- `selection_follows_its_commit_through_a_reload` and
  `sidebar_selection_follows_its_branch_through_a_reload` check the
  selection rules above.
- `reload_runs_one_copy_and_one_more_after_it` in `src/app/mod.rs` tests
  `Reload` on its own.

## Known gaps

- `rebase::state_in` reads the git directory while handling an event.
  These are a few small files, but on a slow network file system it
  could be noticeable.
- Only one operation can run at a time. A fetch that waits for a
  password blocks staging until it finishes.
