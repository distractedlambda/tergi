# Design notes

These documents explain how tergi works inside, and why. Each one covers
one part of the program. It names the files the code is in, explains
the choices that aren't obvious from the code, lists the tests, and ends
with the known gaps.

- [background-work.md](background-work.md): the event loop, background
  tasks, and how state survives reloads. Start here.
- [running-git.md](running-git.md): how tergi runs git, fetch, pull and
  push, and why git never prompts.
- [history.md](history.md): reading the refs, walking the history, and
  laying out the commit graph.
- [diffs.md](diffs.md): commit and working directory diffs, the rows of
  the detail pane, word marks and syntax highlighting.
- [staging.md](staging.md): loading the working directory, and staging,
  unstaging and discarding files, hunks and lines.
- [rewriting-history.md](rewriting-history.md): rewording, squashing,
  reordering and the rebase planner.
- [commands-and-input.md](commands-and-input.md): commands and actions,
  menus and dialogs, and how clicks find their target.
- [config-and-themes.md](config-and-themes.md): the config file and how
  themes are built.
- [watching.md](watching.md): refreshing when files change.
- [text-width.md](text-width.md): measuring and drawing text.
