# Rewriting history

tergi can reword, squash, fix up, drop, edit and reorder commits, either
one at a time from the history or several at once from the rebase
planner. All of it runs `git rebase -i`, but git never opens an editor.
This document explains how that works and what tergi refuses to do.

The git side is in `src/git/rebase.rs`. The planner is in
`src/app/planner.rs`, and the commands that start each action are in
`run_cmd` in `src/app/cmd.rs`.

## Why `git rebase -i`

We could rewrite commits ourselves with gitoxide, but then hooks,
signing, `rebase.*` settings and the reflog would behave differently
from the command line. Running git keeps them the same. It also means a
rebase that stops on a conflict is an ordinary git rebase. The user can
finish it in tergi, or from a shell with `git rebase --continue`.

## A plan

Every rewrite starts as a `Plan`. That is a list of `Step`s, oldest
first, and a `base` commit to replay them onto. A `base` of `None` means
the range starts at the root commit, and the rebase runs with `--root`.

`plan_from(oldest)` makes a plan with every commit from `oldest` up to
HEAD, all set to `Pick`. It refuses in these cases:

- `oldest` is a merge commit.
- `oldest` isn't an ancestor of HEAD, so it isn't on the current branch.
- There is a merge commit anywhere in the range.

tergi doesn't try to recreate merges. `git rebase` would flatten them by
default, and `--rebase-merges` needs a todo list that this code doesn't
build.

## The todo list

`build_todo` writes the todo list for a plan. Most actions map straight
to a todo verb:

| Action | Todo lines |
| --- | --- |
| `Pick` | `pick <id>` |
| `Drop` | `drop <id>` |
| `Edit` | `edit <id>` |
| `Fixup` | `fixup <id>` |
| `Reword(msg)` | `pick <id>`, then `exec git commit --amend … -F <file>` |
| `Squash(msg)` | `fixup <id>`, then `exec git commit --amend … -F <file>` |

Git's own `reword` and `squash` verbs open an editor for the message, and
we don't want git to take over the terminal. So a reword picks the commit
and then amends it with the new message, which we wrote to a file in
`.git/tergi/`. A squash melds the commit in with `fixup`, which keeps the
older commit's message, and then amends it with the combined message.

The amend runs with these options:

- `--allow-empty`, so rewording an empty commit works.
- `--no-verify`, because git doesn't run the commit hooks for the other
  commits it replays either.
- `--cleanup=whitespace`, so a message line that starts with `#`, such
  as `#42 Fix the crash`, isn't taken for a comment. `ops::commit` uses
  the same setting for the same reason.

The commits that git replays without our help need the same care. When
the user sets `commit.cleanup`, git cleans up the message of every
commit it replays, and with `strip` it removes lines like `#42 Fix the
crash` from commits that only moved.

We can't just turn cleanup off, though. When a rebase stops, git
prepares the next message with comments in it, and expects them to be
stripped. After a conflict it lists the files that conflicted. When the
user stages changes at an `edit` step and continues, git amends the
commit with its usual template. When a fixup conflicts, the message
starts as `# This is a combination of 2 commits.` with both messages
below. A user at the command line would see these in an editor, but
tergi's `GIT_EDITOR=true` accepts them as they are.

So every rebase that tergi starts, continues or skips runs with
`-c commit.cleanup=strip -c core.commentChar=<U+0001>`. Git strips its
comments, which now start with that control character, and leaves
lines that start with `#` alone. The cleanup also strips trailing
whitespace and extra blank lines from the messages of the commits that
git replays, which `git commit` would have done to them anyway.

`build_todo` refuses plans that git can't finish, through `check`:

- The oldest step is a squash or a fixup. There is nothing before it to
  meld into.
- A reword or squash has a message that is empty after whitespace
  cleanup. `git commit` refuses an empty message, so the `exec` line
  would fail and stop the rebase partway.
- The plan starts at the root and drops every commit. A rebase with
  `--root` replays the commits onto an empty commit that git makes for
  the purpose. With nothing to replay, the branch would end up on that
  commit, because git can't leave a branch with no commits.

## The sequence editor

`run` writes the todo list to `.git/tergi/todo` and starts

```sh
git -c commit.cleanup=strip -c core.commentChar=<U+0001> rebase -i --autostash <base>
```

with `GIT_SEQUENCE_EDITOR` set to `tergi __seq-editor <our todo>`. Git
writes its own todo list and runs that command with the list's path
added. `main` sees `__seq-editor` before it parses any arguments, and
calls `seq_editor_main`, which replaces git's list with ours.

Git runs the editor and the `exec` lines with `sh -c`, so
`shell_quote` quotes the paths in both. It uses the `shlex` crate, and
works on bytes, because the repository and the tergi executable can
have paths that aren't UTF-8. For the same reason the todo list is
bytes, and `seq_editor_main` reads both lists as bytes. Git's list also
holds the commit subjects as they are, so they needn't be UTF-8
either.
Writing our own is easy to get subtly wrong. For example, bash expands
`{a,b}` even when it runs as `/bin/sh`, as it does on macOS. A todo line
can't hold a line break, so `build_todo` refuses when the path of the
message files has one.

