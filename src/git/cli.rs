//! Running the `git` executable.
//!
//! Every write goes through here so that hooks, signing, credential helpers and
//! the user's config behave exactly as they do on the command line.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// One entry in the command log view.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: SystemTime,
    pub command: String,
    pub ok: bool,
    pub output: String,
}

/// Extra environment and input for a single git invocation.
#[derive(Default, Clone)]
pub struct RunOpts {
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    /// Treat these exit codes as success too (`git diff --no-index` exits 1
    /// when there are differences).
    pub ok_codes: Vec<i32>,
}

#[derive(Clone)]
pub struct Git {
    workdir: PathBuf,
    log: Arc<Mutex<Vec<LogEntry>>>,
}

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

const MAX_LOG: usize = 500;

impl Git {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn log_entries(&self) -> Vec<LogEntry> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn base_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.workdir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            // Never let git open an editor on our terminal. Callers that need a
            // specific editor override this.
            .env("GIT_EDITOR", "true")
            .args(["-c", "color.ui=false", "-c", "core.quotepath=false"])
            .stdin(Stdio::null());
        // These change how git reads every pathspec. With the first one,
        // git takes the `:(literal)` prefix from `ops::pathspec` as part of
        // the file name. With the others, a path could match other files.
        for var in [
            "GIT_LITERAL_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    /// Run a read-only command. These are not logged, and they don't take
    /// optional locks, so `git status` won't rewrite the index and wake up our
    /// own file watcher.
    pub async fn read<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        self.read_opts(args, &RunOpts::default()).await
    }

    pub async fn read_opts<S: AsRef<str>>(&self, args: &[S], opts: &RunOpts) -> Result<Output> {
        let mut cmd = self.base_command();
        // Nobody wants the answer once the task asking for it is gone. Commands
        // that change the repository are left to finish, so they don't stop
        // halfway through.
        cmd.env("GIT_OPTIONAL_LOCKS", "0").kill_on_drop(true);
        self.exec(cmd, args, opts, false).await
    }

    pub async fn read_text<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        Ok(self.read(args).await?.text())
    }

    /// The user's editor, as git would pick it. We set `GIT_EDITOR` on every
    /// command we run, so asking git with that in place would just return
    /// our own setting.
    pub async fn editor(&self) -> Result<String> {
        let mut cmd = self.base_command();
        cmd.env_remove("GIT_EDITOR");
        Ok(self
            .exec(cmd, &["var", "GIT_EDITOR"], &RunOpts::default(), false)
            .await?
            .text()
            .trim()
            .to_string())
    }

    /// Run a command that may change the repository. It is recorded in the
    /// command log whether it succeeds or not.
    pub async fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        self.run_opts(args, &RunOpts::default()).await
    }

    pub async fn run_opts<S: AsRef<str>>(&self, args: &[S], opts: &RunOpts) -> Result<Output> {
        let cmd = self.base_command();
        self.exec(cmd, args, opts, true).await
    }

    async fn exec<S: AsRef<str>>(
        &self,
        mut cmd: Command,
        args: &[S],
        opts: &RunOpts,
        logged: bool,
    ) -> Result<Output> {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        cmd.args(&args);
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if opts.stdin.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().context("could not run git")?;
        let stdin = child.stdin.take();
        // Write the input while reading the output, so a large patch can't
        // deadlock against git filling up its stdout pipe.
        let write = async {
            if let (Some(mut stdin), Some(input)) = (stdin, &opts.stdin) {
                let _ = stdin.write_all(input).await;
                // Dropping stdin here closes it, so git sees the end.
            }
        };
        let ((), out) = tokio::join!(write, child.wait_with_output());
        let out = out.context("git did not finish")?;
        let code = out.status.code().unwrap_or(-1);
        let ok = out.status.success() || opts.ok_codes.contains(&code);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if logged {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            output.push_str(&stderr);
            self.push_log(LogEntry {
                time: SystemTime::now(),
                command: format!("git {}", shell_join(&args)),
                ok,
                output,
            });
        }
        if !ok {
            let msg = if stderr.trim().is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            bail!("git {} failed:\n{}", args.first().unwrap_or(&""), msg);
        }
        Ok(Output {
            stdout: out.stdout,
            stderr,
        })
    }

    /// Run a long command such as fetch or push. Progress lines from stderr
    /// are passed to `progress` as they arrive. Git separates progress updates
    /// with carriage returns, so both `\r` and `\n` end a line.
    pub async fn run_streaming<S: AsRef<str>>(
        &self,
        args: &[S],
        opts: &RunOpts,
        mut progress: impl FnMut(&str),
    ) -> Result<Output> {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let mut cmd = self.base_command();
        cmd.args(&args);
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().context("could not run git")?;
        let mut stdout = child.stdout.take().context("no stdout")?;
        let read_stdout = async {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        };
        let mut stderr = child.stderr.take().context("no stderr")?;
        let read_stderr = async {
            let mut all_err = Vec::new();
            let mut line = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stderr.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                for &b in &chunk[..n] {
                    all_err.push(b);
                    if b == b'\r' || b == b'\n' {
                        if !line.is_empty() {
                            progress(String::from_utf8_lossy(&line).trim());
                            line.clear();
                        }
                    } else {
                        line.push(b);
                    }
                }
            }
            all_err
        };
        let (stdout, all_err) = tokio::join!(read_stdout, read_stderr);
        let status = child.wait().await.context("git did not finish")?;
        let stderr = clean_progress(&String::from_utf8_lossy(&all_err));
        let ok = status.success();
        let mut output = String::from_utf8_lossy(&stdout).into_owned();
        output.push_str(&stderr);
        self.push_log(LogEntry {
            time: SystemTime::now(),
            command: format!("git {}", shell_join(&args)),
            ok,
            output,
        });
        if !ok {
            bail!(
                "git {} failed:\n{}",
                args.first().unwrap_or(&""),
                stderr.trim()
            );
        }
        Ok(Output { stdout, stderr })
    }

    fn push_log(&self, entry: LogEntry) {
        if let Ok(mut log) = self.log.lock() {
            log.push(entry);
            if log.len() > MAX_LOG {
                let extra = log.len() - MAX_LOG;
                log.drain(..extra);
            }
        }
    }
}

/// Keep only the final state of each progress line, so the log doesn't fill
/// up with hundreds of "Receiving objects: 12%" updates.
fn clean_progress(s: &str) -> String {
    s.split('\n')
        .map(|line| {
            line.rsplit('\r')
                .find(|p| !p.trim().is_empty())
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Quote arguments for display in the command log.
pub fn shell_join(args: &[&str]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,^~{}".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn progress_cleanup() {
        let s = "Counting: 1%\rCounting: 50%\rCounting: 100%, done.\nok\n";
        assert_eq!(clean_progress(s), "Counting: 100%, done.\nok\n");
    }
}
