//! Command-line entrypoint for `leiser`.

use std::io::{self, Write};
use std::process::ExitCode;

use leiser_cli::{CliError, HELP_TEXT, ParseOutcome, parse_args, run};

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "leiser: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> Result<(), CliError> {
    let outcome = parse_args(std::env::args_os().skip(1))?;
    let mut stdout = io::stdout().lock();
    let cli = match outcome {
        ParseOutcome::Help => {
            writeln!(stdout, "{HELP_TEXT}")?;
            return Ok(());
        }
        ParseOutcome::Run(cli) => cli,
    };
    let cwd = std::env::current_dir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(&cli, &cwd, &mut stdout))
}
