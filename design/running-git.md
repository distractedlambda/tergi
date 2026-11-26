# Running git

tergi does everything by running the `git` executable, reads as well
as writes. This document explains
how tergi runs git, how fetch, pull and push work, and why git and ssh
never ask the user anything.

The wrapper is `Git` in `src/git/cli.rs`. The remote commands are in
`run_cmd` in `src/app/cmd.rs`.

## Why the CLI

Every write goes through `git`, so hooks, commit signing, credential
helpers, SSH config and the user's other settings behave the way they do
on the command line. The status and working tree diffs come from git
too. Its view of the index, the ignore rules and filters such as
line-ending conversion is the one that counts, and line staging has to
build patches that git will accept against that view.

Reads go through git as well, even the history walk and commit diffs,
which need to be fast. That way tergi shows what `git log` and
`git show` would show. Git applies the user's rename settings and
`refs/replace`, finds the repository the way the user's other git
commands do, and fetches missing objects in a partial clone.
`history.md` and `diffs.md` explain how those two stay fast.

## How a command runs

`Git::base_command` sets up every command the same way:

- It runs in the working directory, with stdin closed unless we have
  input for it.
- `GIT_TERMINAL_PROMPT=0` stops git from asking for credentials on the
  terminal, which tergi owns.
- `SSH_ASKPASS_REQUIRE=force` and `SSH_ASKPASS=false` do the same for
  ssh. See "No prompts" below.
- `GIT_PAGER` and `PAGER` are `cat`, so nothing waits for a pager.
- `GIT_EDITOR=true`, so git never opens an editor on our terminal. A
  command that needs a message gets one through `-F`. History rewriting
  sets `GIT_SEQUENCE_EDITOR` instead (see `rewriting-history.md`).
- `-c color.ui=false -c core.quotepath=false`, so output has no color
  codes and paths with unusual characters aren't escaped.
- `-c i18n.logOutputEncoding=UTF-8`, so commit messages and stash
  names come out as UTF-8 whatever encoding the user asks git to print
  them in.

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

The history walk reads git's output as it comes, with `Git::stream`.
That starts git and hands back a `Stream`, whose output the caller
reads. Other tasks write the input and collect stderr, so git never
waits for us. `Stream::finish` waits for git and turns a failure into
an error. Dropping a `Stream` before that kills git, which is how a
walk stops when a newer one replaces it.

Loading a commit asks two processes that keep running between loads,
`git diff-tree --stdin` and `git cat-file --batch`.
`Git::start_process` starts one, with the same settings as `read`.
`Git` keeps one of each kind while it waits for the next request, and
`stop_processes` stops them when the app refreshes, because git reads
the config only when it starts. `diffs.md` explains how this works.

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
variable or a shell command: the sequence editor and the message files
of a rebase, and the user's editor. `shell_quote` quotes bytes. `main`
reads its arguments with `args_os`, because `args` panics on values
that aren't UTF-8.

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
the push menu names it. So the `Push` command
carries the branch name, and the task checks that the branch is still
checked out before it runs git. If it isn't, nothing is pushed.

A branch without an upstream gets `-u --end-of-options <remote>
<branch>`, where the remote is `origin` if there is one, or else the
first remote.

## Fast-forward

The menu for a commit ahead of the current branch can fast-forward the
branch to it (`history.md` says how tergi knows it is ahead). This runs
`git merge --ff-only --no-autostash --end-of-options <commit>`. Like
push, the `FastForward` command carries the branch's name, and the task
checks that it is still checked out first, because git moves whatever
is checked out.

A fast-forward must never lose uncommitted changes. git updates the
working tree as a checkout does: changes to files that the new commits
don't touch stay, and if a staged, unstaged or untracked change is in
the way, git refuses and changes nothing. The error dialog then lists
the files. `--no-autostash` matters here. With `merge.autoStash` set,
git would instead stash the changes, fast-forward, and apply the stash
again, which can stop with conflicts and leave the changes in the
stash.

`fast_forward_keeps_uncommitted_changes` in `tests/ops.rs` covers each
of these cases, with and without `merge.autoStash`, and
`fast_forward_fails_rather_than_lose_changes` in `tests/ui.rs` checks
the error dialog.

## No prompts

git and ssh sometimes want to ask the user something: an HTTPS
username or password, a key's passphrase, or whether to trust a host
key they haven't seen. By default they ask on the terminal. tergi has
the terminal in raw mode on the alternate screen, so a prompt there
would scramble the display, and tergi would read the keys meant for it.

tergi doesn't answer these questions. It makes git and ssh fail
instead, and the error dialog shows what they wanted. Users keep
credentials in a credential helper and keys in ssh-agent, which need no
prompt.

- `GIT_TERMINAL_PROMPT=0` makes git fail when no credential helper
  has an answer.
- ssh opens `/dev/tty` itself, even though its stdin is closed.
  `SSH_ASKPASS_REQUIRE=force` makes it run the `SSH_ASKPASS` program
  for every question instead. tergi sets that to `false`, which always
  fails, so ssh gives up. If the user has set `SSH_ASKPASS` already,
  tergi keeps it, because that program shows its own window.

Every command gets these, not only the remote ones. SSH commit signing
runs `ssh-keygen`, which asks for a passphrase the same way ssh does.

## The user's editor

"Open in editor" runs the editor git would use. `Git::editor` asks with
`git var GIT_EDITOR`, but without our own `GIT_EDITOR=true` in the
environment, which would otherwise be the answer. The editor runs
through `sh -c`, like git runs it, so an editor setting with arguments
works. The app suspends while it runs (see `background-work.md`).

## Tests

- `force_push_only_pushes_the_branch_the_menu_named` in `tests/ui.rs`
  checks that a force push chosen from the menu for one branch doesn't
  push another one that was checked out in the meantime.
- `src/git/cli.rs` has unit tests for cleaning up progress output, and
  for shell quoting. The quoting test runs the quoted words through `sh`
  and `bash --posix` and checks that they come back unchanged. Some of
  the words aren't UTF-8.
- `ssh_never_asks_on_the_terminal` in `tests/ops.rs` fetches through
  a fake ssh command and checks that ssh would run the askpass program,
  and that the program fails.
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
- `SSH_ASKPASS_REQUIRE` needs OpenSSH 8.4 or newer. An older ssh
  ignores it and asks on the terminal when no display is set.
- Credential helpers that ask on the terminal themselves, rather than
  through git, can still draw over tergi.
- Only one remote operation can run at a time, like every other
  operation.
