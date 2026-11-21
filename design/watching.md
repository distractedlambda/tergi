# Watching for changes

tergi refreshes by itself when files change, including changes made by
other programs. This document explains how that works, why it works that
way, and what it still gets wrong. The code is in `src/watch.rs`, and the
app's side of it is in `src/app/mod.rs`.

## What the watcher reports

`watch::start` takes the working directory, the git directory and the
common directory. It returns a handle and a future. The app spawns the
future and keeps the handle. Dropping the handle stops the watcher and
ends the future.

The watcher reports two kinds of events:

- `Event::Changed(flags)` says what needs reloading. The flags are
  `WORKTREE`, `INDEX`, `REFS` and `STATE`.
- `Event::Degraded(message)` says that watching the working tree failed.

The handle also gives out `Updates`. After each status the app loads, it
sends the status to the watcher through them. The watcher only uses the
paths git lists as ignored.

The app turns `WORKTREE` and `INDEX` into a reload of the working
directory, and `REFS` into a reload of the refs and history. It reads the
rebase or merge state again on every refresh, so it doesn't look at
`STATE` separately. While one of tergi's own operations runs, the app
ignores these events. It refreshes when the operation finishes anyway.

The watcher collects events for a short while before it reports them. It
waits until nothing has happened for 120 ms, but never more than 600 ms
after the first event. A build or a checkout can produce thousands of
events, and this turns them into a few refreshes.

There are two other ways the app refreshes:

- It refreshes everything when the terminal window gets focus.
- If watching failed, it reloads the working directory every three
  seconds and says so in the status bar.

## The git directory

On every platform, we watch these folders. The git directory and the
common directory are the same folder, except in a linked worktree.

- The git directory and the common directory, without subfolders.
- `refs/` in each of them, with subfolders.
- `reftable/` in each of them, if the repository uses the reftable
  format. All refs and reflogs live in there, and every change ends with
  replacing `reftable/tables.list`.
- `info/` in the common directory, without subfolders, for
  `info/exclude`.
- `logs/refs/` in the common directory, without subfolders. This is for
  the stash reflog. Dropping a stash other than the newest one doesn't
  change `refs/stash`. Tools built on libgit2 only rewrite the reflog.
  A fresh repository has no `logs/refs/` until its first commit, so we
  try again when a ref changes.

`classify_git` decides what a change in the git directory means.

| Path | Flags |
| --- | --- |
| `index` | `INDEX` |
| `HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, `packed-refs`, `refs/…` | `REFS`, `INDEX` |
| `config`, `config.worktree` | `REFS`, `WORKTREE` |
| `info/exclude` | `WORKTREE` |
| `logs/refs/stash` | `REFS` |
| `reftable/tables.list` | `REFS`, `INDEX` |
| `rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `sequencer/` | `STATE`, `REFS`, `INDEX` |
| Anything ending in `.lock`, and everything else | nothing |

Git writes a `.lock` file and then renames it over the real file, so the
rename shows up as a change to the real file. Objects are ignored because
a new object on its own changes nothing we show. The config holds remotes
and upstreams, and it can set `core.excludesFile`, so a change there
reloads both refs and the working directory.

## The working tree

We don't read ignore rules ourselves. We ask git, through the status the
app loads anyway. The app runs `git status` with `--ignored=matching`,
which lists ignored paths too. An ignored folder appears once, with a
trailing slash, rather than every file in it. Git doesn't count a folder
as ignored if it holds tracked files, so such a folder doesn't appear.

The watcher keeps these paths in `Ignored` and drops events from them, on
every platform. That covers build output, and also ignored files that sit
next to tracked ones, such as a `*.log` file in `src/`.

When the rules change, the next status shows the result. A `.gitignore`
in the working tree and `info/exclude` are watched like any other file,
so editing them causes that status. Rules from anywhere else, such as the
global excludes file or the system config, take effect at the next
refresh for some other reason. The app also refreshes when the terminal
gets focus, which is usually right after the user changed them.

Git can only list ignored folders that exist. Build tools often delete
their output folder and make it again. If we forgot such a folder while
it was gone, we would see everything the build writes until the next
status. So `Ignored::new` keeps ignored folders from the previous status
as long as they don't exist.

The rules may change while such a folder is gone, so that git no longer
ignores it. We can't tell without asking git. So when a folder we kept
this way appears again, we report `WORKTREE` once, and keep dropping
events from inside it. If the status that follows still lists it as
ignored, nothing else happens. If it doesn't, the folder isn't ignored
any more, and the next section explains what happens then.

A path git hasn't seen yet can't be in the list. When a build folder
appears for the first time after startup, or a new `*.log` file is
written, it causes one refresh. That status lists it, and later events
from it are dropped.

