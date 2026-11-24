# To fix

Known problems that nobody has fixed yet.

## Working tree texts skip git's clean filters

`read_text` in `src/git/workdir.rs` says it reads a file "as git would
store it", but it reads the raw bytes. It doesn't apply `core.autocrlf`,
the `text` and `eol` attributes, `working-tree-encoding`, `ident` or
filter drivers such as git-lfs. `git diff` compares the index with the
filtered file, so the new side of an unstaged diff and its text can
disagree.

With `core.autocrlf = input` and a file with CRLF line endings, every
line of the text ends in a carriage return, but the index lines don't.
`view` in `src/git/context.rs` then finds no lines that both sides
share, so the edges of the file's unstaged hunks can't move out past
git's context. Highlighting checks each line against the diff and
allows the extra carriage return, but with other filters, the lines that
differ get no colors. This was found by reading the code and hasn't been
reproduced yet.

## Working directory diffs run textconv drivers

`git diff` runs the textconv driver of a file whose `diff` attribute
names one, such as `diff.num.textconv = cat -n`. The `DIFF` arguments in
`src/git/workdir.rs` don't pass `--no-textconv`, so the staged and
unstaged diffs show the converted text, not the file's lines. A patch
for line staging is built from those lines, so `git apply` can't apply
it. The lines also don't match the blobs that `read_blobs` reads, so
they get no colors. We checked what `git diff` prints, but haven't
tried staging such a line in tergi.

To see it, add `*.txt diff=num` to `.gitattributes`, set
`git config diff.num.textconv "cat -n"`, commit a `.txt` file and change
one of its lines. `git diff` shows each line with a number in front.
Commit diffs come from `git diff-tree` and imara-diff, which don't run
textconv, so they aren't affected.

## An empty root commit after resolving the first pick of a `--root` rebase

A rebase with `--root` replays the commits onto an empty commit that git
makes for the purpose. When the first pick applies cleanly, git makes
it a root commit. When the first pick conflicts and the user resolves
it and continues, git commits it on top of the empty commit instead. The
branch then starts with an empty commit that has an empty message.

To see it, make a repository with two commits where the second changes
a file that the first added. Then move the second commit down with
Ctrl+J, take either side and continue. This is git's behavior. The
command line does the same, and tergi doesn't work around it.
`tests/rebase_stop_props.rs` expects it.

## `G` starts a line selection in the detail pane

At the line level of the detail pane, `G` selects every line from the
cursor to where it moves, instead of only moving there.

To see it, select a commit or the working directory, go into the lines
of a hunk with `l` twice, and press `G`. The lines are marked as
selected, as with Shift+Down.

The cause is in `nav_key` in `src/app/input.rs`. It treats Shift with
any movement key as extending the selection. Crossterm reports an
uppercase letter with the Shift modifier, so `G` counts too. The
history only extends with Shift for keys that aren't letters.

## Changes in a nested repository that lost its `.git` folder

On Linux, the watcher doesn't watch nested repositories. When the
`.git` folder of one is deleted, the folder around it becomes an
ordinary untracked folder, but the watcher never starts watching it.
Changes in it go unnoticed until something else makes the watcher walk
the tree, such as a change to which folders git ignores.

To see it, make a folder `sub` in the working tree and run `git init`
in it. Start tergi, then delete `sub/.git` and write `sub/f`. The
status shows `sub/` as untracked after the next refresh, but later
writes in `sub` never cause a refresh.

The cause is in `src/watch.rs`. Events from inside `.git` folders are
always dropped, so the watcher doesn't see the deletion. And
`Watches::update` only walks the tree again when the ignored folders
change, which they don't here. macOS watches the whole tree, so it only
misses the deletion itself.

## A UI test of the message box can fail under load

`ui::commits_and_amends_from_the_message_box` in `tests/ui.rs` commits
from the message box, waits until the history shows three commits, and
then checks that the message box is empty. The message box is only
cleared when the app handles the commit's `OpDone` event
(`After::ClearMessage` in `on_op_done` in `src/app/mod.rs`). The file
watcher can see the new commit and reload the history before that
event arrives, so the check runs too early and fails. It failed once
in a full `cargo test` while other containers kept the machine busy,
and passed when run on its own. Waiting for the message box to empty,
or for `ops_done` to go up, would fix it.

## A quote in a fish comment inside braces hides a later `$(`

Inside braces or brackets, a fish word can have a comment in it, and
the comment ends at the end of its line. Fish finds the command
substitutions of a word first, and it skips the comment when it looks
for them. So in `echo {a, #'` on one line and `$(x)}` on the next,
`$(x)` is a command substitution. The fish lexer reads the `'` in the
comment as the start of a string, which goes on to the end of the
word, so `$(x)` gets the string color and `x` isn't an identifier. The
cause is in `parts` in `src/highlight/fish.rs`, which reads quotes in
the comment before it gets to the `$(`.

The fish reference check found this with `CASES=200000 SEED=11`. That
run also disagreed on two other random texts, which end in an unclosed
`(` that tergi lexes as commands and fish's highlighter doesn't. They
are `#not`, a line feed, `$()#(é]case ,-x`, a line feed, and
`!  in \t$(`, and the one line
`()#b[<time -command [\x0breturnfor &&-x\cA{{a,b}(|'--\cA' in `
(where `\x0b` is a vertical tab). Smaller texts with an unclosed `(`
don't show it, so the cause isn't known yet.
