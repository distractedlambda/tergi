//! Letting git ask for passwords and passphrases through the UI.
//!
//! For remote operations we set `GIT_ASKPASS` and `SSH_ASKPASS` to our own
//! executable. When git or ssh runs it, it connects to a Unix socket owned
//! by the running tergi, which shows a prompt and sends back the answer.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::oneshot;

pub const SOCKET_ENV: &str = "TERGI_ASKPASS_SOCK";

/// Listen for prompts. `on_prompt` gets the prompt text and a channel for
/// the answer, where `None` means the user cancelled.
///
/// Returns the socket path and a future that answers prompts, one at a time.
/// The caller should spawn the future.
pub fn start_server(
    on_prompt: impl Fn(String, oneshot::Sender<Option<String>>) + Send + 'static,
) -> Option<(PathBuf, impl Future<Output = ()> + Send + 'static)> {
    // One process can start more than one server, as the tests do, so the
    // process id alone doesn't make the name unique.
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tergi-askpass-{}-{n}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // Bind here rather than in the future, so the socket exists as soon as
    // we return.
    let listener = std::os::unix::net::UnixListener::bind(&path).ok()?;
    listener.set_nonblocking(true).ok()?;
    let serve = async move {
        let Ok(listener) = UnixListener::from_std(listener) else {
            return;
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (read, mut write) = stream.into_split();
            let mut prompt = String::new();
            if BufReader::new(read).read_line(&mut prompt).await.is_err() {
                continue;
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
    Some((path, serve))
}

pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Run as git's askpass helper: forward the prompt and print the answer.
pub fn client_main(socket: &Path, prompt: &str) -> ExitCode {
    let Ok(mut stream) = UnixStream::connect(socket) else {
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