### Linux

inotify watches single folders, not whole trees. At startup we only
watch the root. When the first status arrives, `Watches::rewatch_tree`
walks the working tree and watches each folder that isn't ignored. We
never watch build output such as `target/` or `node_modules/`. That keeps
us well under the inotify limit, and builds don't cause a storm of
refreshes.

- We skip `.git` folders, nested repositories and submodules. Changes
  inside them don't show up in our status, apart from the commit a
  submodule points at.
- We don't follow symlinks, because git doesn't either.
- The `Watches` struct remembers which folders we watch, in `tree`. That
  lets us remove watches later.
- When a folder is deleted or moved away, its parent reports it, and
  notify drops its watches on the folder and everything under it. We
  forget those folders in `tree` too. Otherwise a folder that comes back
  with the same name would look watched, and we would skip it.
- When a later status lists different ignored folders, we walk the whole
  tree again. We add watches for folders that are no longer ignored, and
  remove the watches for folders that now are. If that added any
  watches, we report `WORKTREE`. Files in those folders may have changed
  after git looked at them and before we watched them, and the next
  status catches that. If the walk fails partway, we remove nothing,
  because we can't tell which folders should stay.
- A folder can appear by being created or by being moved in. Before the
  watcher reports a batch, it walks each new folder and watches it and
  its subfolders, unless they are ignored. Doing this before the report
  means the status that follows can't miss a file written to the new
  folder. A new folder that git ignores but hasn't listed yet gets
  watched too, until the next status.
- If adding a watch fails, usually because we hit the inotify limit, we
  report `Degraded` once and the app falls back to polling.

### macOS

FSEvents watches a whole tree cheaply, so we watch the working directory
with all its subfolders, and rely on `Ignored` to drop the events we
don't care about.

When a later status no longer lists a folder as ignored, we report
`WORKTREE`. We dropped events from that folder until the status arrived,
and the next status catches anything that changed after git looked.

## How a batch is reported

The future runs on the async threads, but walking folders and adding
watches is blocking work. So the watcher moves its state, the `Watches`
struct, to a blocking thread with `util::blocking`, and gets it back
when the work is done. It does this in two places.

When a batch is ready to report, it does two things first:

1. Watch the stash reflog folder, if it might have appeared.
2. On Linux, watch the new folders and everything under them.

Then it reports the flags, if there are any.

When a status arrives, it builds the new `Ignored` and, on Linux, walks
the tree again if the ignored folders changed.

## Tests

The tests are at the bottom of `src/watch.rs`. Most of them start a real
watcher on a scratch repository and check what it reports. The `tell`
helper runs `git status` with the same arguments as the app and sends
the result to the watcher, the way the app does after a refresh.

`tests/watch_props.rs` runs random bursts of file and git changes in a
scratch repository and checks that the watcher reports everything that
changes what git says. It gives the watcher shorter delays through
`start_with`, mostly 10 or 30 ms instead of 120 ms. That makes the test
faster, and it splits bursts into more batches.

The tests run on both platforms, but the code that differs between
platforms only runs on one of them. The macOS-only parts have been tested
less, because the development container runs Linux.

## Known gaps and future work

These are things the watcher doesn't handle yet, roughly from most to
least likely to matter.

- **Rule changes outside the working tree.** When the global excludes
  file or the system config changes, nothing reloads the status until
  something else does, often the terminal getting focus. Until then, a
  folder that stopped being ignored isn't watched on Linux, and events
  from it are dropped on both platforms.
- **Startup on Linux.** Until the first status arrives, we only watch the
  root of the working tree. A change in a subfolder after git looked at
  it and before we watch it goes unnoticed until the next refresh. The
  window is about as long as one `git status`.
- **Nested repositories that lose their `.git` folder.** Events from
  inside `.git` folders are always dropped, so we don't notice when a
  nested repository's `.git` folder is deleted. The folder around it is
  then an ordinary folder. On Linux we never watched it, and a new
  status doesn't make us walk it, because the ignored folders didn't
  change. So we miss changes in it until something else makes us walk
  the tree.
- **Nested repositories on macOS.** On Linux we don't watch nested
  repositories or submodules. On macOS we get their events, and they
  cause refreshes unless git ignores them. The status lists an untracked
  nested repository as a folder with a trailing slash, so we could drop
  those too.
- **Other platforms.** Everything that isn't Linux takes the macOS path.
  On a BSD, notify uses kqueue, which needs a file descriptor for every
  watched file, so a recursive watch of a large tree would hit the
  limit. tergi only supports Linux and macOS today.
