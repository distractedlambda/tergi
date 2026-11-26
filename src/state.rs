//! What tergi remembers about each repository between runs: the branches
//! and stashes the user hid from the history, the files they folded in
//! the detail pane, and what they chose in the fetch dialog.
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

use std::collections::{BTreeSet, HashSet};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bstr::BString;
use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadTransaction, ReadableDatabase as _,
    ReadableMultimapTable as _, ReadableTable as _, TableDefinition, TableError,
};

use crate::git::ObjectId;

/// For each repository, the full names of the refs hidden from the history,
/// such as `refs/heads/main` or `refs/remotes/origin/main`.
const HIDDEN: MultimapTableDefinition<&[u8], &[u8]> = MultimapTableDefinition::new("hidden_refs");

/// For each repository, the commit ids of the stashes hidden from the
/// history. A name like `stash@{0}` moves to another stash whenever one is
/// pushed or dropped, but the commit id stays with its stash.
const HIDDEN_STASHES: MultimapTableDefinition<&[u8], &[u8]> =
    MultimapTableDefinition::new("hidden_stashes");

/// For each repository and section of the working directory, the paths of
/// the folded files. The section is a name such as `staged`.
const WORKDIR_FOLDS: MultimapTableDefinition<(&[u8], &str), &[u8]> =
    MultimapTableDefinition::new("workdir_folds");

/// For each repository and commit, the paths of the folded files. The
/// commit is its id as bytes, or both ids one after the other for a
/// comparison of two commits. A commit is only here while its folds
/// differ from the ones it starts with, so this is the whole set rather
/// than one row per file. An empty set means that nothing is folded.
const COMMIT_FOLDS: TableDefinition<RepoAndCommit, Vec<&[u8]>> =
    TableDefinition::new("commit_folds");

type RepoAndCommit = (&'static [u8], &'static [u8]);

/// For each repository, what the fetch dialog last fetched with: whether
/// it pruned, and the remotes it left out. A repository that isn't here
/// fetches every remote and prunes.
const FETCH: TableDefinition<&[u8], (bool, Vec<&[u8]>)> = TableDefinition::new("fetch");

/// What the fetch dialog starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchChoices {
    /// Whether to pass `--prune`.
    pub prune: bool,
    /// The remotes not to fetch. We keep the ones left out rather than the
    /// ones chosen, so that a remote added later starts out chosen.
    pub skip: BTreeSet<BString>,
}

impl Default for FetchChoices {
    fn default() -> Self {
        FetchChoices {
            prune: true,
            skip: BTreeSet::new(),
        }
    }
}

