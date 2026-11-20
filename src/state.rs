//! What tergi remembers about each repository between runs, such as the
//! branches the user hid from the history.
//!
//! This lives in a redb database under the user's data folder, not in the
//! repository, so tergi never adds files to the user's work. Each
//! repository is known by the path of its git folder. Linked worktrees
//! share that folder, so they share the settings too.
//!
//! Each change is written as soon as it is made, and only the change is
//! written, not everything we know. Two copies of tergi open on the same
//! repository then don't undo each other's changes. They only see each
//! other's changes the next time they start, though.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gix::bstr::BString;
use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadableDatabase as _,
    ReadableMultimapTable as _,
};

/// For each repository, the full names of the refs hidden from the history,
/// such as `refs/heads/main` or `refs/remotes/origin/main`.
const HIDDEN: MultimapTableDefinition<&[u8], &[u8]> = MultimapTableDefinition::new("hidden_refs");

/// redb lets only one process open the file at a time. Each copy of tergi
/// only holds it for a moment, so one that finds it busy waits this long
/// for the other to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Hide(BString),
    Show(BString),
    ShowAll,
}

/// The settings of one repository in the database.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    repo: Vec<u8>,
}

impl Store {
    /// Where the database is: `~/.local/share/tergi/state.redb`, unless
    /// `XDG_DATA_HOME` says otherwise.
    pub fn default_path() -> Option<PathBuf> {
        use etcetera::BaseStrategy;
        etcetera::choose_base_strategy()
            .ok()
            .map(|s| s.data_dir().join("tergi").join("state.redb"))
    }

    /// The settings for the repository whose git folder is `common_dir`.
    pub fn new(path: PathBuf, common_dir: &Path) -> Store {
        let dir = common_dir
            .canonicalize()
            .unwrap_or_else(|_| common_dir.to_path_buf());
        Store {
            path,
            repo: dir.as_os_str().as_bytes().to_vec(),
        }
    }

    /// The refs hidden from the history. Without a database, nothing is.
    pub fn hidden(&self) -> Result<HashSet<BString>> {
        if !self.path.exists() {
            return Ok(HashSet::new());
        }
        let db = self.open()?;
        let tx = db.begin_read()?;
        let table = match tx.open_multimap_table(HIDDEN) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashSet::new()),
            Err(e) => return Err(e.into()),
        };
        let mut set = HashSet::new();
        for name in table.get(self.repo.as_slice())? {
            set.insert(BString::from(name?.value()));
        }
        Ok(set)
    }

    /// Write changes, all in one transaction.
    pub fn apply(&self, changes: &[Change]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let db = self.open()?;
        let tx = db.begin_write()?;
        {
            let mut table = tx.open_multimap_table(HIDDEN)?;
            let repo = self.repo.as_slice();
            for c in changes {
                match c {
                    Change::Hide(name) => {
                        table.insert(repo, name.as_slice())?;
                    }
                    Change::Show(name) => {
                        table.remove(repo, name.as_slice())?;
                    }
                    Change::ShowAll => {
                        table.remove_all(repo)?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Open the database, waiting a little if another process has it open.
    fn open(&self) -> Result<Database> {
        let start = Instant::now();
        loop {
            match Database::create(&self.path) {
                Ok(db) => return Ok(db),
                Err(DatabaseError::DatabaseAlreadyOpen) if start.elapsed() < BUSY_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("could not open {}", self.path.display()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> HashSet<BString> {
        v.iter().map(|s| BString::from(*s)).collect()
    }

    #[test]
    fn changes_are_kept_for_each_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The folder for the database doesn't exist yet.
        let path = dir.path().join("tergi").join("state.redb");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let sa = Store::new(path.clone(), &a);
        let sb = Store::new(path.clone(), &b);
        assert_eq!(sa.hidden().expect("read"), names(&[]));
        assert!(!path.exists(), "reading doesn't create the database");

        sa.apply(&[
            Change::Hide("refs/heads/x".into()),
            Change::Hide("refs/remotes/origin/y".into()),
            Change::Hide(BString::from(&b"refs/heads/\xff"[..])),
        ])
        .expect("write");
        sb.apply(&[Change::Hide("refs/heads/z".into())])
            .expect("write");
        sa.apply(&[Change::Show("refs/heads/x".into())])
            .expect("write");

        let mut want = names(&["refs/remotes/origin/y"]);
        want.insert(BString::from(&b"refs/heads/\xff"[..]));
        // A new store, as on the next run, with a path that leads to the
        // same folder.
        let again = Store::new(path, &dir.path().join("b/../a"));
        assert_eq!(again.hidden().expect("read"), want);
        assert_eq!(sb.hidden().expect("read"), names(&["refs/heads/z"]));

        sa.apply(&[Change::ShowAll]).expect("write");
        assert_eq!(sa.hidden().expect("read"), names(&[]));
        assert_eq!(sb.hidden().expect("read"), names(&["refs/heads/z"]));
    }

    #[test]
    fn waits_while_another_handle_has_the_database_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let store = Store::new(path.clone(), dir.path());
        let held = Database::create(&path).expect("open");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        store
            .apply(&[Change::Hide("refs/heads/x".into())])
            .expect("write after the other handle closes");
        release.join().expect("join");
        assert_eq!(store.hidden().expect("read"), names(&["refs/heads/x"]));
    }
}