Before it does, it checks that both lists name the same commits. Git's
list uses short ids, so it matches them by prefix. If they differ, the
branch moved after tergi made the plan. Then the editor fails, git
doesn't start the rebase, and the user sees why.

`Git` sets `GIT_EDITOR=true` on every command. So if git does want an
editor for something we didn't plan for, it gets an empty answer rather
than taking over the terminal.

`--autostash` stashes uncommitted changes before the rebase and applies
them again at the end. Without it, git would refuse to start when the
working tree isn't clean.

The message files have to stay while the rebase is stopped, because the
`exec` lines that read them haven't run yet. So `finish` deletes
`.git/tergi/` only once no rebase is in progress, and `abort_rebase`
deletes it too. A rebase that the user continues on the command line
leaves the folder behind until tergi's next rebase ends.

## How a rebase ends

A rebase can finish, or stop partway. It stops at an `edit` step, and on
a conflict. git exits with an error on a conflict, but that isn't a
failure from the user's point of view. So `finish` doesn't trust the
exit code alone. It looks at the repository afterwards with `state`. If
a rebase is still in progress, the outcome is `Stopped`, whatever git
returned. If not, git's result stands.

That only works if no rebase was in progress before we started. So
`ensure_idle` refuses to start while a rebase, merge, cherry-pick or
revert is unfinished. Git would refuse too, but `finish` would then see
the old rebase and report that the new one stopped.

## The state banner

`state_in` reads the git directory to find out whether something is in
progress. It looks for `rebase-merge/` or `rebase-apply/`, and then for
`MERGE_HEAD`, `CHERRY_PICK_HEAD` and `REVERT_HEAD`. For a rebase it also
reads the branch name, the step and the commit it stopped at. These are
small files, so the app reads them while it handles an event, on every
refresh.

When the state isn't `Clean`, the banner shows it, with Continue, Skip
and Abort buttons. The git menu that `,` opens has the same three. A
merge has no Skip. Each button runs `continue_op`,
`skip_op` or `abort_op`, which call the matching git command. Continue
and Skip for a rebase go through `finish` again, so they can stop once
more.

Skipping needs care with rewords and squashes. Each one is a `pick` or
`fixup` followed by the `exec` line that amends the message. If the
pick or fixup conflicts and the user skips it, git would still run the
`exec` line, and the new message would go to the commit before. So
`skip_rebase` first looks at the last line git did and the next one it
will do. If they are a pick or fixup and our `exec` line, it takes the
`exec` line out of `.git/rebase-merge/git-rebase-todo`. A user who
skips from the command line doesn't get this.

## Single-commit actions

The actions in the history menu each build a plan and change one step:

- `reword` amends HEAD directly when the commit is HEAD. It uses
  `--only`, so staged changes stay out of the commit. Otherwise it runs a
  plan from the commit with that step set to `Reword`.
- `single` sets one step to `Fixup`, `Drop` or `Edit`. A fixup melds the
  commit into its parent, so the plan starts at the parent.
- `move_commit` swaps the commit with the one above or below it. Moving
  it down needs the parent in the plan, so the plan starts there.
- `fixup_staged_into` commits the staged changes with
  `git commit --fixup=<target>`, then runs `git rebase -i --autosquash`
  with `GIT_SEQUENCE_EDITOR=true`, which accepts git's own todo list. If
  the rebase didn't start, it resets the fixup commit away, so the
  changes are staged again as they were. If the rebase stops, it writes
  the id of the fixup commit to `.git/rebase-merge/tergi-fixup`.
  Aborting goes back to that commit, so `abort_rebase` then resets it
  away too. Git deletes the folder when the rebase ends, so the file
  can't outlive the rebase it belongs to.

Reword and squash need a message first. `run_cmd` reads the current
message in the background and opens it in an editor, `App::edit`. For
a squash, the editor starts with the parent's message followed by this
one.

The editor isn't a dialog. It is drawn at the top of the detail pane,
where the commit's message would be, and `Detail::hide_message` leaves
the message out of the header below it. The rest of the commit stays in
view. While the editor is open it gets every key, and clicks outside it
do nothing, as they do around a dialog. Ctrl+Enter saves and Esc
cancels. The hint bar shows both, and they can be clicked.

The editor belongs to the commit that the detail pane showed when the
command ran, which `MessageEdit::commit` remembers. It only opens if the
pane still shows that commit when git answers, and no dialog or planner
has opened, because otherwise the user has moved on. If the pane moves
to another commit while the editor is open, `show_source` closes it and
says so. That happens when the commit leaves the history, for example
because something outside tergi rewrote the branch.

## Several commits

Pressing `v` in the history starts a selection of commits. `History`
keeps the commit it started at in `anchor`, and the selection runs from
there to the selected row. Like the selection itself, the anchor is a
commit id, so a reload can't move it to another commit. When the walk
ends without finding the anchor, usually because a rebase rewrote it,
the selection ends. Otherwise it would come back if the commit came
back, for example after an abort.

