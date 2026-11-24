# Tergi

Tergi is yet another terminal Git client.

> [!WARNING]
> This project is vibe-coded; essentially all code (and much of the documentation) is AI-written. Testing is limited to my own personal use, and the automated test suite (which was written by Claude).

## What? Why?

I wanted a git client that worked exactly the way I liked, while natively supporting terminal-based workflows. In an ordinary world this would be no more than a passing wish, but we live in a strange time of suspiciously-cheap and surprisingly-competent coding agents.

This is a tool for myself first and foremost, but I'm keeping the code public ~~to get free time on GitHub Actions runners~~ in case others share my odd sensibilities.

Functionality may change or break, without notice, according to my whims. If you want something to work differently, fork Tergi and set your own agent on it!

### Similarities with [Sublime Merge][1]

You may notice some striking similarities between Tergi and Sublime Merge. This is not an accident; Sublime Merge was my daily driver until I ~~created~~ had Claude create Tergi. The starting point for Tergi's design was essentially "Sublime Merge as a TUI".

With that said, **Tergi is not intended to be a Sublime Merge clone**. Functionality has diverged wherever I wanted something to work differently, and Sublime Merge has many features that Tergi does not. More importantly, Sublime Merge is a polished and supported software product that in turn financially supports real human developers. **If you want a better Git client and don't happen to need a TUI, please consider [Sublime Merge][1]**.

## Building

You need a recent stable Rust toolchain and `git` on your `PATH`.

```sh
cargo install --path .
```

