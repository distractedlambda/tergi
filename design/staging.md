# Staging

The working directory view lets you stage, unstage and discard whole
sections, files, hunks or single lines, and stash single files. This
document explains how tergi loads the working directory, how it turns a
selection into something git can apply, and the edge cases that shaped
the code.

The loading code is in `src/git/workdir.rs` and `src/git/status.rs`. The
patches are built in `src/git/patch.rs` and applied in `src/git/ops.rs`.
The app's side is `Unit` and `apply_unit` in `src/app/cmd.rs`, and
`unit_at_cursor` in `src/app/doc.rs`.

## Loading the working directory

`Workdir::load` runs these steps:

1. `git status --porcelain=v2 --branch -z`, with the arguments in
   `Status::ARGS`. It lists every untracked file, skips submodules that
   are only dirty, and lists ignored paths for the file watcher (see
   `watching.md`).
2. `git diff` for the unstaged changes, if the status lists any.
3. `git diff --cached` for the staged changes, if the status lists any.
   Before the first commit there is no HEAD to compare with, so it adds
   `--root`.
4. It reads each untracked and conflicted file itself and diffs it
   against nothing with imara-diff, so every line shows up as added.
5. It reads the full text of both sides of each staged and unstaged
   file, for syntax highlighting (see `diffs.md`).

Both diffs use `--no-ext-diff`, `--no-color`, `--no-renames` and
`--full-index`. The full blob ids are for reading the texts in step 5.
`Git` also sets `color.ui=false` and `core.quotepath=false` on every
command. Renames are off because a patch for line staging must name one
path. A staged rename therefore shows up as a deleted file and an added
file. They also pass `--src-prefix=a/` and `--dst-prefix=b/`, because
the user's `diff.mnemonicPrefix` or `diff.noprefix` would change the
prefixes that the parser strips from the paths.

`git diff` lists a conflicted file as an unmerged combined diff. We don't
show those. The parser marks them `too_large` and skips their lines, and
`Workdir::load` drops them from the unstaged list. The conflicted section
shows the whole file instead, conflict markers and all.

Parsing the diffs and diffing untracked files can take a while for big
changes, so that part runs on a blocking thread.

### Limits

Some changes are too big to show at once.

- A file diff with more than 20,000 lines (`DEFAULT_LINE_LIMIT`) is
  marked `too_large` and its hunks are dropped.
- Only the first 200 untracked files (`MAX_UNTRACKED_DIFFS`) are read.
  The rest are listed without contents.

The detail pane shows a notice for such files. Pressing Enter on it runs
`load_full`, which loads that one file again without the limits. When the
result comes back, `put_full_diff` finds the file again by its paths,
because the status may have reloaded in the meantime.

Files of any size are read, as long as they are text. A file counts as
binary if its first 8000 bytes contain a NUL byte. That is the same test
git uses. Git hasn't looked at an untracked or conflicted file, so
`read_worktree` reads its first 8000 bytes, and only reads the rest if
there's no NUL byte in them. That way a huge binary file isn't read in
full. For the other files, git's diff already says which are binary,
and we don't read those at all.

### Modes of new files

A patch that creates a file has to give its mode, and an untracked file
has no mode in git yet. `FileModes` works out the mode git would record.
A symlink is `120000`, and its contents are the path it points to. An
executable file is `100755`, unless `core.fileMode` is false. We only ask
git for `core.fileMode` once we find an executable file.

## What a selection turns into

The cursor and the line selection in the detail pane become a `Unit`:

