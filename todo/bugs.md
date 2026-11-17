# To fix

Known problems that nobody has fixed yet.

## Highlighting JavaScript can hang forever

A JavaScript line with a value, a space and a block comment makes
syntect loop forever. `a = 1 /* c */`, `return x /* c */` and
``x = `a` /* c */`` all do it. So does ``"`a` /*"``, with no end to the
comment. `f() /* c */`, `a = 1; /* c */` and ``a = `s` // c`` don't.
The highlighter handles one job at a time, so after this no file gets
colors for the rest of the session. A blocking thread also spins at
full CPU until tergi exits, because nothing can stop it.

To see it, run `highlight_lines` with the JavaScript syntax on
`"a = 1 /* c */\n"`. It never returns, in debug or release builds.

The cause is two rules in the JavaScript syntax from two-face 0.5.2
(bat 0.26.1), together with syntect 5.3.0. After an expression, one
context has a lookahead for "only spaces and comments until the end of
the line", roughly `(?=(?:\s+|/\*.*?\*/)*(?:/\*.*)?$)`. It `set`s
another context without consuming anything. That context has a
lookahead for a binary operator after optional spaces, `(?=\s*[-+*/%...])`.
The `/` of the comment matches it, so it `set`s the first context back,
again without consuming anything. `ParseState::parse_next_token` in
syntect only breaks loops where a push that consumes nothing is
followed by a pop that consumes nothing. Its own comment says that a
`set` can loop too, but it doesn't check for that. Without the space,
the comment rule in the prototype matches first at the same position,
so there's no loop.

To trace it, copy syntect into a scratch folder, point a
`[patch.crates-io]` entry at it, and print each match in
`parse_next_token` (position, stack depth, operation and regex). The
two contexts were 148 and 11 of syntax 37 in that build.

Things to find out before fixing it:

- Does upstream syntect or bat already know about this? A fix in
  syntect that also breaks loops of `set`s would be best. Sublime Text
  moves on by one character when it sees such a loop.
- Until then, tergi could guard against it. Some options are a patched
  syntect, editing the syntax before use, or highlighting in a separate
  process that we can kill after a timeout.
- Do other syntaxes, such as TypeScript or JSX, have the same loop?

The property test in `src/highlight.rs` leaves JavaScript out because of
this. Put `a.js` back in its `PATHS` once it's fixed.

## Copies are found from the wrong version of a changed file

With `diff.renames = copies`, a commit diff looks for copies among the
files that the commit changed. Git compares a new file with the old
version of each changed file. Gix compares it with the new version. So
tergi and git disagree about copies whenever the source of the copy
also changed.

To see it, commit a file `a`. Then in one commit, change `a` and add `b`
with the old content of `a`. Git shows `b` as a copy of `a`, and tergi
shows it as an added file. If `b` has the new content of `a` instead,
tergi shows a copy, and git only does if the old and new `a` are
similar enough. When both find a copy, tergi diffs `b` against the new
version of `a` rather than the old one. And when tergi finds the copy,
the change to `a` itself is missing from the list. Git lists `a` as
modified and `b` as a copy, but tergi only lists the copy.

The cause is in gix-diff 0.70. `Tracker::emit` matches copies by
`change.id()`, which for a modification is the new blob. We would need
a fix upstream, or our own copy detection. `tests/commit_props.rs`
leaves copies out until then.

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
