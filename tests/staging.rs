mod common;

use common::Scratch;
use tergi::git::diff::{FileDiff, LineKind};
use tergi::git::ops::{ApplyMode, apply_lines};
use tergi::git::workdir::Workdir;

async fn unstaged(s: &Scratch, path: &str) -> FileDiff {
    let wd = Workdir::load(&s.git, 3, 100_000).await.expect("load");
    wd.unstaged
        .into_iter()
        .chain(wd.untracked)
        .find(|f| f.path() == path)
        .expect("file in diff")
}

async fn staged(s: &Scratch, path: &str) -> FileDiff {
    let wd = Workdir::load(&s.git, 3, 100_000).await.expect("load");
    wd.staged.into_iter().find(|f| f.path() == path).expect("file in diff")
}

/// Select changed lines whose text is in `texts`.
fn select<'a>(f: &'a FileDiff, texts: &'a [&str]) -> impl Fn(usize, usize) -> bool + 'a {
    move |h, l| {
        let line = &f.hunks[h].lines[l];
        line.kind != LineKind::Context && texts.contains(&line.text.as_str())
    }
}

#[tokio::test]
async fn stage_single_added_line() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nx\nb\ny\nc\n");
    let f = unstaged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["y"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), "a\nb\ny\nc\n");
    assert_eq!(s.read("f.txt"), "a\nx\nb\ny\nc\n");
}

#[tokio::test]
async fn stage_part_of_a_replacement() {
    let s = Scratch::new();
    s.write("f.txt", "1\n2\n3\n4\n5\n");
    s.commit_all("init");
    s.write("f.txt", "1\nTWO\nTHREE\n4\n5\n");
    let f = unstaged(&s, "f.txt").await;
    // Replace only "3" with "THREE".
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["3", "THREE"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), "1\n2\nTHREE\n4\n5\n");
}

#[tokio::test]
async fn stage_lines_across_hunks() {
    let s = Scratch::new();
    let base: String = (1..=30).map(|i| format!("{i}\n")).collect();
    s.write("f.txt", &base);
    s.commit_all("init");
    let changed = base.replacen("2\n", "two\n", 1).replace("\n25\n", "\n25\nnew\n");
    s.write("f.txt", &changed);
    let f = unstaged(&s, "f.txt").await;
    assert_eq!(f.hunks.len(), 2);
    // Only the second hunk's addition. The first hunk's offset must not matter.
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["new"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), base.replace("\n25\n", "\n25\nnew\n"));
}

#[tokio::test]
async fn unstage_some_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nB\nc\nd\n");
    s.sh(&["add", "f.txt"]);
    let f = staged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Unstage, &select(&f, &["d"])).await.expect("unstage");
    assert_eq!(s.index("f.txt"), "a\nB\nc\n");
    // The working tree is untouched.
    assert_eq!(s.read("f.txt"), "a\nB\nc\nd\n");
}

#[tokio::test]
async fn discard_some_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    s.write("f.txt", "a\nB\nc\nd\n");
    let f = unstaged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Discard, &select(&f, &["b", "B"])).await.expect("discard");
    assert_eq!(s.read("f.txt"), "a\nb\nc\nd\n");
}

#[tokio::test]
async fn stage_part_of_untracked_file() {
    let s = Scratch::new();
    s.write("keep.txt", "x\n");
    s.commit_all("init");
    s.write("new.txt", "one\ntwo\nthree\n");
    let f = unstaged(&s, "new.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["one", "three"])).await.expect("stage");
    assert_eq!(s.index("new.txt"), "one\nthree\n");
}

#[tokio::test]
async fn unstage_part_of_new_file() {
    let s = Scratch::new();
    s.write("keep.txt", "x\n");
    s.commit_all("init");
    s.write("new.txt", "one\ntwo\nthree\n");
    s.sh(&["add", "new.txt"]);
    let f = staged(&s, "new.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Unstage, &select(&f, &["two"])).await.expect("unstage");
    assert_eq!(s.index("new.txt"), "one\nthree\n");
}

#[tokio::test]
async fn stage_part_of_deletion() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\nc\n");
    s.commit_all("init");
    std::fs::remove_file(s.file("f.txt")).expect("rm");
    let f = unstaged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["b"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), "a\nc\n");
}

