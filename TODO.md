# To fix

Known problems that nobody has fixed yet.

- **Staged files can get the wrong syntax colors.** We only have the
  full text of a commit's files and of unstaged and untracked files.
  Staged files are highlighted one hunk at a time, so a hunk that starts
  inside a multi-line comment or string is colored as code. Reading the
  index version with `git show :<path>` would fix it. See
  `section_texts` in `src/app/doc.rs`.
