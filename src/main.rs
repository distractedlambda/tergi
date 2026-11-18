use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::EventStream;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use tokio_stream::StreamExt as _;

use tergi::app::{App, AppEvent, REFRESH_ALL, Repo, Suspend};
use tergi::config::Config;
use tergi::terminal;

/// A terminal git client inspired by Sublime Merge.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The repository to open. Defaults to the current directory.
    path: Option<PathBuf>,
}

fn main() -> ExitCode {
    // Arguments and the socket path can be paths that aren't UTF-8, so
    // they are read as they are. `std::env::args` would panic on them.
    let args: Vec<OsString> = std::env::args_os().collect();
    // Modes used when git calls back into tergi.
    if let Some(sock) = std::env::var_os(tergi::askpass::SOCKET_ENV) {
        let prompt = args.get(1).map(|p| p.to_string_lossy());
        return tergi::askpass::client_main(&sock, prompt.as_deref().unwrap_or("Password:"));
    }
    if args.get(1).is_some_and(|a| a == "__seq-editor") && args.len() >= 4 {
        return match tergi::git::rebase::seq_editor_main(Path::new(&args[2]), Path::new(&args[3])) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tergi: {e:#}");
                ExitCode::FAILURE
            }
        };
    }

    let cli = Cli::parse();
    let path = cli.path.unwrap_or_else(|| PathBuf::from("."));
    let result = tokio::runtime::Runtime::new()
        .context("could not start the async runtime")
        .and_then(|rt| {
            let r = rt.block_on(run(&path));
            // Don't wait for blocking work that is still running, such as a
            // history walk. Dropping the runtime would.
            rt.shutdown_background();
            r
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tergi: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: &Path) -> Result<()> {
    let repo = Repo::open(path)?;
    let config_path = Config::path();
    let (cfg, config_error) = config_path.as_deref().map(Config::load).unwrap_or_default();
    terminal::install_panic_hook();
    let mut term = terminal::setup().context("could not set up the terminal")?;
    let mut app = App::new(repo, cfg, config_error);
    app.config_path = config_path;
    app.start();

    let result = async {
        let mut pointer = "default";
        let mut input = EventStream::new();
        loop {
            crossterm::execute!(term.backend_mut(), BeginSynchronizedUpdate)?;
            term.draw(|f| tergi::ui::draw::draw(&mut app, f))?;
            crossterm::execute!(term.backend_mut(), EndSynchronizedUpdate)?;
            let shape = app.pointer_shape();
            if shape != pointer {
                terminal::set_pointer(shape);
                pointer = shape;
            }

            let ev = tokio::select! {
                ev = app.next() => ev,
                Some(ev) = input.next() => match ev {
                    Ok(ev) => Some(AppEvent::Input(ev)),
                    Err(e) => return Err(anyhow::Error::from(e).context("could not read from the terminal")),
                },
            };
            let Some(ev) = ev else { break };
            app.handle(ev);
            // Handle everything that piled up before drawing again, so a
            // burst of mouse moves doesn't cause a burst of redraws.
            for _ in 0..1000 {
                match app.try_recv() {
                    Some(ev) => app.handle(ev),
                    None => break,
                }
            }
            if let Some(text) = app.clipboard.take() {
                terminal::copy(&text);
            }
            if app.quit {
                break;
            }
            if let Some(s) = app.suspend.take() {
                // Dropping the stream stops crossterm from reading the
                // terminal, so it doesn't take keys meant for the editor.
                drop(input);
                terminal::restore();
                let r = match s {
                    Suspend::Stop => {
                        let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
                        Ok(())
                    }
                    Suspend::Edit(file) => edit(&app.repo, &file).await,
                };
                terminal::resume(&mut term)?;
                input = EventStream::new();
                pointer = "default";
                if let Err(e) = r {
                    app.show_error("Couldn't run the editor", &e);
                }
                app.refresh(REFRESH_ALL);
            }
        }
        Ok(())
    }
    .await;

    terminal::restore();
    if let Some(p) = &app.repo.askpass_socket {
        tergi::askpass::cleanup(p);
    }
    result
}

/// Open a file in the user's editor, as configured for git.
async fn edit(repo: &Repo, file: &Path) -> Result<()> {
    let editor = repo.git.editor().await.unwrap_or_else(|_| "vi".into());
    // The setting is a shell command, as bytes, since it can name a program
    // by a path that isn't UTF-8.
    let script = [editor.as_bytes(), b" \"$@\""].concat();
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(OsString::from_vec(script))
        .arg("editor")
        .arg(file)
        .current_dir(&repo.workdir)
        .status()
        .await
        .context("could not start the editor")?;
    if !status.success() {
        anyhow::bail!("the editor exited with {status}");
    }
    Ok(())
}
