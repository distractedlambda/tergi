# Settings and themes

tergi has a few settings, which live in a TOML file that both the user
and tergi can change. This document explains how the file is read,
saved and watched, and how a theme is built from a published color
palette.

The settings are in `src/config.rs` and the themes in `src/theme.rs`.
The app's side is `apply_config` and `change_config` in
`src/app/mod.rs`.

## The file

The file is `config.toml` in a `tergi` folder under the user's config
folder, as the etcetera crate picks it. On both Linux and macOS that is
`~/.config/tergi/config.toml`, unless `XDG_CONFIG_HOME` says otherwise.

```toml
theme = "one-half-dark"
context_lines = 3
tab_width = 4
relative_dates = true
rainbow_identifiers = true
```

Every setting is optional. A missing setting takes its default, and so
does one the user removes from the file while tergi runs.

### Reading

`Config::parse` reads the text with serde. Numbers out of range are
brought into range, not rejected. The tab width goes from 1 to 16, and
the context lines from 0 to 50. `Config::check` then makes sure the
theme exists, which serde can't check.

At startup, `Config::load` never fails:

- A missing file gives the defaults.
- A file that isn't valid TOML gives the defaults, and the status bar
  shows the error.
- An unknown theme only resets the theme. The other settings are kept,
  and the status bar lists the themes that exist.

### Saving

Settings changed from the command palette are written back to the file.
`change_config` applies the new settings and then calls `Config::save`
with the old and new settings. Saving only writes the settings that
changed, and it edits the file with toml_edit rather than writing it out
from scratch. The user's comments, the order of the keys and any keys
we don't know about stay as they were. A comment on the same line as a
changed value is kept too.

Some details:

- If the file isn't valid TOML, saving refuses and says so. We can't
  edit a broken file without losing something.
- The new text is written to a temporary file next to the real one and
  renamed over it. Then the watcher, or another copy of tergi, never
  reads a half-written file. The temporary file gets the real file's
  permissions first.
- If the path is a symlink, for example into a dotfiles repository, we
  write to the file it points to. Renaming over the link would replace
  it with a plain file.
- If the folder doesn't exist yet, saving creates it.

Saving happens while the app handles the event, not in the background.
Writing one small file is quick, and doing it right away means the file
always matches the app's settings by the time the watcher reads it.

### Watching

`config::watch` watches the folder that holds the file, and the folder
of the file a symlink points to, if it is one. It only reacts to events
for files with the config file's name, and it ignores access events,
which our own reads cause.

Editors often save in several steps. So after an event, the watcher
waits until nothing has happened for 100 ms before it reads the file.

What happens next depends on the file:

- If it parses and its theme exists, the app gets the new settings. If
  they differ from the current ones, `apply_config` uses them and the
  status bar says so.
- If it's broken, or names an unknown theme, the app keeps the settings
  it has. The status bar shows the first line of the error.
- If it's missing, nothing happens. While an editor saves, the file can
  be gone for a moment.

The folder has to exist before it can be watched. If it doesn't exist
at startup, the app tries again after the first save creates it.

### Applying

`apply_config` only redoes what depends on the settings that changed. A
new theme swaps the colors. New context
lines reload the working directory and the commit on screen. The tab
width, the date style and rainbow identifiers are read every frame, so
they need nothing else.

## Themes

Each theme comes from a color scheme whose authors published its
palette, such as Catppuccin, Gruvbox or Solarized. The README lists
them all. `dark` and `light` in the config mean the One Half themes.

Each palette also has a `Syntax`, which gives a color to each kind of
token that the lexers find (see `diffs.md`). Operators and punctuation
can have no color, which leaves them in the text color. These colors
are written out by hand. They come from the scheme's TextMate theme,
the one bat and Sublime Text use. Each kind has the color that theme
gives the scope that comes closest, such as `constant.character.escape`
for escapes. A `Syntax` also has the color of `entity.name.function`.
No token gets it, but it helps set the colors for identifiers.

A palette is a `Palette`: a background, a few surface shades, text in
three strengths, an accent, red, yellow, green and magenta, and eight
colors for the graph lanes. Each palette's comments note the names its
authors gave those colors.

A palette doesn't cover everything we draw. There is no color for the
background of an added line, for example. `Theme::build` makes those by
mixing two palette colors, much as an editor draws a color at partial
opacity. An added line's background is 15% green over the background,
and a hunk header's is halfway between the background and a surface
shade.

Palette accents are made to be text colors, so text drawn on top of them
is often hard to read. Backgrounds for selections, word marks and ref
labels go through `tint` instead. It mixes in up to some share of the
color, but less if the text would lose too much contrast compared with
the plain background. It never goes below a floor, so the color still
shows. Some palettes, such as Solarized, have little contrast to spare,
and this is what keeps their selections readable.

Each theme also has up to 16 colors for rainbow identifiers, which
`identifier_colors` makes from its syntax colors. `diffs.md` explains
how.

Everything is 24-bit color.

### Previewing

The theme menu (`Choose theme…` in the palette) switches themes as you
move through it, without saving. `set_theme` changes the colors. The
highlighter only stores the kind of each token, and drawing looks up
the colors, so nothing has to be highlighted again. Closing the menu
without choosing puts the saved theme back. Choosing one saves it like
any other setting. `commands-and-input.md` explains how menus preview.

## Tests

- `src/config.rs` has unit tests for parsing and clamping, unknown
  themes, saving only what changed while keeping comments, creating the
  file and its folder, leaving a broken file alone, writing through a
  symlink, and the watcher.
- `src/theme.rs` checks that every theme can be found by its id, and
  that text on every mixed background keeps enough contrast.
- `tests/ui.rs` has `settings_follow_the_config_file`,
  `palette_commands_change_and_save_settings` and
  `the_theme_menu_previews_themes`.

## Known gaps

- Only these five settings exist. Keys, colors and the layout can't be
  configured.
- Published palettes don't all meet the usual contrast guidelines. We
  keep their colors anyway, and only make sure the backgrounds we mix
  don't make things worse.
- There is no fallback for terminals without 24-bit color.