While there is a selection, `marked_actions` replaces the actions for
one commit. Each command carries the selected ids, newest first. The
detail pane shows the changes from one end of the selection to the
other, which `diffs.md` describes.

- Drop, fixup and squash go through `several`. It finds the oldest of
  the commits with `git merge-base --octopus`, which gives back one of
  them only if all the others descend from it. Then it makes a plan from
  that commit and checks that every selected commit is in it, so that
  they are all on the current branch.
- `apply_to_several` sets the steps. A drop drops each commit. A fixup
  or squash melds the others into the oldest, which keeps its place. A
  squash puts its message on the newest commit, so that git amends the
  message once everything is melded in. Melding refuses commits that
  have unselected ones between them, because those would have to move.
  A selection in the history can't have gaps on the current branch,
  since the history is in topological order, but `several` doesn't rely
  on that.
- The message editor for a squash starts with the messages of all the
  commits, oldest first.
- Cherry-pick and revert pass all the commits to one git command, which
  applies them in the order given. Cherry-picks go oldest first, and
  reverts newest first.

The selection ends when one of these commands runs. Cancelling the
squash message or the question before a drop keeps it.

## The planner

The planner (`i` in the history) edits a whole plan before running it.
`Planner::new` takes a plan from `plan_from` and shows the rows newest
first, like the history. `to_plan` turns them back, oldest first.

Each row has a `PlanAction` and, for reword and squash, a message.
Choosing reword or squash asks for the message right away. The planner
fills the screen, so there is no detail pane to edit the message in, and
it asks in a dialog instead. `planner_set`
reads the commit's message in the background. For a squash, it puts the
message of the row below first, using the message already written for
that row if there is one. When the read comes back, the row may have
moved. So the code finds the row again by its commit id, and only opens
the dialog if the row still wants a message. If another dialog opened in
the meantime, it sets the row back to `Pick`, as cancelling the dialog
would.

Changing a row's action drops its message, because a message written
for a reword doesn't fit a squash.

`to_plan` checks what a reorder can break:

- A reword or squash row without a message, or with a blank one.
- A squash or fixup at the bottom, with nothing below it.
- A squash or fixup on top of a dropped commit.

It also runs `check`, so the planner refuses everything that
`build_todo` would refuse, and stays open to show why.

Enter runs the plan with `rebase::run` through `spawn_op`, and the
planner closes.

## Tests

`tests/rebase.rs` runs each action against a scratch repository with
four commits. It covers rewording HEAD, an older commit and the root
commit, message lines that start with `#`, drop, fixup, squash, moves,
edit with continue, a planner-style plan, autostash, a conflict that
stops and aborts, skipping a reword that conflicts, amending staged
changes into an older commit and aborting that, refusing merges in the
range, and refusing to start during another rebase. It also drops,
fixes up and squashes several commits, and checks that `several`
refuses commits that are apart or off the branch.
`reword_where_paths_are_not_utf8` rewords and squashes in a repository
whose path isn't UTF-8, with tergi run from a folder whose name isn't
UTF-8 either.
`message_files_are_removed_when_the_rebase_ends` checks that the message
files stay while the rebase is stopped and go away when it ends.

`tests/rebase_props.rs` is a property test. It makes a straight history
in which each commit adds its own file, so nothing can conflict. Then it
moves planner rows around at random, gives them random actions and
messages, and runs the plan with the real git. A model works out which
commits should come out, with which messages and files, and which plans
the planner must refuse. The messages have `#` lines, blank lines,
quotes and non-ASCII text.

`tests/rebase_stop_props.rs` covers rebases that stop. Most of its
commits change one of three lines in a shared file, so reordering,
dropping and melding them can conflict. It starts each rebase through
the planner, one of the single-commit actions, or `several` on a run
of commits. At a conflict it takes
a side with `take_side`, edits the file, skips or aborts. At an `edit`
step it continues, amends the commit, skips or aborts. A model replays
the todo list with a three-way merge of each line and file. It says
where the rebase stops, what `state_in` reports there, and which
commits and messages come out. After an abort, the branch, index and
working tree must be as they were before, including uncommitted changes
that `--autostash` put away.

`src/app/planner.rs` has unit tests for the row order and the oldest
commit check. Its property test checks that moves only reorder the
rows, and that `build_todo` accepts every plan that `to_plan` accepts. In `tests/ui.rs`,
`the_planner_asks_for_a_message_after_its_row_moves` and
`a_reword_without_its_dialog_goes_back_to_pick` cover the planner's
dialogs. `edits_the_message_where_the_commit_shows_it` and
`the_message_editor_closes_when_its_commit_goes_away` cover the message
editor in the detail pane, and
`rewords_a_commit_and_runs_a_plan` runs the whole flow from the keyboard.
`squashes_commits_selected_with_v` and
`selects_commits_with_shift_and_the_mouse` cover selecting several
commits.

The tests set `repo.exe` to `CARGO_BIN_EXE_tergi`, because during a test
the current executable is the test runner, not tergi.

## Known gaps

- Ranges with merge commits are refused.
- Moving a commit with Ctrl+J or Ctrl+K runs a whole rebase for one
  step. The planner is the way to make several moves at once.
