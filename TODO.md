# To fix

Known problems that nobody has fixed yet.

- Line staging in files that aren't valid UTF-8 can't work, because the
  diff text is converted lossily. `Git::read` in `src/git/cli.rs` does the
  conversion, so the patch we build no longer matches the file.

- Combining characters are overwritten when drawing code lines, in
  `draw_code` in `src/ui/draw.rs`.

- In the planner, choosing reword or squash sets the row's action right
  away, then asks git for the commit message before opening the dialog.
  If another dialog opens in the meantime, `planner_set` in
  `src/app/planner.rs` skips its dialog, and the row keeps the new action
  without a message. Starting the rebase then fails with an error. The
  row should go back to pick, as it does when the dialog is cancelled.
