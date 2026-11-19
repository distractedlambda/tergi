//! Helpers for building scratch repositories in temporary directories.

#![allow(dead_code)]

pub mod settings;
pub mod ui;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use proptest::prelude::ProptestConfig;
use tergi::git::cli::Git;

use settings::Settings;

pub struct Scratch {
    pub dir: tempfile::TempDir,
    /// The working tree. This is `dir` itself, or a folder in it.
    root: PathBuf,
    pub git: Git,
}

impl Scratch {
    pub fn new() -> Scratch {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        Scratch::init(dir, root)
    }

    /// A repository with the given settings in its config.
    pub fn with(settings: &Settings) -> Scratch {
        let s = Scratch::new();
        for (key, value) in &settings.0 {
            s.sh(&["config", key, value]);
        }
        s
    }

    /// A repository in a folder of the temporary directory. The name is
    /// bytes, so it can be one that isn't UTF-8.
    pub fn in_folder(name: &[u8]) -> Scratch {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join(tergi::util::fs_path(name));
        std::fs::create_dir(&root).expect("mkdir");
        Scratch::init(dir, root)
    }

    fn init(dir: tempfile::TempDir, root: PathBuf) -> Scratch {
        let s = Scratch {
            git: Git::new(&root),
            root,
            dir,
        };
        s.sh(&["init", "-q", "-b", "main"]);
        s.sh(&["config", "user.name", "Test User"]);
        s.sh(&["config", "user.email", "test@example.com"]);
        s.sh(&["config", "commit.gpgsign", "false"]);
        s
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Make sure the repository can hold ref names that aren't UTF-8.
    /// Git keeps most refs as files named after the ref, so on a file
    /// system that rejects such file names we move the repository to the
    /// reftable format, which keeps all refs inside a few files. Returns
    /// false if this git is too old to do that. Call it before making
    /// any commits.
    pub fn allow_non_utf8_refs(&self) -> bool {
        if non_utf8_file_names() {
            return true;
        }
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(["refs", "migrate", "--ref-format=reftable"])
            .output()
            .expect("run git");
        if !out.status.success() {
            eprintln!(
                "skipped: this file system rejects names that aren't UTF-8, \
                 and this git can't move the repository to reftable: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        out.status.success()
    }

    pub fn file(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.file(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(p, content).expect("write");
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.file(rel)).expect("read")
    }

    /// Run git and return stdout, panicking on failure.
    pub fn sh(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_EDITOR", "true")
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run git with arguments that needn't be UTF-8, such as ref names, and
    /// return stdout as bytes, panicking on failure.
    pub fn sh_bytes(&self, args: &[&[u8]]) -> Vec<u8> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_EDITOR", "true")
            .args(args.iter().map(|a| tergi::util::os_str(a)))
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args.iter()
                .map(|a| a.escape_ascii().to_string())
                .collect::<Vec<_>>(),
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    pub fn commit_all(&self, msg: &str) -> String {
        self.sh(&["add", "-A"]);
        self.sh(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.sh(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// The staged content of a file.
    pub fn index(&self, rel: &str) -> String {
        self.sh(&["show", &format!(":{rel}")])
    }

    pub fn log(&self) -> Vec<String> {
        self.sh(&["log", "--format=%s"])
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Whether the file system that holds temporary directories accepts file
/// names that aren't UTF-8. APFS on macOS doesn't.
pub fn non_utf8_file_names() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(tergi::util::fs_path(b"caf\xe9")), "").is_ok()
    })
}

/// Give each of `parts` tests its share of the cases in `config`.
pub fn split(mut config: ProptestConfig, parts: u32) -> ProptestConfig {
    config.cases = config.cases.div_ceil(parts);
    config
}

/// Define a property test as several tests that share its cases.
///
/// The test harness runs separate tests at the same time, but a property
/// test runs its cases one after another. A long one would keep one core
/// busy after the rest of the suite has finished. Splitting its cases over
/// several tests lets them run at once. Each part picks its own random
/// cases, and all of them replay the file's saved failures.
///
/// `config` gives the number of cases for all the parts together, so
/// `PROPTEST_CASES` still sets the total. The parts go in a module named
/// after the test, so a filter with that name runs all of them.
macro_rules! split_proptest {
    (
        config = $config:expr;
        parts = [$($part:ident)+];
        $(#[$meta:meta])*
        fn $name:ident($case:ident in $strategy:expr) $body:block
    ) => {
        $(#[$meta])*
        mod $name {
            use super::*;

            const PARTS: u32 = [$(stringify!($part)),+].len() as u32;

            ::proptest::proptest! {
                #![proptest_config(crate::common::split($config, PARTS))]
                $(
                    #[test]
                    fn $part($case in $strategy) $body
                )+
            }
        }
    };
}

pub(crate) use split_proptest;
