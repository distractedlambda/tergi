# To fix

Known problems that nobody has fixed yet.

- **Rebase message files are left behind.** Rewording and squashing
  write each new message to a file in `.git/tergi/`, and nothing removes
  them after the rebase. The next rebase overwrites them, but files from
  a longer plan can stay. See `build_todo` in `src/git/rebase.rs`.
- **Stashes don't show their untracked files.** A stash is shown as its
  commit diffed against its first parent. Untracked files saved with
  `--include-untracked` live in its third parent, so they don't appear.
  Stashing from the top bar always includes them. See `commit::load` in
  `src/git/commit.rs`.
- **Staged files can get the wrong syntax colors.** We only have the
  full text of a commit's files and of unstaged and untracked files.
  Staged files are highlighted one hunk at a time, so a hunk that starts
  inside a multi-line comment or string is colored as code. Reading the
  index version with `git show :<path>` would fix it. See
  `section_texts` in `src/app/doc.rs`.