#[tokio::test]
async fn stage_whole_deletion_through_patch() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb\n");
    s.write("g.txt", "keep\n");
    s.commit_all("init");
    std::fs::remove_file(s.file("f.txt")).expect("rm");
    let f = unstaged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &|_, _| true).await.expect("stage");
    let out = s.sh(&["diff", "--cached", "--name-status"]);
    assert_eq!(out.trim(), "D\tf.txt");
}

#[tokio::test]
async fn no_newline_at_end() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb");
    s.commit_all("init");
    s.write("f.txt", "a\nb\nc");
    let f = unstaged(&s, "f.txt").await;
    // Taking the whole change at the end of the file works.
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["b", "c"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), "a\nb\nc");
}

#[tokio::test]
async fn splitting_last_line_change_is_refused() {
    let s = Scratch::new();
    s.write("f.txt", "a\nb");
    s.commit_all("init");
    s.write("f.txt", "a\nb\nc");
    let f = unstaged(&s, "f.txt").await;
    // Selecting only "+b" (with newline) but not "-b" (without) can't work.
    let only_add_b = |h: usize, l: usize| {
        let line = &f.hunks[h].lines[l];
        line.kind == LineKind::Add && line.text == "b"
    };
    assert!(apply_lines(&s.git, &f, ApplyMode::Stage, &only_add_b).await.is_err());
}