/// redb lets only one process open the file at a time. Each copy of tergi
/// only holds it for a moment, so one that finds it busy waits this long
/// for the other to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Hide(BString),
    Show(BString),
    HideStash(ObjectId),
    ShowStash(ObjectId),
    /// Show all hidden branches and stashes.
    ShowAll,
    /// Fold a file in the section of the working directory with this name.
    Fold(&'static str, BString),
    Unfold(&'static str, BString),
    /// Replace the folded files of a commit, or forget them with `None`.
    CommitFolds(Vec<u8>, Option<Vec<BString>>),
    /// Replace what the fetch dialog starts with.
    Fetch(FetchChoices),
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
        self.read(|tx| {
            let mut set = HashSet::new();
            if let Some(table) = exists(tx.open_multimap_table(HIDDEN))? {
                for name in table.get(self.repo.as_slice())? {
                    set.insert(BString::from(name?.value()));
                }
            }
            Ok(set)
        })
    }

    /// The stashes hidden from the history, by commit id.
    pub fn hidden_stashes(&self) -> Result<HashSet<ObjectId>> {
        self.read(|tx| {
            let mut set = HashSet::new();
            if let Some(table) = exists(tx.open_multimap_table(HIDDEN_STASHES))? {
                for id in table.get(self.repo.as_slice())? {
                    set.extend(ObjectId::from_bytes(id?.value()));
                }
            }
            Ok(set)
        })
    }

    /// The folded files in the sections of the working directory with
    /// these names.
    pub fn workdir_folds(&self, sections: &[&'static str]) -> Result<Vec<(&'static str, BString)>> {
        self.read(|tx| {
            let mut folds = Vec::new();
            if let Some(table) = exists(tx.open_multimap_table(WORKDIR_FOLDS))? {
                for &sec in sections {
                    for path in table.get((self.repo.as_slice(), sec))? {
                        folds.push((sec, BString::from(path?.value())));
                    }
                }
            }
            Ok(folds)
        })
    }

    /// The folded files of a commit, if they differ from the ones it
    /// starts with.
    pub fn commit_folds(&self, commit: &[u8]) -> Result<Option<Vec<BString>>> {
        self.read(|tx| {
            let Some(table) = exists(tx.open_table(COMMIT_FOLDS))? else {
                return Ok(None);
            };
            let Some(paths) = table.get((self.repo.as_slice(), commit))? else {
                return Ok(None);
            };
            Ok(Some(paths.value().into_iter().map(BString::from).collect()))
        })
    }

    /// What the fetch dialog last fetched with.
    pub fn fetch_choices(&self) -> Result<FetchChoices> {
        self.read(|tx| {
            let Some(table) = exists(tx.open_table(FETCH))? else {
                return Ok(FetchChoices::default());
            };
            let Some(v) = table.get(self.repo.as_slice())? else {
                return Ok(FetchChoices::default());
            };
            let (prune, skip) = v.value();
            Ok(FetchChoices {
                prune,
                skip: skip.into_iter().map(BString::from).collect(),
            })
        })
    }

    /// Read from the database. Without one, `f` reads from an empty one.
    fn read<T: Default>(&self, f: impl FnOnce(&ReadTransaction) -> Result<T>) -> Result<T> {
        if !self.path.exists() {
            return Ok(T::default());
        }
        let db = self.open()?;
        let tx = db.begin_read()?;
        f(&tx)
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
            let mut hidden = tx.open_multimap_table(HIDDEN)?;
            let mut stashes = tx.open_multimap_table(HIDDEN_STASHES)?;
            let mut workdir = tx.open_multimap_table(WORKDIR_FOLDS)?;
            let mut commits = tx.open_table(COMMIT_FOLDS)?;
            let mut fetch = tx.open_table(FETCH)?;
            let repo = self.repo.as_slice();
            for c in changes {
                match c {
                    Change::Hide(name) => {
                        hidden.insert(repo, name.as_slice())?;
                    }
                    Change::Show(name) => {
                        hidden.remove(repo, name.as_slice())?;
                    }
                    Change::HideStash(id) => {
                        stashes.insert(repo, id.as_bytes())?;
                    }
                    Change::ShowStash(id) => {
                        stashes.remove(repo, id.as_bytes())?;
                    }
                    Change::ShowAll => {
                        hidden.remove_all(repo)?;
                        stashes.remove_all(repo)?;
                    }
                    Change::Fold(sec, path) => {
                        workdir.insert((repo, *sec), path.as_slice())?;
                    }
                    Change::Unfold(sec, path) => {
                        workdir.remove((repo, *sec), path.as_slice())?;
                    }
                    Change::CommitFolds(commit, Some(paths)) => {
                        let paths: Vec<&[u8]> = paths.iter().map(|p| p.as_slice()).collect();
                        commits.insert((repo, commit.as_slice()), paths)?;
                    }
                    Change::CommitFolds(commit, None) => {
                        commits.remove((repo, commit.as_slice()))?;
                    }
                    Change::Fetch(c) => {
                        let skip: Vec<&[u8]> = c.skip.iter().map(|r| r.as_slice()).collect();
                        fetch.insert(repo, (c.prune, skip))?;
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

/// A table that hasn't been written to yet doesn't exist, which is the
/// same as an empty one.
fn exists<T>(r: std::result::Result<T, TableError>) -> Result<Option<T>> {
    match r {
        Ok(t) => Ok(Some(t)),
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
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
    fn hidden_stashes_are_kept_for_each_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let sa = Store::new(path.clone(), &a);
        let sb = Store::new(path, &b);
        let id = |n: u8, len: usize| ObjectId::from_bytes_or_panic(&vec![n; len]);
        let ids = |v: &[ObjectId]| v.iter().copied().collect::<HashSet<_>>();
        assert_eq!(sa.hidden_stashes().expect("read"), ids(&[]));

        sa.apply(&[
            Change::HideStash(id(1, 20)),
            Change::HideStash(id(2, 32)),
            Change::HideStash(id(3, 20)),
            Change::ShowStash(id(3, 20)),
            Change::Hide("refs/heads/x".into()),
        ])
        .expect("write");
        sb.apply(&[Change::HideStash(id(4, 20))]).expect("write");
        assert_eq!(
            sa.hidden_stashes().expect("read"),
            ids(&[id(1, 20), id(2, 32)])
        );
        assert_eq!(sb.hidden_stashes().expect("read"), ids(&[id(4, 20)]));

        // Showing everything shows the stashes too.
        sa.apply(&[Change::ShowAll]).expect("write");
        assert_eq!(sa.hidden_stashes().expect("read"), ids(&[]));
        assert_eq!(sa.hidden().expect("read"), names(&[]));
        assert_eq!(sb.hidden_stashes().expect("read"), ids(&[id(4, 20)]));
    }

    #[test]
    fn folds_are_kept_for_each_section_and_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let sa = Store::new(path.clone(), &a);
        let sb = Store::new(path, &b);
        let sections = ["staged", "unstaged"];
        assert_eq!(sa.workdir_folds(&sections).expect("read"), []);
        assert_eq!(sa.commit_folds(b"c1").expect("read"), None);

        sa.apply(&[
            Change::Fold("staged", "x".into()),
            Change::Fold("unstaged", "x".into()),
            Change::Fold("unstaged", "y".into()),
            Change::Unfold("unstaged", "x".into()),
            Change::CommitFolds(b"c1".to_vec(), Some(vec!["p".into(), "q".into()])),
            Change::CommitFolds(b"c2".to_vec(), Some(vec![])),
        ])
        .expect("write");
        sb.apply(&[
            Change::Fold("staged", "z".into()),
            Change::CommitFolds(b"c1".to_vec(), Some(vec!["r".into()])),
        ])
        .expect("write");

        let mut folds = sa.workdir_folds(&sections).expect("read");
        folds.sort();
        assert_eq!(
            folds,
            [("staged", "x".into()), ("unstaged", BString::from("y"))]
        );
        assert_eq!(
            sa.workdir_folds(&["unstaged"]).expect("read"),
            [("unstaged", BString::from("y"))]
        );
        assert_eq!(
            sa.commit_folds(b"c1").expect("read"),
            Some(vec!["p".into(), "q".into()])
        );
        // An empty set is not the same as no set.
        assert_eq!(sa.commit_folds(b"c2").expect("read"), Some(vec![]));
        assert_eq!(
            sb.commit_folds(b"c1").expect("read"),
            Some(vec!["r".into()])
        );

        sa.apply(&[Change::CommitFolds(b"c1".to_vec(), None)])
            .expect("write");
        assert_eq!(sa.commit_folds(b"c1").expect("read"), None);
        assert_eq!(
            sb.commit_folds(b"c1").expect("read"),
            Some(vec!["r".into()])
        );
    }

    #[test]
    fn fetch_choices_are_kept_for_each_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&a).expect("mkdir");
        std::fs::create_dir(&b).expect("mkdir");
        let sa = Store::new(path.clone(), &a);
        let sb = Store::new(path.clone(), &b);
        assert_eq!(sa.fetch_choices().expect("read"), FetchChoices::default());

        let choices = FetchChoices {
            prune: false,
            skip: [BString::from("upstream"), BString::from(&b"\xff"[..])].into(),
        };
        sa.apply(&[Change::Fetch(choices.clone())]).expect("write");
        let again = Store::new(path, &a);
        assert_eq!(again.fetch_choices().expect("read"), choices);
        assert_eq!(sb.fetch_choices().expect("read"), FetchChoices::default());

        // Each change replaces the whole set.
        let fewer = FetchChoices {
            prune: true,
            skip: [BString::from("upstream")].into(),
        };
        sa.apply(&[Change::Fetch(fewer.clone())]).expect("write");
        assert_eq!(sa.fetch_choices().expect("read"), fewer);
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
