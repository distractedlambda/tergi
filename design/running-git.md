# Running git

tergi reads the history and commit diffs with gitoxide, and does
everything else by running the `git` executable. This document explains
how tergi runs git, how fetch, pull and push work, and how git asks the
user for a password through tergi's own UI.

The wrapper is `Git` in `src/git/cli.rs`. The password prompts are in
`src/askpass.rs`, and the remote commands are in `run_cmd` in
`src/app/cmd.rs`.

## Why the CLI

Every write goes through `git`, so hooks, commit signing, credential
helpers, SSH config and the user's other settings behave the way they do
on the command line. The status and working tree diffs come from git
too. Its view of the index, the ignore rules and filters such as
line-ending conversion is the one that counts, and line staging has to
build patches that git will accept against that view.

gitoxide is used where speed matters and the result is only shown, not
written. That means the history walk and commit diffs.

## How a command runs

`Git::base_command` sets up every command the same way:

- It runs in the working directory, with stdin closed unless we have
  input for it.
- `GIT_TERMINAL_PROMPT=0` stops git from asking for credentials on the
  terminal, which tergi owns.
- `GIT_PAGER` and `PAGER` are `cat`, so nothing waits for a pager.
- `GIT_EDITOR=true`, so git never opens an editor on our terminal. A
  command that needs a message gets one through `-F`. History rewriting
  sets `GIT_SEQUENCE_EDITOR` instead (see `rewriting-history.md`).
- `-c color.ui=false -c core.quotepath=false`, so output has no color
  codes and paths with unusual characters aren't escaped.

It also removes `GIT_ICASE_PATHSPECS` from the environment. With it,
a path like `foo` would also match `Foo`, and git has no way to turn
that off for a single path.

Arguments go straight to git, not through a shell, so they need no shell
quoting. They still need care on git's side:

- Paths go after `--`, and `ops::pathspec` adds `:(literal)` to each
  one. Otherwise git would read `[ab].txt` as a pattern that also
  matches `a.txt`. If the user has set `GIT_LITERAL_PATHSPECS`, git
  already reads paths literally and would take the prefix as part of
  the name, so `ops::pathspec` leaves it off.
- Ref names and revisions go after `--end-of-options`. A ref name can
  start with a dash, and a remote can send us such a tag. Without it,
  git would read a tag named `-f` or a branch named `--exec=cmd` as an
  option.

There are two ways to run a command.

`Git::read` is for commands that don't change the repository. It sets
`GIT_OPTIONAL_LOCKS=0`. Without that, `git status` can rewrite the index
to update its cached file stats. That would wake our own file watcher
and cause another refresh. It also kills the process if
the task that asked for it is dropped, because nobody wants the answer
then. Reads aren't logged.

`Git::run` is for commands that may change something. It is never
killed early, so a command doesn't stop halfway through. Every run is
added to the command log, whether it succeeds or not.

Both write any input while they read the output. A large patch could
otherwise fill git's output pipe while we are still writing, and both
sides would wait for each other forever.

When a command fails, the error is git's stderr, or its stdout if
stderr is empty. That is what the error dialog shows.

### Names that aren't UTF-8

Git and Unix store file names as bytes, and those bytes needn't be
UTF-8. A file can have a name in Latin-1, for example. So tergi keeps
paths as bytes all the way from git's output back to git's arguments.
Paths in the repository are `BString`s, and arguments and environment
values are `OsStr`s, which on Unix hold any bytes. `util::fs_path` and
`util::path_bytes` convert between git's bytes and file system paths.
Nothing is lost on the way, so a command always gets the exact name git
gave us.

Ref names work the same way. Git allows a branch named `caf\xe9`
("café" in Latin-1), and remote names come from the config, which can
hold any bytes too. `Refs::load` reads `git for-each-ref` as bytes, so
the names in `Refs`, the history's labels and the commands that carry a
ref name are all `BString`s. Functions in `git/ops.rs` that take a ref
name take bytes and pass them on as `OsStr` arguments. The branch in
`Status` and the branch of a rebase in progress are bytes as well.

Names only turn into text when they are shown. Bytes that aren't UTF-8
then show up as replacement characters. Two different names can look
the same on screen this way, so the app never uses the text to find a
file or a ref again. It keeps the bytes. For the same reason, the
sidebar remembers which remotes are folded by their names as bytes.

Names the user types, such as a new branch or an upstream, are text,
because that's what the input box holds. Renaming a branch fills in the
old name as text, so a name that isn't UTF-8 shows up there with
replacement characters, and the user has to type the new name over it.

The same goes for the places where a path ends up in an environment
variable or a shell command: the askpass helper and its socket, the
sequence editor and the message files of a rebase, and the user's
editor. `shell_quote` quotes bytes. `main` reads its arguments and
`TERGI_ASKPASS_SOCK` with `args_os` and `var_os`, because `args` and
`var` panic or fail on values that aren't UTF-8.

The command log shows commands as text, so a name that isn't UTF-8
shows up there with replacement characters.

### The command log

`Git` keeps the last 500 commands it ran with `run`, with their output
and whether they succeeded. The `@` overlay shows them. It lets the user
see exactly what tergi did, and run it themselves if they want to.

## Fetch, pull and push

These can take a while, so they use `Git::run_streaming`. It reads
stderr as it arrives and passes each progress line to a callback. Git
separates progress updates with carriage returns, so both `\r` and `\n`
end a line. The operation shows the latest line next to its spinner in
the top bar. For the command log, `clean_progress` keeps only the final
state of each progress line, so the log doesn't fill up with hundreds of
percentages.

The commands are:

