//! Random git settings for the property tests.
//!
//! tergi runs the user's git, which reads the user's config, and gix reads
//! it too. The tests otherwise only see the dev image's config. Each case
//! of a test that uses the real git can pick a random set of settings from
//! here, and `Scratch::with` writes them to the scratch repository. tergi
//! should behave the same under all of them, so the checks don't change.
//!
//! The test's own git commands read these settings as well. Most of them
//! are plumbing, which ignores settings like these, and the others name
//! what they need on the command line.

use proptest::prelude::*;

/// Settings that change what git prints or does, with the values to pick
/// from.
///
/// `core.autocrlf` is only ever `input`. With `true`, git writes files
/// with CRLF line ends when it checks them out, so discarding a change
/// rightly gives a different file than the models in the tests expect.
const SETTINGS: &[(&str, &[&str])] = &[
    ("diff.mnemonicPrefix", &["true"]),
    ("diff.noprefix", &["true"]),
    ("diff.renames", &["false", "true", "copies"]),
    ("diff.algorithm", &["minimal", "patience", "histogram"]),
    ("diff.context", &["0", "1", "7"]),
    ("diff.interHunkContext", &["2", "10"]),
    ("diff.suppressBlankEmpty", &["true"]),
    ("color.diff", &["always"]),
    ("color.status", &["always"]),
    ("core.autocrlf", &["input"]),
    ("core.commentChar", &[";", "auto"]),
    ("commit.cleanup", &["verbatim", "strip", "scissors"]),
    ("commit.verbose", &["true"]),
    ("status.showUntrackedFiles", &["no", "normal"]),
    ("status.relativePaths", &["false"]),
    ("status.renames", &["false", "copies"]),
    ("apply.whitespace", &["error", "fix"]),
    ("rebase.abbreviateCommands", &["true"]),
    ("rebase.instructionFormat", &["%s (%an)", "%H"]),
    ("rebase.updateRefs", &["true"]),
    ("log.showSignature", &["true"]),
    ("i18n.logOutputEncoding", &["ISO-8859-1"]),
];

/// The settings for one case, as keys and values for `git config`.
#[derive(Debug, Clone, Default)]
pub struct Settings(pub Vec<(&'static str, &'static str)>);

impl Settings {
    pub fn get(&self, key: &str) -> Option<&'static str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

/// A random set of settings. Each one is left out more often than not, so
/// a case has a few at a time, and a failing case shrinks to the ones that
/// matter.
pub fn settings() -> impl Strategy<Value = Settings> {
    SETTINGS
        .iter()
        .map(|&(key, values)| {
            prop::option::weighted(0.25, prop::sample::select(values))
                .prop_map(move |v| v.map(|v| (key, v)))
        })
        .collect::<Vec<_>>()
        .prop_map(|picked| Settings(picked.into_iter().flatten().collect()))
}
