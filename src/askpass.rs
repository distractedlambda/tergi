//! Letting git ask for passwords and passphrases through the UI.
//!
//! For remote operations we set `GIT_ASKPASS` and `SSH_ASKPASS` to our own
//! executable. When git or ssh runs it, it connects to a Unix socket owned
//! by the running tergi, which shows a prompt and sends back the answer.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crossbeam_channel::{Sender, bounded};

pub const SOCKET_ENV: &str = "TERGI_ASKPASS_SOCK";

/// Listen for prompts. `on_prompt` gets the prompt text and a channel for
/// the answer, where `None` means the user cancelled.
pub fn start_server(on_prompt: impl Fn(String, Sender<Option<String>>) + Send + 'static) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("tergi-askpass-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    std::thread::Builder::new()
        .name("askpass".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut prompt = String::new();
                if BufReader::new(&stream).read_line(&mut prompt).is_err() {
                    continue;
                }
                let (tx, rx) = bounded(1);
                on_prompt(prompt.trim_end().to_string(), tx);
                let reply = match rx.recv() {
                    Ok(Some(answer)) => format!("1{answer}"),
                    _ => "0".to_string(),
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        })
        .ok()?;
    Some(path)
}

pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Run as git's askpass helper: forward the prompt and print the answer.
pub fn client_main(socket: &str, prompt: &str) -> ExitCode {
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
