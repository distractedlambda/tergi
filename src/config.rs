//! User configuration, read from `~/.config/tergi/config.toml`.
//!
//! ```toml
//! theme = "dark"          # or "light"
//! context_lines = 3       # lines of context around changes
//! tab_width = 4
//! relative_dates = true   # "3 days ago" instead of "2026-11-08"
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use crate::theme::ThemeName;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: ThemeName,
    pub context_lines: u32,
    pub tab_width: usize,
    pub relative_dates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: ThemeName::Dark,
            context_lines: 3,
            tab_width: 4,
            relative_dates: true,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        use etcetera::BaseStrategy;
        etcetera::choose_base_strategy()
            .ok()
            .map(|s| s.config_dir().join("tergi").join("config.toml"))
    }

    /// Load the config file. A missing file gives the defaults. A broken one
    /// gives the defaults plus an error message to show.
    pub fn load() -> (Config, Option<String>) {
        let Some(path) = Self::path() else {
            return (Config::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut c) => {
                    c.tab_width = c.tab_width.clamp(1, 16);
                    c.context_lines = c.context_lines.min(50);
                    (c, None)
                }
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => (Config::default(), None),
        }
    }
}
