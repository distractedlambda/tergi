//! Operations on scratch repositories that have gone wrong before.

mod common;

use common::Scratch;
use tergi::git::ops;

#[test]
fn message_lines_starting_with_hash() {
    let s = Scratch::new();
    s.write("f.txt", "a\n");
    s.sh(&["add", "-A"]);
    ops::commit(&s.git, "#42 fix it\n\n# Heading\ntext\n", false).expect("commit");
    assert_eq!(s.sh(&["log", "-1", "--format=%B"]).trim(), "#42 fix it\n\n# Heading\ntext");
}
