# Commands, keys and the mouse

Everything the user can do in tergi works from the keyboard and the
mouse, and most things also show up in a right-click menu, the `?` help,
the hint bar and the command palette. This document explains how one
list of actions drives all of these, how dialogs work, and how mouse
clicks find what they hit.

Commands and actions are in `src/app/cmd.rs`. Key and mouse handling is
in `src/app/input.rs`. Layout and hit regions are recorded in
`src/ui/draw.rs`.

## Commands

`Cmd` lists every action the user can take, such as `Stage(Unit)`,
`Reword(ObjectId)` or `Fetch`. A command carries everything it needs as
data. It can then wait in a menu or a dialog, and still mean the same
thing when it finally runs. `App::run_cmd` runs a command.

A command never holds a row number or a list index, because the lists
can change while it waits. It holds a commit id, a ref name or a path
instead. `background-work.md` explains why.

## One list of actions

Each pane builds a list of `ActionDef`s from its current selection:
`history_actions`, `detail_actions`, `sidebar_actions`, plus fixed lists
for the message box, the editor for reword and squash messages, and the
planner. `App::actions` picks the list for
the focused pane. `global_actions` adds the ones that work everywhere,
such as the palette and quit. `repo_actions` lists the git operations on
the whole repository, such as fetch, push and stage all.

An `ActionDef` has an optional key, a label, a command, the `via` that
says what comes before its key, and three flags:

- `enabled`: whether it can run now. Disabled actions are shown greyed
  out in menus.
- `menu`: whether it appears in the right-click menu.
- `hint`: whether it appears in the hint bar at the bottom.

`via` is one of three things. The keys of `Direct` actions work by
themselves. These are movement keys like `l` and `h`, Space for
folding, `v` for selecting lines or commits, and the global keys. `Dot`
actions act on the selection. Their keys only work in the menu that `.`
and the right mouse button open, so that letters like `d` can't clash
with movement. `Comma` actions are the
ones in `repo_actions`. Their keys work in the git menu that `,` opens.
The planner is the exception. Its actions are `Direct` and also in its
right-click menu, because setting actions on its rows is all it does.

That one list is used in six places:

- **Keys.** `handle_key` looks the key up in the focused pane's list,
  then in the global list. It runs the first enabled match whose `via`
  is `Direct`.
- **The right-click menu.** `open_context_menu` shows the pane's actions
  that have `menu` set. The `.` key opens the same menu near the
  selected row.
- **The git menu.** `,` runs `Cmd::RepoMenu`, which shows
  `repo_actions`. It works in every pane, whatever is selected there,
  and in the planner. Only the text boxes take it as text.
- **The hint bar.** `draw_bottom` shows the actions with `hint` set, as
  many as fit, and each one can be clicked. Each key is shown with what
  comes before it, such as `.r` or `,F` (`ActionDef::keys_label`).
  The global hints go at the right end, so that a pane with many hints
  can't push them off.
- **The help menu.** `?` shows `all_actions`, which is the pane's list
  plus the global one and the git menu. A command that is in more than
  one appears once if its key is the same. The help menu shows each key
  with what comes before it. Pressing a key there runs nothing, because
  a key can mean one thing after `.` and another after `,`.
- **The command palette.** Ctrl+P shows the enabled actions from
  `all_actions` and filters them with `util::fuzzy_score`.

Because the lists are built from the selection, the labels can say what
an action will do. The detail pane says "Stage hunk" or "Discard lines"
depending on what the cursor is on.

## Esc

Esc backs out of whatever is open. In a text box it cancels or leaves
the box, and in a menu or dialog it closes it. In the planner it
cancels the plan. In the history it ends a selection of commits. In the
detail pane, it first ends a line selection and then backs out one
level, like `h`, until the cursor is at the file level. `diffs.md`
explains the levels. Elsewhere it does nothing. The git menu has its
own key, `,`, so that it opens the same way however deep the cursor is
and whatever is selected.

To add a feature, add a `Cmd` variant, put an `ActionDef` for it in the
right pane's list, and handle it in `run_cmd`. Its key, menu entry, help
entry and palette entry then all come from that one place.

## Confirming

Commands that can lose work ask first. `run_cmd` calls `confirm_for`,
which returns a title, a message and a button label for commands such as
discarding changes, dropping a commit, a hard reset, deleting a branch,
tag or stash, and aborting a rebase. If it returns one, `run_cmd` opens
a `Confirm` dialog whose button runs `Cmd::Confirmed(the command)`, and
`run_cmd` runs that one without asking again.

The message names what will be lost, such as the files a discard will
throw away. Some commands are adjusted before the dialog opens, so the
dialog and the action agree. Discarding a whole section turns into
discarding the files the dialog lists (see `staging.md`).

## Overlays

Menus and dialogs are `Overlay`s, kept as a stack in `App::overlays`.
Only the top one gets keys and clicks. The kinds are:

- `Menu`: a list of actions, for right-click menus, the pull and push
  menus, help and the theme picker.
- `Confirm`: a yes or no question.
- `Input`: a text box with a `Purpose` that says what to do with the
  text, such as naming a branch.
- `Palette`: the command palette.
- `Log`: the command log.
- `Message`: an error or other long message.

A menu that the mouse opens appears at the pointer, or below the button
that opened it, if there is one. A menu that the keyboard opens appears in the middle of
the screen, like the dialogs do. `App::by_mouse` says whether the input
being handled came from the mouse. The one exception is the right-click
menu. The `.` key opens it next to the selected row, because the menu
acts on that row.

