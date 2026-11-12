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

- Background threads are detached. `std::thread::spawn` and
  `Builder::spawn(...).ok()` drop the `JoinHandle`, so nothing notices
  when a thread panics or exits early. The app aborts on panic, so there
  a panic still ends the process. Tests unwind, though, and a panicked
  worker leaves flags like `refs_inflight` or `busy` set forever. The
  threads are spawned in `src/app/mod.rs`, `src/terminal.rs`,
  `src/highlight.rs`, `src/watch.rs`, `src/askpass.rs`, and the stdin
  writer in `Git::run` in `src/git/cli.rs`. They should be kept and
  joined, or scoped, so a panic reaches the main thread.
