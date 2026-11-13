# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

tergi is a terminal git client modeled on Sublime Merge, written in Rust
with ratatui, crossterm and Tokio. `README.md` describes what it does from
the user's side, and `TODO.md` lists known bugs that nobody has fixed yet.

## Building and testing

Build and test inside the dev image from `dev/Containerfile`, not on the
host. The image has Rust, clippy, rustfmt, tmux, and a git new enough for
the reftable tests. It also sets a system-wide git identity, which the
tests need. Run these commands inside it:

```sh
cargo build
cargo test
cargo test --test ui                       # one integration test file
cargo test --test staging some_test_name   # one test
cargo test --lib git::patch                # unit tests in one module
cargo clippy --all-targets
cargo fmt
```

Clippy lints are set in `Cargo.toml`. `unwrap_used` warns outside tests,
and `unsafe_code` is forbidden. Both dev and release builds use
`panic = "abort"`.

To try the app by hand, run it inside tmux in the dev container, so you
can send it keys and capture the screen.

## Architecture

### Event loop

`main.rs` runs one loop. It draws a frame, waits for either a terminal
event or an `AppEvent` from `App::next`, then handles that event and
everything else already queued before it draws again.

All slow work happens in background Tokio tasks that the `App` owns in a
`JoinSet` (`App::spawn`). Tasks send their results back as `AppEvent`s
over an unbounded channel. If a task panics, the panic comes back out of
`App::next` or `App::try_recv`. Blocking work, such as the gix history
walk, goes through `util::blocking`.

Loads of refs, the working directory, history and commit details carry a
generation number. The handler drops results whose generation is stale.
Refs and workdir loads also coalesce: a refresh that arrives while a load
is running sets an "again" flag instead of starting a second load.

### Reading with gix, writing with the git CLI

- Reads that need to be fast use gitoxide. These are the history walk in
  `git/log.rs` and commit diffs in `git/commit.rs`, which diff the text
  with imara-diff.
- Every write, and the status and working tree diffs, run the `git`
  executable through `git/cli.rs`. That way hooks, signing, credential
  helpers and the user's config behave as they do on the command line.
  `Git` keeps a log of the commands it ran, which the `@` overlay shows.
- Operations that change the repository go through `App::spawn_op`. Only
  one can run at a time. When it finishes, the app shows the result,
  applies its `After` action and refreshes everything.

### tergi calls itself

`main` checks two special modes before parsing arguments, because git
runs the tergi executable back.

- **Askpass.** For remote operations, `Repo::remote_env` points
  `GIT_ASKPASS` and `SSH_ASKPASS` at tergi. The child connects to a Unix
  socket owned by the running app, which shows a prompt dialog
  (`askpass.rs`).
- **Sequence editor.** History rewriting runs `git rebase -i` with
  `GIT_SEQUENCE_EDITOR` set to `tergi __seq-editor <todo>`, which swaps in
  a todo list that tergi wrote. Rewords and squashes use
  `exec git commit --amend -F <file>` lines, so git never opens an editor
  (`git/rebase.rs`).

Integration tests set `repo.exe` to `CARGO_BIN_EXE_tergi`, because the
current executable there is the test runner.

### Commands and actions

`app/cmd.rs` defines `Cmd`, every action the user can take. Each pane
builds a list of `ActionDef`s from the current selection
(`App::actions`). That one list drives key handling, right-click menus,
the `?` help, the hint bar and the command palette. To add a feature,
add a `Cmd` variant, list it in the right pane's actions, and handle it
in `run_cmd`. Destructive commands ask first through `confirm_for`.

### Detail pane and drawing

`app/doc.rs` flattens a commit's diff, or the staging view, into a list
of `Row`s so it can be scrolled and drawn quickly. Line staging builds a
partial patch in `git/patch.rs` and feeds it to `git apply`. Syntax
highlighting runs in the background (`highlight.rs`) and is matched to
the current document by generation.

`ui/draw.rs` draws in two phases. First it computes the layout and stores
it in `App::areas`. Then it draws and records clickable regions in
`App::hits` as `input::Target`s. Mouse handling in `app/input.rs` looks up
clicks in that hit list, so anything clickable must be registered while
drawing.

### File watching

`watch.rs` watches the working tree and the git directory, and it works
differently on Linux (inotify on each directory git doesn't ignore) and
macOS (FSEvents on the root). `design/watching.md` explains how it works
in detail. Read it before changing the watcher.

## Tests

- Unit tests live next to the code. Some use inline `insta` snapshots.
- Integration tests in `tests/` build scratch repositories with
  `common::Scratch`.
- `tests/ui.rs` drives the whole `App` against a ratatui `TestBackend`.
  It sends real key and mouse events, and finds click targets through
  `App::hits`. Its `wait` helper handles events until a condition holds,
  with a timeout.
- Some tests need git 2.45 or newer for reftable repositories. With an
  older git they skip themselves.
