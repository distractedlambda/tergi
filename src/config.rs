//! User configuration, read from `~/.config/tergi/config.toml`.
//!
//! ```toml
//! theme = "one-half-dark" # see `Theme::list` for the others
//! context_lines = 3       # lines of context around changes
//! tab_width = 4
//! relative_dates = true   # "3 days ago" instead of "2026-11-08"
//! ```
//!
//! Settings changed from inside tergi are written back to the file. We
//! edit the file rather than write it out from scratch, so the user's
//! comments and layout stay as they were. While tergi runs, it watches the
//! file and picks up changes made by hand.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::Deserialize;
use tokio::sync::mpsc::unbounded_channel;
use toml_edit::{DocumentMut, Item, Value};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// A theme id. Unknown ones are reported when loading.
    pub theme: String,
    pub context_lines: u32,
    pub tab_width: usize,
    pub relative_dates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: Theme::DEFAULT.to_string(),
            context_lines: 3,
            tab_width: 4,
            relative_dates: true,
        }
    }
}

pub const MAX_CONTEXT_LINES: u32 = 50;
pub const MAX_TAB_WIDTH: usize = 16;

impl Config {
    pub fn path() -> Option<PathBuf> {
        use etcetera::BaseStrategy;
        etcetera::choose_base_strategy()
            .ok()
            .map(|s| s.config_dir().join("tergi").join("config.toml"))
    }

    /// Load the config file. A missing file gives the defaults. A broken one
    /// gives the defaults plus an error message to show. An unknown theme
    /// only resets the theme.
    pub fn load(path: &Path) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match Config::parse(&text) {
                Ok(mut c) => match c.check() {
                    Ok(()) => (c, None),
                    Err(e) => {
                        c.theme = Theme::DEFAULT.to_string();
                        (c, Some(format!("{}: {e}", path.display())))
                    }
                },
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => (Config::default(), None),
        }
    }

    /// Parse the text of a config file. Numbers out of range are brought
    /// into range rather than rejected.
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut c: Config =
            toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string())?;
        c.tab_width = c.tab_width.clamp(1, MAX_TAB_WIDTH);
        c.context_lines = c.context_lines.min(MAX_CONTEXT_LINES);
        Ok(c)
    }

    /// Check what parsing can't, which is that the theme exists.
    pub fn check(&self) -> Result<(), String> {
        if Theme::named(&self.theme).is_some() {
            return Ok(());
        }
        let known: Vec<&str> = Theme::list().map(|(id, _)| id).collect();
        Err(format!(
            "there's no theme called \"{}\". The themes are {}.",
            self.theme,
            known.join(", ")
        ))
    }

    /// Write the settings that differ between `old` and `new` to the file
    /// at `path`, leaving the rest of the file alone. We won't touch a file
    /// that isn't valid TOML, because we can't edit it without losing
    /// something.
    pub fn save(path: &Path, old: &Config, new: &Config) -> Result<()> {
        // Write to the real file if the path is a symlink, for example into
        // a dotfiles repository. Replacing the link would cut it off.
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let text = match std::fs::read_to_string(&target) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("could not read {}", target.display()));
            }
        };
        let mut doc: DocumentMut = text.parse().map_err(|e| {
            anyhow::anyhow!(
                "{} isn't valid TOML, so it was left alone.\n\n{e}",
                target.display()
            )
        })?;
        if old.theme != new.theme {
            set(&mut doc, "theme", new.theme.as_str());
        }
        if old.context_lines != new.context_lines {
            set(&mut doc, "context_lines", i64::from(new.context_lines));
        }
        if old.tab_width != new.tab_width {
            set(
                &mut doc,
                "tab_width",
                i64::try_from(new.tab_width).unwrap_or(4),
            );
        }
        if old.relative_dates != new.relative_dates {
            set(&mut doc, "relative_dates", new.relative_dates);
        }
        write_replacing(&target, doc.to_string().as_bytes())
    }
}

