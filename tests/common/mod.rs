//! Helpers for building scratch repositories in temporary directories.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tergi::git::cli::Git;

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
