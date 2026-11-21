# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

tergi is a terminal git client initially modeled on Sublime Merge, written in Rust
with ratatui, crossterm and Tokio. `README.md` describes what it does from
the user's side, and `todo/bugs.md` lists known bugs that nobody has fixed
yet. `design/` explains how each part works inside. `design/README.md`
lists the documents. Read the one for the part you are changing.

## Reporting bugs you find

When you notice a bug that `todo/bugs.md` doesn't mention, add it there,
even if it has nothing to do with your current task. Describe what goes
wrong, how to make it happen, and where in the code the cause is, if you
know. Commit the entry on its own. When you fix a bug that is listed
there, remove its entry in the same commit as the fix.

## Building and testing

Build and test inside the dev image from `dev/Containerfile`, not on the
host. The image has Rust, clippy, rustfmt, tmux, and a git new enough for
the reftable tests. It also sets a system-wide git identity, which the
tests need. Run these commands inside it:

```sh
cargo build
cargo test
cargo test --test integration ui::                        # one test file
cargo test --test integration staging::some_test_name     # one test
cargo test --lib git::patch                                # unit tests in one module
cargo clippy --all-targets
cargo fmt
```

Clippy lints are set in `Cargo.toml`. `unwrap_used` warns outside tests,
and `unsafe_code` is forbidden. Both dev and release builds use
`panic = "abort"`.

`.github/workflows/ci.yml` runs the tests on Linux and macOS and builds
release binaries with the `dist` profile. For each push to `main`, it also
makes a prerelease with them. It names the Rust version
itself, so change it there too when you update the one in
`dev/Containerfile`.

Code is formatted with rustfmt's default settings. `rustfmt.toml` is empty
so that nobody's personal config applies. `tests/fmt.rs` runs
`cargo fmt --check`, so `cargo test` fails until you run `cargo fmt`.

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

`App::handle` and everything it calls are plain functions, so handling
an event never waits for git. When a handler needs to read something
from git first, it uses `App::spawn_read`. That runs the read in the
background and applies the result when it comes back as an event. The
user may have moved on by then, so the code that applies it checks that
its target is still there.

Refs and working directory loads run one at a time, which `Reload`
keeps track of. A refresh that arrives while a load is running makes it
run once more afterwards instead of starting a second copy. The history
walk and syntax highlighting number their requests with
`util::Generation` and drop results from older requests. The running
walk holds a clone of its counter and stops early once a newer walk
starts. Commit details go through a watch channel that only keeps the
newest request.

The history selection is a commit id (`Selected`), not a row, because a
reload can move rows around while it runs. Anything else that outlives
the event that created it follows the same rule. The sidebar finds its
selected ref again by name after a reload, planner rows are found by
commit id, and a staging `Unit` names its file by path and keeps the
diff that its hunks and lines came from.

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

`main` checks for a special mode before parsing arguments, because git
runs the tergi executable back. History rewriting runs `git rebase -i`
with `GIT_SEQUENCE_EDITOR` set to `tergi __seq-editor <todo>`, which
swaps in a todo list that tergi wrote. Rewords and squashes use
`exec git commit --amend -F <file>` lines, so git never opens an editor
(`git/rebase.rs`).

tergi never answers prompts from git or ssh. `Git::base_command` makes
them fail instead of asking on the terminal (`design/running-git.md`).

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
highlighting runs in the background (`highlight/`) and is matched to
the current document by generation. Each language has its own lexer,
written with Logos. Colors come from lexing alone. Identifiers that
get no other color are colored by their names.

`ui/draw.rs` draws in two phases. First it computes the layout and stores
it in `App::areas`. Then it draws and records clickable regions in
`App::hits` as `input::Target`s. Mouse handling in `app/input.rs` looks up
clicks in that hit list, so anything clickable must be registered while
drawing.

Text is measured and drawn one grapheme at a time through `ui/text.rs`.
Use its `width`, `truncate`, `put` and `draw` rather than `len()`,
`chars().count()` or ratatui's `set_stringn`. `design/text-width.md`
explains the rules.

### File watching

`watch.rs` watches the working tree and the git directory, and it works
differently on Linux (inotify on each directory git doesn't ignore) and
macOS (FSEvents on the root). `design/watching.md` explains how it works
in detail. Read it before changing the watcher.

## Tests

- Unit tests live next to the code. Some use inline `insta` snapshots.
- Integration tests in `tests/` build scratch repositories with
  `common::Scratch`. Each file there is a module of `tests/main.rs`, so
  they all build into one binary and run at the same time. Cargo only
  finds a new file once you add it to the list in `tests/main.rs`.
- `tests/ui.rs` drives the whole `App` against a ratatui `TestBackend`
  with the `Ui` helper from `tests/common/ui.rs`. It sends real key and
  mouse events, and finds click targets through `App::hits`. The
  helper's `wait` handles events until a condition holds, with a
  timeout.
- `tests/ui_props.rs` runs random sessions in the UI: keys, clicks,
  drags, resizes, palette commands, file edits and git commands behind
  the app's back. It has no model. It checks that nothing panics and
  that selections and cursors point at rows that exist.
- Some tests need git 2.45 or newer for reftable repositories. With an
  older git they skip themselves.
- Property tests use `proptest`. The most important one is
  `tests/patch_props.rs`, which stages, unstages and discards random line
  selections with the real git and checks the result against a model.
  It does this both in a single step and in random sequences of steps
  over several files. Most property tests run 256 cases by default, and
  slow ones run fewer. Set `PROPTEST_CASES=5000` to search
  harder after changing the code they cover. When one fails, proptest
  saves the case under `proptest-regressions/` (or next to the file in
  `tests/`) and runs it first from then on. Check those files in.
- Property tests in `tests/` use `common::split_proptest!`, which splits
  their cases over several tests so that they run at the same time.
  `PROPTEST_CASES` still sets the total.
- The property tests that use the real git give each case a random set
  of git settings from `tests/common/settings.rs`, such as
  `diff.mnemonicPrefix` or `commit.cleanup`, because tergi runs git
  with the user's config. When you find another setting that changes
  what git prints or does, add it there.
