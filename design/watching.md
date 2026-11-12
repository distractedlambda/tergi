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
| `config` | `REFS`, because remotes and upstreams live there |
| `logs/refs/stash` | `REFS` |
| `reftable/tables.list` | `REFS`, `INDEX` |
| `rebase-merge/`, `rebase-apply/`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `sequencer/` | `STATE`, `REFS`, `INDEX` |
| Anything ending in `.lock`, and everything else | nothing |

Git writes a `.lock` file and then renames it over the real file, so the
rename shows up as a change to the real file. Objects are ignored because
a new object on its own changes nothing we show.

## The working tree

Linux and macOS watch the working tree in different ways, because inotify
and FSEvents work differently. Every platform other than Linux takes the
macOS path.

### Linux

inotify watches single folders, not whole trees. So we walk the working
tree with the `ignore` crate and watch each folder that git doesn't
ignore. We never watch build output such as `target/` or `node_modules/`.
That keeps us well under the inotify limit, and builds don't cause a
storm of refreshes.

- We skip `.git` folders, nested repositories and submodules. Changes
  inside them don't show up in our status, apart from the commit a
  submodule points at.
- A folder can appear by being created or by being moved in. When the
  watcher reports a batch, it first asks `git check-ignore` about each
  new folder and watches the ones git doesn't ignore.
- The `Watches` struct remembers which folders of the tree we watch, in
  `tree`. That lets us remove watches later.
- If adding a watch fails at startup, usually because we hit the inotify
  limit, we report `Degraded` and the app falls back to polling. When
  adding a watch fails later, for a new folder or after the rules
  changed, we ignore the failure and report nothing.

### macOS

FSEvents watches a whole tree cheaply, so we watch the working directory
with all its subfolders. That means we get events from ignored folders
too, and we have to filter them out.

At startup we ask git for the ignored folders with
`git ls-files --others --ignored --exclude-standard --directory`. We
drop events from inside those folders straight away. This is cheap, but
the list can go out of date, so we don't trust it alone.

Other changes in the working tree are collected per batch. Before
reporting the batch, `check_changes` sends the changed paths to
`git check-ignore --stdin`, together with every folder above each path.
That gives us two answers:

- If every changed path is ignored, we don't report a change.
- If an ignored path is a folder, or lies inside an ignored folder, then
  a folder we don't know about is ignored. That happens when a build
  folder appears after startup, or when a folder becomes ignored. We
  rebuild the list with `git ls-files`, so later events from that folder
  are dropped without asking git.

A lone ignored file in a normal folder, such as `.DS_Store`, doesn't make
us rebuild the list. Git doesn't count a folder as ignored if it holds
tracked files, and both `check-ignore` and `ls-files` agree on that.

`check-ignore` answers each path as soon as it reads it. We write the
paths from a separate thread. If we wrote all of them before reading, a
long list would fill both pipes, and git and tergi would wait on each
other forever. If git fails, we treat every path as not ignored, so the
worst case is an extra refresh.

## Ignore rules

Git reads ignore rules from several places:

- `.gitignore` files in the working tree.
- `info/exclude` in the common directory.
- The global excludes file. That is `core.excludesFile` if it's set, and
  otherwise `$XDG_CONFIG_HOME/git/ignore`, or `~/.config/git/ignore`
  without `XDG_CONFIG_HOME`. Git uses this default on macOS too.

When any of these change, two things follow. First, the set of untracked
files git shows can change, so we report `WORKTREE`. Second, folders can
become ignored or stop being ignored, so we redo the work that depends on
that:

- On Linux, `Watches::rewatch_tree` walks the whole tree again. It adds
  watches for folders that are no longer ignored, and removes the watches
  for folders that now are. If the walk fails partway, it removes
  nothing, because it can't tell which folders are ignored now.
- On macOS, we rebuild the list of ignored folders.

We notice `.gitignore` changes as ordinary working tree events. The other
two files are outside the working tree, or at least not where we look for
.gitignore files, so we watch them separately. `rules_files` finds them,
and `Watches::rules` holds them.

The global excludes file can move. Which file it is depends on
`core.excludesFile`, which can be set in the repository config or the
global config. So we also watch the global config files, in
`Watches::configs`. These are `~/.gitconfig` and
`$XDG_CONFIG_HOME/git/config` (or `~/.config/git/config`). If
`GIT_CONFIG_GLOBAL` is set, git reads only that file, and so do we.
When the repository config or a global config file changes, we ask git
again which excludes file it uses. If the answer changed, we watch the
new file and treat the change as a change of rules.

