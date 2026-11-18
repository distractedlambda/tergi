//! Letting git ask for passwords and passphrases through the UI.
//!
//! For remote operations we set `GIT_ASKPASS` and `SSH_ASKPASS` to our own
//! executable. When git or ssh runs it, it connects to a Unix socket owned
//! by the running tergi, which shows a prompt and sends back the answer.
//!
//! On Linux the socket has an abstract name, which is not a file. The
//! kernel forgets the name as soon as the listener closes, even when tergi
//! is killed, so there is nothing to clean up. Anyone on the machine can
//! connect to an abstract socket, so the server only answers connections
//! from our own user. Other systems don't have abstract sockets, so the
//! socket is a file in the temporary directory. Each server removes the
//! files that dead servers left behind when it starts.

use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::oneshot;

pub const SOCKET_ENV: &str = "TERGI_ASKPASS_SOCK";

/// Socket addresses that start with this byte are abstract names, as in
/// `ss` and systemd. Socket paths are absolute, so they never start with
/// it.
const ABSTRACT: u8 = b'@';

const PREFIX: &str = "tergi-askpass-";
const SUFFIX: &str = ".sock";

/// Listen for prompts. `on_prompt` gets the prompt text and a channel for
/// the answer, where `None` means the user cancelled.
///
/// Returns the socket address for `SOCKET_ENV` and a future that answers
/// prompts, one at a time. The caller should spawn the future. The socket
/// closes when the future is dropped.
pub fn start_server(
    on_prompt: impl Fn(String, oneshot::Sender<Option<String>>) + Send + 'static,
) -> Option<(OsString, impl Future<Output = ()> + Send + 'static)> {
    // Bind here rather than in the future, so the socket exists as soon as
    // we return.
    let (address, listener) = bind()?;
    listener.set_nonblocking(true).ok()?;
    let serve = async move {
        let Ok(listener) = UnixListener::from_std(listener) else {
            return;
        };
        let Some(check) = PeerCheck::new() else {
            return;
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            if !check.allows(&stream) {
                continue;
            }
            let (read, mut write) = stream.into_split();
            let mut prompt = String::new();
            // Servers that look for stale sockets connect and hang up
            // without sending anything. That isn't a prompt.
            match BufReader::new(read).read_line(&mut prompt).await {
                Ok(0) | Err(_) => continue,
                Ok(_) => {}
            }
            let (tx, rx) = oneshot::channel();
            on_prompt(prompt.trim_end().to_string(), tx);
            let reply = match rx.await {
                Ok(Some(answer)) => format!("1{answer}"),
                _ => "0".to_string(),
            };
            let _ = write.write_all(reply.as_bytes()).await;
        }
    };
    Some((address, serve))
}

fn socket_name() -> String {
    // One process can start more than one server, as the tests do, so the
    // process id alone doesn't make the name unique. The random part stops
    // other users from guessing the name and taking it first.
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().build_hasher().finish();
    format!("{PREFIX}{}-{n}-{random:016x}{SUFFIX}", std::process::id())
}

#[cfg(target_os = "linux")]
fn bind() -> Option<(OsString, std::os::unix::net::UnixListener)> {
    use std::os::linux::net::SocketAddrExt;
    let name = socket_name();
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).ok()?;
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr).ok()?;
    Some((format!("@{name}").into(), listener))
}

#[cfg(not(target_os = "linux"))]
fn bind() -> Option<(OsString, std::os::unix::net::UnixListener)> {
    let dir = std::env::temp_dir();
    let path = dir.join(socket_name());
    let listener = std::os::unix::net::UnixListener::bind(&path).ok()?;
    // Connecting to sockets can block, so look in the background. This
    // server's own socket is already listening, so it stays.
    std::thread::spawn(move || remove_stale_sockets(&dir));
    Some((path.into_os_string(), listener))
}

/// Remove the socket files in `dir` that no server listens on anymore.
/// Servers that exit normally remove their own file, but killed ones
/// can't.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn remove_stale_sockets(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.as_bytes();
        if !(name.starts_with(PREFIX.as_bytes()) && name.ends_with(SUFFIX.as_bytes())) {
            continue;
        }
        let path = entry.path();
        // A live server accepts the connection and ignores it, because we
        // hang up without sending a prompt.
        if UnixStream::connect(&path).is_err_and(|e| e.kind() == ErrorKind::ConnectionRefused) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Decides which connections the server answers.