#[tokio::test]
async fn crlf_lines() {
    let s = Scratch::new();
    s.write("f.txt", "a\r\nb\r\nc\r\n");
    s.commit_all("init");
    s.write("f.txt", "a\r\nB\r\nc\r\nd\r\n");
    let f = unstaged(&s, "f.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["d\r"])).await.expect("stage");
    assert_eq!(s.index("f.txt"), "a\r\nb\r\nc\r\nd\r\n");
}

#[tokio::test]
async fn paths_with_spaces_and_unicode() {
    let s = Scratch::new();
    s.write("dir with space/fïle.txt", "a\n");
    s.commit_all("init");
    s.write("dir with space/fïle.txt", "a\nb\nc\n");
    let f = unstaged(&s, "dir with space/fïle.txt").await;
    apply_lines(&s.git, &f, ApplyMode::Stage, &select(&f, &["c"])).await.expect("stage");
    assert_eq!(s.index("dir with space/fïle.txt"), "a\nc\n");
}

#[tokio::test]
async fn every_single_line_of_a_mixed_hunk() {
    // Stage each changed line on its own, from a fresh state, and check the
    // index matches what we expect from applying just that line.
    let s = Scratch::new();
    let old = "1\n2\n3\n4\n5\n6\n";
    let new = "1\nX\n3\nY\nZ\n6\n7\n";
    s.write("f.txt", old);
    s.commit_all("init");
    s.write("f.txt", new);
    let f = unstaged(&s, "f.txt").await;
    let changed: Vec<(usize, usize)> = f
        .hunks
        .iter()
        .enumerate()
        .flat_map(|(h, hunk)| hunk.changed_lines().map(move |l| (h, l)))
        .collect();
    for &(h, l) in &changed {
        s.sh(&["reset", "-q"]);
        let line = f.hunks[h].lines[l].clone();
        apply_lines(&s.git, &f, ApplyMode::Stage, &|a, b| (a, b) == (h, l)).await.expect("stage");
        let idx = s.index("f.txt");
        match line.kind {
            LineKind::Del => assert!(!idx.lines().any(|x| x == line.text), "{idx:?}"),
            LineKind::Add => assert!(idx.lines().any(|x| x == line.text), "{idx:?}"),
            LineKind::Context => unreachable!(),
        }
        // Everything not touched stays as it was.
        assert_eq!(idx.lines().count(), old.lines().count() + usize::from(line.kind == LineKind::Add) - usize::from(line.kind == LineKind::Del));
    }
}

/// What a file should contain after applying only the selected changes.
/// Going forward, the result is the old file plus the selected changes. In
/// reverse, it's the new file minus them.
fn expected(f: &FileDiff, base: &str, selected: &dyn Fn(usize, usize) -> bool, reverse: bool) -> String {
    let base: Vec<&str> = base.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut pos = 0;
    for (hi, h) in f.hunks.iter().enumerate() {
        let (start, len) = if reverse { (h.new_start, h.new_len) } else { (h.old_start, h.old_len) };
        // For an empty range the header gives the line before it.
        let first = if len == 0 { start as usize } else { start as usize - 1 };
        out.extend(&base[pos..first]);
        pos = first + len as usize;
        for (li, line) in h.lines.iter().enumerate() {
            let keep = match (line.kind, reverse) {
                (LineKind::Context, _) => true,
                (LineKind::Add, false) | (LineKind::Del, true) => selected(hi, li),
                (LineKind::Del, false) | (LineKind::Add, true) => !selected(hi, li),
            };
            if keep {
                out.push(&line.text);
            }
        }
    }
    out.extend(&base[pos..]);
    out.iter().map(|l| format!("{l}\n")).collect()
}

#[tokio::test]
async fn every_selection_with_every_context_size() {
    // Repeated lines make it matter exactly where a hunk is applied, most of
    // all without context lines.
    let cases = [
        ("1\n2\n3\n4\n5\n6\n7\n8\n9\n", "1\nA\n3\n4\n6\n7\nB\nC\n8\n9\n"),
        ("x\nx\nx\nx\nx\nx\n", "x\nx\ny\nx\nx\nx\nx\nz\n"),
        ("a\nb\na\nb\na\nb\n", "b\na\nb\nc\na\n"),
        ("a\nb\nc\n", "new\na\nb\n"),
    ];
    for (old, new) in cases {
        let s = Scratch::new();
        s.write("f.txt", old);
        s.commit_all("init");
        for context in [0, 1, 3] {
            s.write("f.txt", new);
            s.sh(&["reset", "-q"]);
            let load = async || Workdir::load(&s.git, context, 100_000).await.expect("load");
            let f = load().await.unstaged.remove(0);
            let changed: Vec<(usize, usize)> = f
                .hunks
                .iter()
                .enumerate()
                .flat_map(|(h, hunk)| hunk.changed_lines().map(move |l| (h, l)))
                .collect();
            assert!(changed.len() <= 7, "too many subsets to try");
            s.sh(&["add", "f.txt"]);
            let staged = load().await.staged.remove(0);
            assert_eq!(staged, f);
            for mask in 1u32..(1 << changed.len()) {
                let sel = |h: usize, l: usize| {
                    changed.iter().position(|&c| c == (h, l)).is_some_and(|i| mask & (1 << i) != 0)
                };
                let what = format!("{old:?} to {new:?}, context {context}, selection {mask:b}");

                s.sh(&["reset", "-q"]);
                apply_lines(&s.git, &f, ApplyMode::Stage, &sel).await.unwrap_or_else(|e| panic!("stage {what}: {e:#}"));
                assert_eq!(s.index("f.txt"), expected(&f, old, &sel, false), "stage {what}");

                s.sh(&["add", "f.txt"]);
                apply_lines(&s.git, &f, ApplyMode::Unstage, &sel).await.unwrap_or_else(|e| panic!("unstage {what}: {e:#}"));
                assert_eq!(s.index("f.txt"), expected(&f, new, &sel, true), "unstage {what}");

                s.sh(&["reset", "-q"]);
                apply_lines(&s.git, &f, ApplyMode::Discard, &sel).await.unwrap_or_else(|e| panic!("discard {what}: {e:#}"));
                assert_eq!(s.read("f.txt"), expected(&f, new, &sel, true), "discard {what}");
                s.write("f.txt", new);
            }
        }
    }
}