### Watching single files

`watch_file` watches one file. It has to deal with a few details:

- Editors often save by writing a new file and renaming it over the old
  one. On Linux that ends a watch on the file itself, so we watch its
  folder instead.
- On macOS, watching a folder without its subfolders still makes FSEvents
  watch all of them. For `~/.gitconfig` that would be the whole home
  folder. So on macOS we watch the file itself if it exists. FSEvents
  follows paths, not files, so it should keep working after a rename.
  Nobody has checked that on a Mac yet.
- Dotfile managers often make these files symlinks. We resolve the path
  first, because events use the real path.
- The file may not exist yet. Then we watch its folder, if that exists.
  If the folder doesn't exist either, we try again each time the watcher
  reports a batch. When we start watching a file late, we treat it as
  changed, because it may have changed before we started.

## How a batch is reported

The future runs on the async threads, but most of the work at the end of
a batch is blocking. It walks folders, adds watches and runs git. So the
watcher moves its state, the `Watches` struct, to a blocking thread with
`util::blocking`, and gets it back when the work is done. In order, that
work is:

1. Watch the stash reflog folder, if it might have appeared.
2. Watch any global config files that appeared, and look up the rules
   files again if a config file changed.
3. Watch any rules files that aren't watched yet.
4. On Linux, walk the whole tree again if the rules changed. Otherwise
   watch the new folders.
5. On macOS, run `check_changes` on the changed paths.

Then the watcher reports the flags, if there are any.

## Tests

The tests are at the bottom of `src/watch.rs`. Most of them start a real
watcher on a scratch repository and check what it reports. They run on
both platforms, but the code that differs between platforms only runs on
one of them. The macOS-only parts have been tested less, because the
development container runs Linux. `check_changes` doesn't depend on
FSEvents, so its test runs everywhere.

`follows_the_global_config` needs its own `GIT_CONFIG_GLOBAL`. Tests run
in parallel threads of one process and share its environment, and
changing the environment needs `unsafe`, which the crate forbids. So the
test runs the test binary again in a child process, with the variable
set, and runs `follows_the_global_config_in_this_process` there. That
inner test is marked `#[ignore]` so it doesn't run on its own.

## Known gaps and future work

These are things the watcher doesn't handle yet, roughly from most to
least likely to matter.

- **Tracked files in ignored folders on Linux.** The `ignore` crate
  decides which folders we watch, and it doesn't know about the index.
  If a folder matches an ignore rule but holds files that were added with
  `git add -f`, we don't watch it, and changes to those files go
  unnoticed until the next refresh. Git itself doesn't count such a
  folder as ignored, so macOS gets this right.
- **Two sources of ignore rules on Linux.** The tree walk uses the
  `ignore` crate's reading of git's rules, while the rest of the watcher
  asks git. They can disagree in unusual setups, for example on how the
  global excludes file is found.
- **Ignored files in watched folders on Linux.** On Linux, a change to an
  ignored file in a folder we watch, such as a `*.log` file next to the
  source, still causes a refresh. macOS filters these out with
  `check-ignore`. Linux could do the same.
- **Nested repositories on macOS.** On Linux we don't watch nested
  repositories or submodules. On macOS we get their events, and they
  cause refreshes unless git ignores them.
- **The system git config.** We don't watch `/etc/gitconfig`, or the
  system config of another git install, such as Homebrew's under
  `/opt/homebrew/etc`. Setting `core.excludesFile` there takes effect
  after a restart.
- **Included config files.** Config files pulled in with `include.path`
  or `includeIf` aren't watched.
- **`config.worktree`.** With `extensions.worktreeConfig`, a linked
  worktree can have its own `config.worktree` in its git directory. We
  don't treat a change there as a config change.
- **Watch failures after startup.** If a watch for a new folder fails
  on Linux, nobody hears about it, and changes in that folder go
  unnoticed. Reporting `Degraded` would turn on polling.
- **Old watches.** When the global excludes file moves, we keep watching
  the old one. That only costs a few events. Likewise, a deleted folder
  stays in `Watches::tree` until the next walk of the whole tree.
- **Other platforms.** Everything that isn't Linux takes the macOS path.
  On a BSD, notify uses kqueue, which needs a file descriptor for every
  watched file, so a recursive watch of a large tree would hit the
  limit. tergi only supports Linux and macOS today.
