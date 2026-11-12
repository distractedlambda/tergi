# To fix

Known problems that nobody has fixed yet.

- Line staging in files that aren't valid UTF-8 can't work, because the
  diff text is converted lossily. `Git::read` in `src/git/cli.rs` does the
  conversion, so the patch we build no longer matches the file.

- On macOS the list of ignored directories for the watcher is computed
  once at startup, in `ignored_directories` in `src/watch.rs`. A directory
  that becomes ignored later still causes refreshes.

- Combining characters are overwritten when drawing code lines, in
  `draw_code` in `src/ui/draw.rs`.

- The history selection can briefly point at the wrong commit while a
  reload is in flight.

- Dropping a stash other than the top one from another terminal doesn't
  trigger a refresh. That only rewrites `.git/logs/refs/stash`, and
  `classify_git` in `src/watch.rs` ignores everything under `logs`.