struct PeerCheck {
    #[cfg(target_os = "linux")]
    uid: u32,
}

impl PeerCheck {
    #[cfg(target_os = "linux")]
    fn new() -> Option<PeerCheck> {
        // The peer of a socket pair is ourselves, so this is our own user
        // id, the same kind of id that `peer_cred` reports for clients.
        let (ours, _theirs) = tokio::net::UnixStream::pair().ok()?;
        let uid = ours.peer_cred().ok()?.uid();
        Some(PeerCheck { uid })
    }

    #[cfg(target_os = "linux")]
    fn allows(&self, stream: &tokio::net::UnixStream) -> bool {
        stream.peer_cred().is_ok_and(|c| c.uid() == self.uid)
    }

    // A socket file gets its permissions from the umask, and the temporary
    // directory usually belongs to the user, so other users can't connect.
    #[cfg(not(target_os = "linux"))]
    fn new() -> Option<PeerCheck> {
        Some(PeerCheck {})
    }

    #[cfg(not(target_os = "linux"))]
    fn allows(&self, _stream: &tokio::net::UnixStream) -> bool {
        true
    }
}

/// Remove the server's socket file, if it has one. `main` calls this when
/// tergi exits normally.
pub fn cleanup(address: &OsStr) {
    if address.as_bytes().first() != Some(&ABSTRACT) {
        let _ = std::fs::remove_file(address);
    }
}

fn connect(address: &OsStr) -> std::io::Result<UnixStream> {
    #[cfg(target_os = "linux")]
    if let Some(name) = address.as_bytes().strip_prefix(&[ABSTRACT]) {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
        return UnixStream::connect_addr(&addr);
    }
    UnixStream::connect(address)
}

/// Run as git's askpass helper: forward the prompt and print the answer.
pub fn client_main(socket: &OsStr, prompt: &str) -> ExitCode {
    let Ok(mut stream) = connect(socket) else {
        return ExitCode::FAILURE;
    };
    let line = prompt.replace('\n', " ");
    if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
        return ExitCode::FAILURE;
    }
    let mut reply = String::new();
    if stream.read_to_string(&mut reply).is_err() {
        return ExitCode::FAILURE;
    }
    match reply.strip_prefix('1') {
        Some(answer) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        None => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn removes_only_sockets_that_nobody_listens_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join(socket_name());
        let _listener = UnixListener::bind(&live).expect("bind");
        let stale = dir.path().join(socket_name());
        drop(UnixListener::bind(&stale).expect("bind"));
        let other = dir.path().join("other.sock");
        drop(UnixListener::bind(&other).expect("bind"));

        remove_stale_sockets(dir.path());
        assert!(live.exists());
        assert!(!stale.exists());
        assert!(other.exists());
    }

    /// A server that checks for stale sockets connects and hangs up. That
    /// must not show an empty prompt.
    #[tokio::test]
    async fn ignores_connections_without_a_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (address, serve) = start_server(move |prompt, reply| {
            let _ = tx.send(prompt);
            let _ = reply.send(None);
        })
        .expect("start the server");
        let server = tokio::spawn(serve);
        let quiet = connect(&address).expect("connect");
        drop(quiet);
        let addr = address.clone();
        let code = tokio::task::spawn_blocking(move || client_main(&addr, "Password:"))
            .await
            .expect("client");
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(rx.recv().await.as_deref(), Some("Password:"));
        assert!(rx.try_recv().is_err());
        server.abort();
        let _ = server.await;
        cleanup(&address);
    }

    /// Nothing is left to clean up when the server goes away, however it
    /// goes away.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_socket_vanishes_with_the_server() {
        let (address, serve) = start_server(|_, _| {}).expect("start the server");
        assert_eq!(address.as_bytes().first(), Some(&ABSTRACT));
        let server = tokio::spawn(serve);
        assert!(connect(&address).is_ok());
        server.abort();
        let _ = server.await;
        assert!(connect(&address).is_err_and(|e| e.kind() == ErrorKind::ConnectionRefused));
    }
}