A menu can preview its items. The theme menu sets `preview` to
`Cmd::RevertTheme`. Moving through the items runs each one's
`Cmd::preview`, which switches the theme without saving it. Closing the
menu without choosing runs `RevertTheme`, which puts the saved theme
back.

An input box always ends with `submit_input` or `cancel_input`.
Cancelling the message for a reword or squash in the planner turns the
row back into a pick.

## Drawing and hit testing

`ui::draw::draw` works in two phases.

First, `layout` works out where everything goes and stores the
rectangles in `App::areas`. That covers the sidebar, the history, the
detail pane, the message box and the planner. Keyboard handling needs
these too, for example to know how far a page down should move. Layout
also brings scroll offsets back into range after a resize or a reload.

Then everything is drawn. Each thing that reacts to the mouse is
recorded in `App::hits` as a rectangle and an `input::Target`, while it
is drawn. Targets are rows (`HistoryRow(i)`, `DocRow(i)` and so on),
buttons that carry a `Cmd`, the dividers between panes, menu items, and
text boxes. The only buttons are in dialogs, the hint bar and the banner
for an operation that stopped. Everything else is done from a menu.

`target_at` looks up a click in that list, from the end, so whatever was
drawn last wins. Drawing a dialog clears the list first, so while a
dialog is open, nothing behind it can be clicked or lit up by the mouse.
A dialog also records its whole area as `Target::Overlay`. A click
inside the dialog that misses its buttons then does nothing, instead of
closing it.

So anything clickable has to be added to `hits` when it is drawn. A
button that is drawn but not recorded can't be clicked, and one that is
recorded but covered by something drawn later can't be reached either.

The mouse pointer shape comes from the same list. `pointer_shape` gives
a hand over buttons, a text cursor over text boxes and a resize arrow
over dividers. The main loop sends it to the terminal with OSC 22 when
it changes.

### Clicks

- A left click selects. Two clicks on the same cell within 400 ms count
  as a double click, which opens or checks out.
- A right click selects the row and opens its menu. A right click
  inside the selected lines or commits keeps the selection, so the menu
  acts on all of them.
- Shift with a click in the history selects the commits from the
  selected one to the one clicked. A click without Shift ends the
  selection.
- Dragging over diff lines selects them. Dragging past the top or bottom
  edge scrolls.
- Dragging a hunk's header or the bar under it moves that edge of the
  hunk (see "Moving the edges of hunks" in `diffs.md`).
- Dragging a divider resizes the panes. Dragging a planner row moves it.
- Clicks are looked up in the hits of the last frame. The main loop
  handles every queued event before it draws again, so a click can
  name a row that an earlier event in the same batch removed. Clicks
  on detail rows that are gone do nothing.
- A diagonal swipe on a trackpad scrolls both ways at once. So after a
  scroll step on one axis, steps on the other axis are ignored for
  100 ms (`SCROLL_LOCK`). Shift with the wheel counts as sideways.
- Clicking outside a menu, the palette or the command log closes it.
  Clicking outside a dialog that asks something, such as a confirmation
  or a text box, does nothing. Then the question isn't lost by
  accident. The same goes for the reword and squash message editor in
  the detail pane (see `rewriting-history.md`).

## The terminal

`terminal::setup` turns on raw mode, the alternate screen, mouse capture,
bracketed paste and focus events. If the terminal supports the kitty
keyboard protocol, it also asks for unambiguous key codes. Then Esc
works without a delay, and keys like Ctrl+Enter can be told apart from
Enter. `terminal::restore` undoes all of this. It also runs from the
panic hook, so a crash doesn't leave the terminal in raw mode.

Paste arrives as one event and goes into whichever text box has focus.
Focus events make the app refresh, since the user often changed
something in another window. Copying a commit hash uses OSC 52, so it
works over SSH too. `App::handle` never writes to the terminal itself.
It leaves the text in `App::clipboard`, and the main loop writes it
out.

## Tests

`tests/ui.rs` drives the app with real key and mouse events and finds
click targets through `App::hits`. These tests are about the rules
above:

- `clicks_behind_a_dialog_are_ignored` and
  `buttons_under_a_dialog_do_not_light_up`.
- `actions_on_the_selection_need_a_dot_first`,
  `esc_backs_out_one_level_at_a_time`,
  `comma_opens_the_git_menu_everywhere` and
  `menus_from_the_keyboard_open_in_the_middle`.
- `the_detail_pane_moves_by_file_then_hunk_then_line` and
  `moving_by_hunks_opens_folded_files`, for the levels.
- `right_click_selects_without_activating` and
  `right_click_in_the_planner_opens_one_menu`.
- `the_theme_menu_previews_themes` and
  `palette_commands_change_and_save_settings`.
- `draws_at_any_size`, which draws the main screens and dialogs at
  many small sizes to catch layout code that panics.
- `clicks_on_detail_rows_that_are_gone_do_nothing`.

`tests/ui_props.rs` runs random sessions. It sends random keys, clicks
and right clicks on random entries in `App::hits`, drags, scrolls,
pastes and resizes, and runs random commands from the palette. It
checks that only the top dialog's targets are left in `App::hits` while
a dialog is open, and that menus and the palette select an item that
exists.

## Known gaps

- Keys can't be changed.
- The hint bar shows as many hints as fit and drops the rest. There is
  no sign that some are missing.
