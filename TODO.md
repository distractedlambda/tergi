# To fix

Known problems that nobody has fixed yet.

- Line staging in files that aren't valid UTF-8 can't work, because the
  diff text is converted lossily. `Git::read` in `src/git/cli.rs` does the
  conversion, so the patch we build no longer matches the file.

- Combining characters are overwritten when drawing code lines, in
  `draw_code` in `src/ui/draw.rs`.