| Action | Command |
| --- | --- |
| Fetch | `git fetch --all --prune --progress` |
| Pull | `git pull --progress`, plus `--ff-only`, `--rebase --autostash` or `--no-rebase --no-edit` depending on the choice |
| Push | `git push --progress`, plus `--force-with-lease` for a force push |
| Push tag | `git push --progress --end-of-options <remote> refs/tags/<tag>` |

Push needs some care. Without a refspec, git pushes whatever is checked
out when it runs, and it respects settings like `remote.pushDefault` and
`branch.<name>.pushRemote`. We want to keep those settings, so we leave
the refspec out. But the user chose to push a particular branch, and
the confirmation for a force push names it. So the `Push` command
carries the branch name, and the task checks that the branch is still
checked out before it runs git. If it isn't, nothing is pushed.

A branch without an upstream gets `-u --end-of-options <remote>
<branch>`, where the remote is `origin` if there is one, or else the
first remote.

## Password prompts

When git or ssh needs a password, a token or a passphrase, it can run a
helper program instead of asking on the terminal. tergi is its own
helper.

At startup, `App::new` calls `askpass::start_server`. It binds a Unix
socket and returns a future that answers connections one at a time. The
socket's name holds the process id, a counter and a random number.
Remote commands get the environment from `Repo::remote_env`:

- `GIT_ASKPASS` and `SSH_ASKPASS` point at the tergi executable.
- `SSH_ASKPASS_REQUIRE=force` makes ssh use the helper even though it
  has a terminal.
- `TERGI_ASKPASS_SOCK` holds the socket address.

When git runs the helper, `main` sees `TERGI_ASKPASS_SOCK` before it
parses any arguments, and calls `askpass::client_main` with the prompt.
The helper connects to the socket and sends the prompt as one line. The
server passes the prompt and a oneshot channel to the app as
`AppEvent::AskPass`, and the app opens an input dialog. The text is
masked if the prompt mentions a password, a passphrase or a token.

The answer goes back over the socket as `1` followed by the text, or `0`
if the user cancelled. The helper prints the text and exits with
success, or exits with failure on a cancel, so git gives up.

### Where the socket lives

A socket file in the temporary directory stays there if tergi is
killed, or if an `App` is dropped without `main` cleaning up after it,
as in the tests. Removing it when something is dropped isn't reliable,
since dropping isn't guaranteed to happen. It would also do blocking IO
in a destructor. So the socket is set up so that a leftover can't pile
up.

On Linux the socket has an abstract name instead of a path. Abstract
names aren't files, and the kernel frees one as soon as the listening
socket closes, however the process ends. `TERGI_ASKPASS_SOCK` holds the
name with an `@` in front, which `askpass::connect` recognizes. File
permissions don't apply to abstract names, so any user on the machine
could connect and show a fake prompt, or read the answer. The server
checks each connection's user id with `peer_cred` and drops the ones
that aren't ours. The random part of the name stops other users from
taking the name before we do.

macOS has no abstract names, so there the socket is a file in the
temporary directory, which belongs to the user. `main` removes it when
tergi exits normally. When a server starts, it also removes the files
that other servers left behind, in a background thread. A file is stale
if connecting to it fails with "connection refused". A live server
accepts that connection, sees that it closes without a prompt, and
ignores it.

## The user's editor

"Open in editor" runs the editor git would use. `Git::editor` asks with
`git var GIT_EDITOR`, but without our own `GIT_EDITOR=true` in the
environment, which would otherwise be the answer. The editor runs
through `sh -c`, like git runs it, so an editor setting with arguments
works. The app suspends while it runs (see `background-work.md`).

## Tests

- `answers_a_password_prompt` in `tests/ui.rs` runs the real helper
  against the running app. It checks that the prompt shows up, that the
  answer is hidden on screen and reaches the helper, and that cancelling
  makes the helper fail.
- `force_push_only_pushes_the_branch_it_asked_about` in `tests/ui.rs`
  checks that a force push confirmed for one branch doesn't push another
  one that was checked out in the meantime.
- `src/git/cli.rs` has unit tests for cleaning up progress output, and
  for shell quoting. The quoting test runs the quoted words through `sh`
  and `bash --posix` and checks that they come back unchanged. Some of
  the words aren't UTF-8.
- `tests/askpass.rs` runs the real helper with a socket path and a
  prompt that aren't UTF-8. On Linux, when the tests run as root, it
  also runs the helper as another user and checks that the server
  ignores it.
- The unit tests in `src/askpass.rs` check that the abstract socket goes
  away with its server, that only stale socket files are removed, and
  that a connection without a prompt doesn't show one.
- `editor_setting_keeps_its_bytes` in `tests/ops.rs` checks that an
  editor setting that isn't UTF-8 comes back as it is.
- The property tests that use the real git, such as
  `tests/patch_props.rs` and `tests/rebase_props.rs`, write a random set
  of settings to each scratch repository's config. The list is in
  `tests/common/settings.rs`. It holds settings that change what git
  prints, such as `diff.mnemonicPrefix` and `diff.interHunkContext`, and
  settings that change what it does, such as `commit.cleanup`. The
  checks stay the same, because tergi should behave the same under all
  of them.
- `ref_names_that_are_not_utf8` in `tests/ops.rs` loads branches, tags
  and remotes whose names aren't UTF-8, and checks out, renames and
  deletes them. `branches_and_remotes_whose_names_are_not_utf8` in
  `tests/ui.rs` does the same from the sidebar, with two branches and
  two remotes that look the same on screen.

## Known gaps

- GPG and SSH signing only work if the agent can ask for the passphrase
  without the terminal, for example with a graphical pinentry. A
  terminal pinentry can't draw while tergi owns the screen.
- On macOS, if tergi is killed, its askpass socket file stays in the
  temporary directory until tergi runs again.
- Only one remote operation can run at a time, like every other
  operation.
