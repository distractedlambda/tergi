//! Running tergi as the askpass helper, the way git and ssh run it, against
//! a socket that stands in for the app.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::process::Command;

use tergi::util::fs_path;

/// The socket path comes from an environment variable and the prompt from
/// an argument. Both can hold bytes that aren't UTF-8, for example when the
/// temporary folder or the user's home folder has such a name.
#[test]
fn forwards_prompts_through_paths_that_are_not_utf8() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join(fs_path(b"caf\xe9.sock"));
    let listener = UnixListener::bind(&sock).expect("bind");
    let app = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut prompt = String::new();
        BufReader::new(&stream)
            .read_line(&mut prompt)
            .expect("read the prompt");
        stream.write_all(b"1hunter2").expect("answer");
        prompt
    });

    let out = Command::new(env!("CARGO_BIN_EXE_tergi"))
        .arg(OsStr::from_bytes(b"Passphrase for /home/\xff/.ssh/id: "))
        .env(tergi::askpass::SOCKET_ENV, &sock)
        .output()
        .expect("run the helper");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hunter2\n");
    // The app only shows the prompt, so the odd byte becomes a
    // replacement character.
    assert_eq!(
        app.join().expect("the app side"),
        "Passphrase for /home/\u{fffd}/.ssh/id: \n"
    );
}

/// On Linux the socket has an abstract name, which any user can connect
/// to, so the server has to turn away other users. This needs root to run
/// the helper as someone else, and `setpriv` to do it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn ignores_prompts_from_other_users() {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.stdout == b"0\n");
    let setpriv = Command::new("setpriv").arg("--help").output().is_ok();
    if !root || !setpriv {
        eprintln!("skipping: needs root and setpriv");
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (address, serve) = tergi::askpass::start_server(move |prompt, reply| {
        let _ = tx.send(prompt);
        let _ = reply.send(Some("hunter2".to_string()));
    })
    .expect("start the server");
    let server = tokio::spawn(serve);
    let ask = |user: &[&str]| {
        let mut cmd = Command::new("setpriv");
        cmd.args(user)
            .arg(env!("CARGO_BIN_EXE_tergi"))
            .arg("Password:")
            .env(tergi::askpass::SOCKET_ENV, &address);
        tokio::task::spawn_blocking(move || cmd.output().expect("run the helper"))
    };

    let out = ask(&["--reuid=65534", "--regid=65534", "--clear-groups"])
        .await
        .expect("helper");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let out = ask(&[]).await.expect("helper");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hunter2\n");
    assert_eq!(rx.recv().await.as_deref(), Some("Password:"));
    assert!(rx.try_recv().is_err());
    server.abort();
}