/// Set a top-level key, keeping any comment that sits on the same line.
fn set(doc: &mut DocumentMut, key: &str, value: impl Into<Value>) {
    let mut value = value.into();
    if let Some(old) = doc.get(key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    doc[key] = Item::Value(value);
}

/// Replace a file's contents in one step. Writing it in place could let the
/// watcher, or another copy of tergi, read it half written.
fn write_replacing(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("the config file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let name = path.file_name().context("the config file has no name")?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);
    let r = std::fs::write(&tmp, contents).and_then(|()| {
        if let Ok(m) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, path)
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("could not write {}", path.display()))
}

/// Wait this long after the last change before reading the file, since
/// editors often save in several steps.
const QUIET: Duration = Duration::from_millis(100);

/// Watch the config file. Each time it changes, the future reads it and
/// passes the parsed config, or why it couldn't be used, to `report`.
/// The watcher has to be kept alive for as long as the watching should go
/// on. The future ends once the watcher is dropped.
///
/// The folder holding the file has to exist. If the file is a symlink, we
/// also watch the folder of the file it points to.
pub fn watch(
    path: &Path,
    report: impl Fn(Result<Config, String>) + Send + 'static,
) -> Result<(
    RecommendedWatcher,
    impl Future<Output = ()> + Send + 'static,
)> {
    let mut dirs = HashSet::new();
    let mut names = HashSet::new();
    let targets = [Some(path.to_path_buf()), std::fs::canonicalize(path).ok()];
    for p in targets.into_iter().flatten() {
        if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
            dirs.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
            names.insert(name.to_os_string());
        }
    }
    let (tx, mut rx) = unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        // Reading the file causes access events, including our own reads.
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        if ev
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| names.contains(n)))
        {
            let _ = tx.send(());
        }
    })
    .context("could not start a file watcher")?;
    for dir in &dirs {
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("could not watch {}", dir.display()))?;
    }
    let path = path.to_path_buf();
    let run = async move {
        while rx.recv().await.is_some() {
            loop {
                tokio::select! {
                    more = rx.recv() => if more.is_none() { return },
                    () = tokio::time::sleep(QUIET) => break,
                }
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(text) => report(Config::parse(&text).and_then(|c| c.check().map(|()| c))),
                // While an editor saves, the file can be missing for a
                // moment. A file that stays missing leaves the settings as
                // they are, which is also what an empty one would mean
                // for anything changed from inside tergi.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => report(Err(e.to_string())),
            }
        }
    };
    Ok((watcher, run))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fills_in_defaults_and_clamps() {
        let c = Config::parse("tab_width = 0\ncontext_lines = 900\n").expect("parse");
        assert_eq!(c.tab_width, 1);
        assert_eq!(c.context_lines, MAX_CONTEXT_LINES);
        assert_eq!(c.theme, Theme::DEFAULT);
        assert!(Config::parse("tab_width = \"wide\"").is_err());
        assert!(Config::parse("theme = ").is_err());
    }

    #[test]
    fn unknown_themes_fail_the_check() {
        let c = Config::parse("theme = \"plaid\"").expect("parse");
        assert!(c.check().is_err_and(|e| e.contains("plaid")));
        assert!(
            Config::parse("theme = \"dark\"")
                .expect("parse")
                .check()
                .is_ok()
        );
    }

    #[test]
    fn load_keeps_other_settings_when_the_theme_is_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"plaid\"\ntab_width = 8\n").expect("write");
        let (c, err) = Config::load(&path);
        assert_eq!(c.theme, Theme::DEFAULT);
        assert_eq!(c.tab_width, 8);
        assert!(err.is_some());
        let (c, err) = Config::load(&dir.path().join("missing.toml"));
        assert_eq!(c, Config::default());
        assert!(err.is_none());
    }

    #[test]
    fn save_changes_only_what_changed_and_keeps_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let text = "# My settings\ntheme = \"nord\" # cool colors\n\ntab_width = 2\nrelative_dates = false\n";
        std::fs::write(&path, text).expect("write");
        let old = Config::parse(text).expect("parse");
        let new = Config {
            theme: "dracula".into(),
            context_lines: 5,
            ..old
        };
        Config::save(&path, &old, &new).expect("save");
        let saved = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            saved,
            "# My settings\ntheme = \"dracula\" # cool colors\n\ntab_width = 2\nrelative_dates = false\ncontext_lines = 5\n"
        );
        assert_eq!(Config::parse(&saved), Ok(new));
    }

    #[test]
    fn save_creates_the_file_and_its_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tergi").join("config.toml");
        let new = Config {
            relative_dates: false,
            ..Config::default()
        };
        Config::save(&path, &Config::default(), &new).expect("save");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "relative_dates = false\n"
        );
        // Nothing is left behind next to it.
        assert_eq!(
            std::fs::read_dir(path.parent().expect("parent"))
                .expect("list")
                .count(),
            1
        );
    }

    #[test]
    fn save_leaves_a_broken_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \n").expect("write");
        let new = Config {
            tab_width: 8,
            ..Config::default()
        };
        assert!(Config::save(&path, &Config::default(), &new).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "theme = \n");
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("dotfiles.toml");
        std::fs::write(&real, "tab_width = 2\n").expect("write");
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let old = Config::load(&link).0;
        let new = Config {
            tab_width: 3,
            ..old.clone()
        };
        Config::save(&link, &old, &new).expect("save");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("stat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "tab_width = 3\n"
        );
    }

    #[tokio::test]
    async fn watch_reports_changes_and_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let (tx, mut rx) = unbounded_channel();
        let (_watcher, run) = watch(&path, move |r| {
            let _ = tx.send(r);
        })
        .expect("watch");
        tokio::spawn(run);
        let next = async |rx: &mut tokio::sync::mpsc::UnboundedReceiver<Result<Config, String>>| {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a report")
                .expect("open")
        };

        std::fs::write(&path, "tab_width = 7\n").expect("write");
        assert_eq!(next(&mut rx).await.map(|c| c.tab_width), Ok(7));
        std::fs::write(&path, "tab_width = [\n").expect("write");
        assert!(next(&mut rx).await.is_err());
        std::fs::write(&path, "theme = \"plaid\"\n").expect("write");
        assert!(next(&mut rx).await.is_err());
        // Other files in the folder don't count.
        std::fs::write(dir.path().join("other.toml"), "x = 1\n").expect("write");
        let plaid = Config::parse("theme = \"plaid\"\n").expect("parse");
        let fixed = Config {
            tab_width: 5,
            ..Config::default()
        };
        Config::save(&path, &plaid, &fixed).expect("save");
        assert_eq!(next(&mut rx).await, Ok(fixed));
        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv())
                .await
                .is_err()
        );
    }
}
