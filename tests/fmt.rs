//! Fails when the code isn't formatted with rustfmt, so that `cargo test`
//! catches it. Run `cargo fmt` to fix it.

use std::process::Command;

#[test]
fn code_is_formatted() {
    // Cargo sets CARGO when it runs tests. Using it picks the same toolchain
    // that built the tests, and so the same rustfmt.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["fmt", "--check"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("running cargo fmt");
    assert!(
        out.status.success(),
        "The code isn't formatted. Run `cargo fmt` to fix it.\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