CI also builds binaries on every push, for macOS on Apple silicon and for
Linux on x86_64 and aarch64. The Linux binaries link musl statically, so
they run on any distribution. Each push to `main` gets a build on the
[Releases](https://github.com/distractedlambda/tergi/releases) page,
tagged `build-` and the first 12 digits of the commit hash. Builds of
other branches are on each run's page under the Actions tab.

## Running

```sh
tergi            # the repository in the current directory
tergi path/to/repo
```

## The window

- The **sidebar** on the left lists branches, remotes, tags and stashes.
  Click a ref to jump to it in the history. Double-click a branch to
  check it out.
- The **history** in the middle shows the commit graph. Each commit
  takes two rows: the summary and its labels, then the author, the date
  and the short commit id. The first entry is the working directory.
  Each stash shows up on top of the commit it was made on.
- The **detail pane** on the right shows the selected commit with its
  full diff. For the working directory, it shows the commit message box
  and your changes, grouped into conflicts, untracked, unstaged and
  staged files. While several commits are selected, it shows the
  changes between the first and the last of them.
- The **top bar** shows the repository, the current branch and its
  upstream, and what tergi is busy with.

Drag the dividers between panes to resize them. When a rebase, merge,
cherry-pick or revert stops partway, a banner appears with Continue,
Skip and Abort buttons.

## Mouse

- Click to select, and double-click to open or check out.
- Right-click anything for a menu of what you can do with it.
- Drag over diff lines to select them, then right-click them to stage,
  unstage or discard them.
- Drag a hunk's header up to show more lines above it, or down to hide
  them. The thin bar under each hunk does the same for the lines below
  it. A hunk that grows into the next one joins it.
- Scroll with the wheel. Hold Shift to scroll the file or commit message
  under the pointer sideways. Each one keeps its own position, and it
  stops once the end of its longest line is in view.
- In the rebase planner, drag commits to reorder them.
- Click a key in the bottom bar to run it.

## Keys

The keys work like vim's. Movement keys work by themselves. Actions on
the selected item come after `.`, which opens the menu for it. Git
operations on the whole repository come after Esc, which opens a menu
of them. Both menus list their keys, and a second Esc closes them.

The focused pane has a colored border. The bottom bar shows the most
useful keys for it. Press `?` for the full list, or Ctrl+P to search
for any command by name.

| Key | What it does |
| --- | --- |
| `h` / `l`, arrows | Move to the pane on the left or right |
| `j` / `k`, arrows | Move down and up |
| `g` / `G` | Go to the top or bottom |
| Ctrl+D / Ctrl+U | Move by half a page |
| Tab, `1` `2` `3` | Switch panes |
| `.` | Open the menu for the selected item |
| Esc | Open the git menu |
| `?` | Show all keys |
| Ctrl+P | Command palette |
| `t` | Switch between unified and side-by-side diffs |
| `@` | Show the git commands tergi has run |
| Ctrl+B | Show or hide the sidebar |
| `q` | Quit |

### The detail pane

The detail pane has three levels. At first, `j` and `k` move between
files, and the commit's details at the top. `l` or Enter goes into the
selected file's hunks, where `j` and `k` move from hunk to hunk, on into
the next file. Another `l` or Enter goes into the hunk's lines. `h` or
Esc goes back out one level, and `h` from the files goes back to the
history.

| Key | What it does |
| --- | --- |
| Space | Fold or unfold the file |
| `Z` | Fold or unfold all files |
| `v` | Start selecting lines. Esc ends the selection. |
| Shift+arrows | Select lines |
| `]` `[` / `}` `{` | Next and previous file or hunk |
| `K` / `J` | Move the hunk's top edge up or down |
| Ctrl+J / Ctrl+K | Move the hunk's bottom edge down or up |
| `.=` | Put the file's hunks back the way git shows them |
| Enter | On a parent commit, jump to it. On a large diff, show it. |

Moving the edges of a hunk shows the unchanged lines around it, or
hides its context lines. This only changes what you see. When two
hunks meet, they become one, and staging it stages both. The edges stay
where you put them when the working directory reloads.

tergi remembers which files you folded, even after you quit. Each
commit has its own folds. In the working directory, staged and unstaged
changes have separate folds. When you stage a whole file, its staged
changes stay folded if its unstaged changes were, and the other way
around when you unstage it.

In the working directory:

| Key | What it does |
| --- | --- |
| `.` Space | Stage or unstage the file, hunk or selected lines |
| `.d` | Discard (asks first) |
| `.S` | Stash the file, with its staged and unstaged changes |
| `.o` | Open the file in your editor |

### The git menu

| Key | What it does |
| --- | --- |
| Esc `F` / `p` / `P` | Fetch, pull, push |
| Esc `S` | Stash your changes |
| Esc `a` / `u` | Stage all, unstage all |
| Esc `c` | Write a commit message. Ctrl+Enter or Ctrl+S commits. |
| Esc `A` | Toggle amend |
| Esc `w` | Show the working directory |
| Esc `r` | Refresh |
| Esc `C` / `s` / `X` | Continue, skip or abort a rebase, merge, cherry-pick or revert that stopped |

In the message box, Esc leaves the box instead, and Alt+A toggles amend.

### On a commit in the history

| Key | What it does |
| --- | --- |
| `.r` | Reword |
| `.s` / `.f` | Squash or fix up into the parent |
| `.d` | Drop |
| `.e` | Stop at the commit so you can edit it |
| `.` Ctrl+K / Ctrl+J | Move the commit up or down |
| `.A` | Amend the staged changes into this commit |
| `.i` | Open the rebase planner from this commit |
| `.b` / `.T` | New branch or tag here |
| `.C` / `.V` | Cherry-pick or revert |
| `.X` | Reset the current branch to here |
| `.y` | Copy the hash |

Reword and squash open the message for editing at the top of the detail
pane, where the commit's message is normally shown. Ctrl+Enter saves it
and Esc cancels.

On a stash, in the history or the sidebar, `.` Space applies it, `.g`
pops it and `.d` drops it. `.H` hides it from the history, or shows it
again.

In the sidebar, Space folds a section, and `.` Space checks out the
selected branch. `.H` hides the selected branch or remote branch from the
history, or shows it again.

### On several commits in the history

Press `v` to start selecting commits, then move with `j` and `k`.
Shift with the arrow keys, or Shift with a click, selects too. Esc or
`v` ends the selection.

| Key | What it does |
| --- | --- |
| `.s` | Squash them into one commit, with a new message |
| `.f` | Fix them up into the oldest, keeping its message |
| `.d` | Drop them |
| `.C` / `.V` | Cherry-pick or revert them |
| `.y` | Copy their hashes |

Squash, fixup and drop only work on commits of the current branch.

While commits are selected, the detail pane shows the changes from the
commit at the bottom of the selection to the one at the top. They can
be on different branches, so this is how you compare any two commits.
It compares the two snapshots, as `git diff A B` does. So the changes
that the bottom commit made itself aren't part of it. Click the `From`
or `To` line to copy that commit's hash.

## Hiding branches and stashes

Hide a branch with `.H` in the sidebar when its commits get in the way.
Its commits leave the history, unless another ref you can see leads to
them, and its label goes away. The sidebar still lists it, with a `⊘` in
front. Press `.H` on it again to bring it back, or run "Show all hidden
branches and stashes" from the command palette.

Stashes work the same way. Press `.H` on one in the sidebar or the
history to hide it. tergi remembers which stash it was even when pushing
or dropping other stashes changes its `stash@{n}` name.

The checked out branch can't be hidden, since HEAD shows its commits
anyway. tergi remembers hidden branches for each repository, even when
the branch is deleted, so a remote branch you hid stays hidden when a
fetch brings it back. They are saved in
`~/.local/share/tergi/state.redb`, or under `XDG_DATA_HOME` if you set
it, and not in the repository.

## Rewriting history

The single-commit actions above run `git rebase -i` for you, without
opening an editor. The rebase planner (`.i`) lets you plan several
changes at once. Set an action for each commit with `p`, `r`, `s`, `f`,
`d` or `e`, reorder with Ctrl+J and Ctrl+K or the mouse, and press
Enter to run it. Esc cancels the plan. tergi asks for the new messages
for rewords and squashes before it starts.

Local changes are stashed during a rebase and restored at the end.
Ranges that contain merge commits are refused, because tergi doesn't
try to recreate merges.

If a rebase stops on a conflict, the conflicted files appear at the top
of the working directory. Resolve each one from its menu with `.O`
(ours) or `.T` (theirs), or edit it and mark it resolved with `.M`.
Then press Esc `C` to continue, or Esc `X` to abort. The banner's
buttons do the same.

## Remotes and passwords

Fetch, pull and push run the `git` command, so your credential helpers,
SSH config and hooks all work as usual. tergi never asks for passwords
or passphrases itself. Store HTTPS credentials in a credential helper,
and load SSH keys into ssh-agent. If git or ssh needs to ask for
something, the operation fails and the error says what it wanted. The
same happens when ssh meets a host it doesn't know yet. Connect to it
once with `ssh` in a terminal to add its key to `known_hosts`.

If you have set `SSH_ASKPASS` to a graphical program, ssh uses it for
its questions.

GPG and SSH signing work if your agent can ask for the passphrase
without the terminal, for example with a graphical pinentry. A
terminal pinentry such as `pinentry-curses` can't draw while tergi
owns the screen.

## Watching for changes

tergi watches the working tree and the `.git` directory and refreshes
by itself when files change, including changes made by other programs.
On Linux it skips ignored directories such as `target/` and
`node_modules/`, so it doesn't use up inotify watches on them. If
watching fails, tergi falls back to checking every few seconds and says
so in the status bar. It also refreshes whenever the terminal window
gets focus.

## Configuration

tergi reads `~/.config/tergi/config.toml` on Linux. On macOS it uses
the same path under `~/.config`. Every setting is optional.

```toml
theme = "one-half-dark" # see the list below
context_lines = 3       # lines of context around each change
tab_width = 4
relative_dates = true   # "3 days ago" instead of a date in the history
rainbow_identifiers = true  # give each name in code a color of its own
```

You can also change each setting from the command palette (Ctrl+P).
The commands are "Choose theme…", "Set context lines…", "Set tab
width…", "Show full dates in history" (or "Show relative dates in
history"), and "Color identifiers by name" (or "Stop coloring
identifiers by name"). tergi saves the change to the config file. It only rewrites the line for that setting,
so your comments and the rest of the file stay as they are.

tergi watches the config file while it runs, so edits you make by hand
take effect right away. If an edit leaves the file broken, for example
with a TOML syntax error or a theme that doesn't exist, tergi keeps the
settings it had and says so in the status bar. If the config file is a
symlink, tergi writes to the file the link points to.

## Themes

Each theme uses the colors that a popular color scheme publishes, and
syntax colors that follow the scheme's own. For now, only C, C++,
CMake, JavaScript, HipHop.js, JSON, Python, Rust, shell scripts, fish scripts,
Dockerfiles, Zig, LLVM IR, Racket, Rhombus, Scribble, TOML, YAML,
Markdown and `.gitignore` files get syntax colors. To try them out, press Ctrl+P and run "Choose theme…". The
theme changes as you move through the list. Press Enter to keep one
and save it to the config file, or Esc to go back.

Names in C, C++, CMake, JavaScript, HipHop.js, Python, Rust, shell, fish,
Dockerfile, Zig, LLVM IR, Racket, Rhombus and Scribble code get a color picked from
the name itself, so each use of a variable has the same color. So do the keys in JSON, TOML
and YAML files. Keywords keep their own color. Each
theme has its own set of these colors. To draw the other names in the
text color instead, set `rainbow_identifiers = false`.

| Name in the config | Theme |
| --- | --- |
| `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`, `catppuccin-mocha` | [Catppuccin](https://catppuccin.com) |
| `dracula` | [Dracula](https://draculatheme.com) |
| `gruvbox-dark`, `gruvbox-light` | [Gruvbox](https://github.com/morhetz/gruvbox) |
| `nord` | [Nord](https://www.nordtheme.com) |
| `one-half-dark`, `one-half-light` | [One Half](https://github.com/sonph/onehalf) |
| `solarized-dark`, `solarized-light` | [Solarized](https://ethanschoonover.com/solarized) |

`dark` and `light` also work. They mean `one-half-dark` and
`one-half-light`.

## Limitations

- tergi opens one repository at a time.
- There is no search, file history or blame yet.
- Merge conflicts are resolved by picking a side or editing the file.
  There is no three-way merge view.

## License

MIT. See [LICENSE](LICENSE).

[1]: https://www.sublimemerge.com
