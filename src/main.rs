use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

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
    let args: Vec<String> = std::env::args().collect();
    // Modes used when git calls back into tergi.
    if let Ok(sock) = std::env::var(tergi::askpass::SOCKET_ENV) {
        return tergi::askpass::client_main(&sock, args.get(1).map_or("Password:", String::as_str));
    }
    if args.get(1).map(String::as_str) == Some("__seq-editor") && args.len() >= 4 {
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
    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tergi: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &Path) -> Result<()> {
    let repo = Repo::open(path)?;
    let (cfg, config_error) = Config::load();
    terminal::install_panic_hook();
    let mut term = terminal::setup().context("could not set up the terminal")?;
    let mut app = App::new(repo, cfg, config_error);
    let tx = app.tx.clone();
    let input = terminal::Input::start(move |ev| {
        let _ = tx.send(AppEvent::Input(ev));
    });
    app.start();

    let result = (|| -> Result<()> {
        loop {
            crossterm::execute!(term.backend_mut(), BeginSynchronizedUpdate)?;
            term.draw(|f| tergi::ui::draw::draw(&mut app, f))?;
            crossterm::execute!(term.backend_mut(), EndSynchronizedUpdate)?;

            let Some(ev) = app.recv() else { break };
            app.handle(ev);
            // Handle everything that piled up before drawing again, so a
            // burst of mouse moves doesn't cause a burst of redraws.
            for _ in 0..1000 {
                match app.try_recv() {
                    Some(ev) => app.handle(ev),
                    None => break,
                }
            }
            if app.quit {
                break;
            }
            if let Some(s) = app.suspend.take() {
                input.pause();
                terminal::restore();
                let r = match s {
                    Suspend::Stop => {
                        let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
                        Ok(())
                    }
                    Suspend::Edit(file) => edit(&app.repo, &file),
                };
                terminal::resume(&mut term)?;
                input.resume();
                if let Err(e) = r {
                    app.show_error("Couldn't run the editor", &e);
                }
                app.refresh(REFRESH_ALL);
            }
        }
        Ok(())
    })();

    terminal::restore();
    if let Some(p) = &app.repo.askpass_socket {
        tergi::askpass::cleanup(p);
    }
    result
}

/// Open a file in the user's editor, as configured for git.
fn edit(repo: &Repo, file: &Path) -> Result<()> {
    let editor = repo.git.editor().unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("editor")
        .arg(file)
        .current_dir(&repo.workdir)
        .status()
        .context("could not start the editor")?;
    if !status.success() {
        anyhow::bail!("the editor exited with {status}");
    }
    Ok(())
}
