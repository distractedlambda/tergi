# tergi

tergi is a git client for the terminal, modeled on Sublime Merge. It's
built for browsing history, reading diffs, staging hunks and single
lines, and rewriting commits. Everything works with both the mouse and
the keyboard.

It runs on Linux and macOS. It looks best in a terminal with truecolor
and the kitty keyboard protocol, such as ghostty, kitty or WezTerm.

## Building

You need a recent stable Rust toolchain, a C compiler for the
tree-sitter grammars, and `git` on your `PATH`.

```sh
cargo install --path .
```

## Running

```sh
tergi            # the repository in the current directory
tergi path/to/repo
```

## The window

- The **sidebar** on the left lists branches, remotes, tags and stashes.
  Click a ref to jump to it in the history. Double-click a branch to
  check it out.
- The **history** in the middle shows the commit graph. The first row is
  the working directory.
- The **detail pane** on the right shows the selected commit with its
  full diff. For the working directory, it shows the commit message box
  and your changes, grouped into conflicts, untracked, unstaged and
  staged files.
- The **top bar** has Fetch, Pull, Push and Stash buttons, and a command
  palette.

Drag the dividers between panes to resize them. When a rebase, merge,
cherry-pick or revert stops partway, a banner appears with Continue,
Skip and Abort buttons.

## Mouse

- Click to select, and double-click to open or check out.
- Right-click anything for a menu of what you can do with it.
- Drag over diff lines to select them, then click "Stage lines" or
  "Unstage lines".
- Scroll with the wheel. Hold Shift to scroll a diff sideways.
- In the rebase planner, drag commits to reorder them.

## Keys

The bottom bar shows the most useful keys for the focused pane. Press
`?` for the full list, or Ctrl+P to search for any command by name.
Right-click menus show keys too.

| Key | What it does |
| --- | --- |
| `j` / `k`, arrows | Move down and up |
| `g` / `G` | Go to the top or bottom |
| Ctrl+D / Ctrl+U | Move by half a page |
| Tab, `1` `2` `3` | Switch panes |
| `.` | Open the menu for the selected item |
| `?` | Show all keys |
| Ctrl+P | Command palette |
| `w` | Show the working directory |
| `t` | Switch between unified and side-by-side diffs |
| `F` / `p` / `P` | Fetch, pull, push |
| `S` | Stash your changes |
| `@` | Show the git commands tergi has run |
| `q` | Quit |

In the working directory:

| Key | What it does |
| --- | --- |
| Space | Stage or unstage the file, hunk or selected lines |
| `v` | Start selecting lines |
| `d` | Discard (asks first) |
| `a` / `u` | Stage all, unstage all |
| `c` | Write a commit message. Ctrl+Enter or Ctrl+S commits. |
| `A` | Toggle amend |
| `z` / `Z` | Fold a file, or all files |
| `]` `[` / `}` `{` | Next and previous file or hunk |
| `o` | Open the file in your editor |

On a commit in the history:

| Key | What it does |
| --- | --- |
| `r` | Reword |
| `s` / `f` | Squash or fix up into the parent |
| `d` | Drop |
| `e` | Stop at the commit so you can edit it |
| Ctrl+K / Ctrl+J | Move the commit up or down |
| `A` | Amend the staged changes into this commit |
| `i` | Open the rebase planner from this commit |
| `b` / `T` | New branch or tag here |
| `C` / `V` | Cherry-pick or revert |
| `X` | Reset the current branch to here |
| `y` | Copy the hash |

## Rewriting history

The single-commit actions above run `git rebase -i` for you, without
opening an editor. The rebase planner (`i`) lets you plan several
changes at once. Set an action for each commit with `p`, `r`, `s`, `f`,
`d` or `e`, reorder with Ctrl+J and Ctrl+K or the mouse, and press
Enter to run it. tergi asks for the new messages for rewords and
squashes before it starts.

Local changes are stashed during a rebase and restored at the end.
Ranges that contain merge commits are refused, because tergi doesn't
try to recreate merges.

If a rebase stops on a conflict, the conflicted files appear at the top
of the working directory. Resolve each one with Ours, Theirs, or by
editing it and clicking Resolved. Then press Alt+C to continue, or
Alt+A to abort.

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
```

You can also change each setting from the command palette (Ctrl+P):
"Choose theme…", "Set context lines…", "Set tab width…", and "Show full
dates in history" or "Show relative dates in history". tergi saves the
change to the config file. It only rewrites the line for that setting,
so your comments and the rest of the file stay as they are.

tergi watches the config file while it runs, so edits you make by hand
take effect right away. If an edit leaves the file broken, for example
with a TOML syntax error or a theme that doesn't exist, tergi keeps the
settings it had and says so in the status bar. If the config file is a
symlink, tergi writes to the file the link points to.

## Themes

Each theme uses the colors that a popular color scheme publishes, and
the same scheme's colors for syntax highlighting. To try them out, press Ctrl+P and
run "Choose theme…". The theme changes as you move through the list.
Press Enter to keep one and save it to the config file, or Esc to go
back.

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
- Syntax highlighting covers Bash, C, C++, C#, CSS, diffs, Elixir, Go,
  Haskell, HTML, INI, Java, JavaScript, JSON, Lua, Make, Markdown, Nix,
  OCaml, PHP, Protocol Buffers, Python, Racket, Ruby, Rust, Scala,
  Swift, TOML, TypeScript, XML, YAML and Zig. Files in other languages
  are shown without colors.

## License

MIT. See [LICENSE](LICENSE).
