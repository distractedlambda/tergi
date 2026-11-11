use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Helper modes used when git calls back into tergi.
    if args.get(1).map(String::as_str) == Some("__seq-editor") && args.len() >= 4 {
        return match tergi::git::rebase::seq_editor_main(&PathBuf::from(&args[2]), &PathBuf::from(&args[3])) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("tergi: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    ExitCode::SUCCESS
}