- `Section`: everything in one section.
- `File` and `Files`: whole files, by path.
- `Hunk`: one hunk as the detail pane shows it, with the diff it came
  from. That's a range of git's hunks. Usually it holds one, but the
  user can join hunks by moving their edges (see "Moving the edges of
  hunks" in `diffs.md`). The patch still holds git's hunks as git gave
  them, so moving edges never changes what we pass to `git apply`.
- `Lines`: some changed lines, as `(hunk, line)` indexes into the diff
  they came from.

A unit can wait in a menu or a confirmation dialog while the status
reloads. So it names files by path, never by their place in a list. A
hunk or line unit keeps an `Arc` of its `FileDiff`, and the patch is
built from that copy. If the file has changed since then, `git apply`
refuses the patch, and the user sees git's error. That is better than
applying lines the user never saw.

Discarding a whole section asks first. When that dialog opens,
`run_cmd` turns the `Section` unit into a `Files` unit with the paths the
dialog names. A file that appears while the dialog is open is left alone.

Files, sections and stage-all go through plain git commands:

| Action | Command |
| --- | --- |
| Stage files | `git add -A -- <paths>` |
| Unstage files | `git restore --staged -- <paths>` |
| Unstage all | `git reset -q` |
| Discard tracked files | `git restore --worktree -- <paths>` |
| Discard untracked files | `git clean -f -q -- <paths>` |

Before the first commit there is nothing to restore from, so unstaging
uses `git rm --cached -r -f -q` instead. `--cached` keeps the files on
disk, and `-f` stops git from refusing when the file changed after it was
staged.

Git reads paths on the command line as patterns. `ops::pathspec` adds
`:(literal)` to any path with `*`, `?`, `[` or `\` in it, or one that
starts with a colon, so a command only touches the file we mean.

## Building a partial patch

Hunks and lines go through `build_patch`, which writes a patch with only
the selected changes, and `apply_lines`, which feeds it to `git apply`.
Staging applies it to the index with `--cached`. Unstaging applies it to
the index in reverse. Discarding applies it to the working tree in
reverse.

The trick is the one `git add -p` and `git gui` use. When we apply a
patch forward, the target file still has every line the diff removes.
So an unselected `-` line becomes context, and an unselected `+` line is
left out. In reverse, the target is the new side of the diff, and the
rule flips. An unselected `+` line becomes context, and an unselected `-`
line is left out.

Here is a hunk:

```diff
@@ -1,4 +1,4 @@
 a
-b
-c
+B
+C
 d
```

This is the patch for staging only `-b` and `+B`:

```diff
@@ -1,4 +1,4 @@
 a
-b
 c
+B
 d
```

`build_patch` then writes new hunk headers. On the side the patch applies
to, a hunk starts where the original hunk did. On the other side, it
moves by however many lines the earlier hunks in the patch added or
removed. We also pass `--recount`, so git counts the lines in each hunk
itself.

If the file doesn't exist on the side the patch applies to, the header
always says that the patch creates it. That happens when you stage lines
of an untracked file, or unstage or discard lines of a deleted one. The
file then holds only the lines you picked. The other way round, the
patch only deletes the file, or changes its mode, when every changed
line is selected. Otherwise the file stays, with the lines you didn't
pick.

A patch that deletes a file and is applied in reverse creates the file.
`git apply` then ignores the `deleted file mode` line and always makes a
plain file, so an executable would lose its executable bit. So when
`build_patch` writes such a patch, it adds a `new mode` line with the
same mode, which git does use.

### Bytes, not text

Diff lines are kept as display text, with bad UTF-8 replaced. When a
line isn't valid UTF-8, `DiffLine` also keeps its original bytes in
`raw`. Patches are built from `DiffLine::bytes`, so they match the file
exactly. The same goes for CRLF line endings, which stay in the line.

File names are bytes everywhere, and never display text (see
`running-git.md`). A patch header gets the name as it is, quoted the
way git quotes it with `core.quotePath` off. A `Unit` that names files
holds their bytes, so two names that only differ in bytes that aren't
UTF-8 stay apart, although they look the same on screen.

### The last line of a file

A line without a trailing newline must be the last line on its side of a
patch. Picking part of a change at the end of the file can break that.
Say the old file ends in `c` without a newline, and the new one adds the
newline. The diff removes `c` and adds `c`. Unstaging only the `-c` line
would need a `c` without a newline followed by another line, which isn't
a file. `build_patch` refuses such a selection and asks the user to
select the whole change at the end of the file.

### Patches without context

With `context_lines = 0`, the diffs have no context lines at all. Then
`git apply` can't check where a hunk belongs, and it refuses the patch
unless we pass `--unidiff-zero`. Without context, git places each hunk by
the start in its header, so that start has to be exact.

If the user sets `diff.interHunkContext`, git joins hunks that are that
close together, even with `-U0`. A joined hunk has context lines in the
middle but none at its ends. `git apply` then wants the hunk to sit at
the end of the file, because a hunk with context only lacks it there.
So `apply_lines` passes `--unidiff-zero` whenever every hunk starts and
ends with a changed line, rather than only when there is no context at
all. With more context, only a hunk that covers the whole file looks
like that.

There is one more wrinkle there. For an empty range, a hunk header
usually gives the line before it. But `git apply` guesses where a hunk
goes from the start of the side it applies towards, and takes that
number as it is. For a hunk that only removes lines, that guess is one
line too early. If the line there has the same text, git removes that
one instead. Usually that changes nothing. But if the removed line is
the last one and has no newline, git also drops the newline from the
line before it. `range_start` gives the exact line on that side to avoid
this.

### Conflicts

Line staging isn't offered for conflicted files. They are resolved by
taking one side or by marking the file as resolved.

`take_side` looks at the unmerged index entries with `git ls-files
--unmerged`. Stage 2 is our version and stage 3 is theirs. If the chosen
side has an entry, it checks out that side and stages it. If it doesn't,
that side deleted the file, so taking it means `git rm`. "Mark as
resolved" stages the file as it is.

## Stashing one file

`.S` stashes the file the cursor is in, staged and unstaged changes
together, and puts the file back the way it is in HEAD. `ops::stash_file`
does this.

`git stash push -- <path>` isn't enough. It only resets that file, but
the stash it makes still holds everything that is staged. Applying the
stash later brings back staged changes to other files, even ones the
user has thrown away since. So we build the stash ourselves, in a
scratch index in `.git/tergi/`. The commits are laid out the way git
lays out a stash, so `git stash` and the sidebar treat it like any
other:

- The index commit is HEAD with the file's index entry, or without the
  file if the index has none.
- The stash commit is that, with the file as it is on disk. An untracked
  file isn't in it.
- An untracked file goes in a third commit on its own, like
  `--include-untracked` does.

`git stash store` adds the stash to the list. Only then do we reset the
file, with `git restore --source=HEAD --staged --worktree`, or
`git clean` for an untracked file. If resetting fails, the file is left
as it was, and nothing is lost.

The stash is named after the message, or after the file. Whitespace in
the name is squeezed into single spaces, the way the reflog that lists
stashes does it anyway. A name that isn't UTF-8 gets replacement
characters, because git would read it as Latin-1.

Conflicted files and submodules are refused. So are files without
changes, and repositories without a commit.

## After an operation

Each of these runs through `App::spawn_op`, so only one runs at a time.
When it finishes, the app reloads the working directory. The detail pane
puts the cursor back on the same file and hunk by path (see
`background-work.md`), and the line selection is cleared.

## Tests

- `tests/staging.rs` stages, unstages and discards lines with the real
  git. It covers selections across hunks, new and deleted files, missing
  newlines, CRLF, unusual paths and files that aren't UTF-8.
  `every_selection_with_every_context_size` tries every selection of a
  few small diffs with context sizes from 0 up.
- `tests/patch_props.rs` makes random files and random edits, picks
  random lines, applies them with `apply_lines`, and checks the index or
  working tree against a small model. The model also says when a
  selection should be refused. A second test, `staging_in_steps`, runs a
  random sequence of steps on several files that can have staged and
  unstaged changes at once. Each step stages, unstages or discards a
  section, a file, a hunk or some lines through the functions in
  `ops.rs`. After each step it checks HEAD, the index and the working
  tree against the model, and checks what `Workdir::load` lists. Run
  them with `PROPTEST_CASES=5000` after changing `patch.rs`, `ops.rs` or
  `workdir.rs`. Their saved failures are in
  `tests/patch_props.proptest-regressions`.
- `tests/path_props.rs` makes files with odd names, such as names with
  spaces, quotes, tabs, newlines or ` b/` in them, and names that aren't
  UTF-8. It gives each file
  random versions in HEAD, the index and the working tree, including
  binary files, symlinks and executables. Then it checks the paths,
  modes and texts in `Workdir` against git's `-z` output, stages or
  unstages a line or all lines of each file, and checks the paths that
  `commit::load` gives for a commit.
- `tests/ops.rs` covers pattern-like paths, modes of untracked
  executables and symlinks, taking a side when one side deleted the
  file, and unstaging before the first commit.
- `tests/conflict_props.rs` gives a few files random versions in a base
  commit and two branches, and merges them. The versions are text,
  binary and executable files, symlinks, folders and missing files, so
  the merge leaves every kind of conflict. When a file meets a symlink
  or a folder, git moves one of them to a path like `f~other`. The test
  takes a random side on each unmerged path. Then it checks that the
  index and the working tree have the entry that git recorded for that
  side, or no file if the side has none, and that `Workdir::load` lists
  no conflicts.
- `tests/stash_file_props.rs` makes the files of `path_props.rs` and
  stashes one of them. It checks that the other files are left alone,
  that the stash commits hold only that file, and that popping the
  stash puts it back on disk.
- `tests/ui.rs` checks that a file or hunk waiting in the discard dialog
  stays the same one after a reload, and that discarding a section
  spares files that show up later.
  `stages_and_discards_files_whose_names_are_not_utf8` stages one of two
  files whose names look the same on screen, and discards a third, in a
  repository whose own path isn't UTF-8.

## Known gaps

- Renames show up as a deletion and an addition, because line staging
  needs one path per patch.
- Untracked files past the first 200 show no contents until you ask for
  them one at a time.
- A selection that splits a change to the last line of a file is
  refused rather than staged.
- A file whose deletion is staged, but which is still on disk as an
  untracked file, can be stashed but not popped again. Git won't put
  the untracked file back over the version from HEAD. `git stash -u`
  has the same problem.
- Conflicted files show their whole text as added lines. There is no
  three-way view, and you can't stage parts of them.
