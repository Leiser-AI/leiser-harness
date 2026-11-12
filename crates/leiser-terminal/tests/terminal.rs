//! The terminal panel's shell (#40): it starts in the chat's checkout,
//! takes input, works for programs that ask for the terminal's size, and
//! leaves no processes behind when it is closed.

use std::error::Error;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use leiser_terminal::{Size, Terminal, TerminalError, TerminalEvent};

/// Login shells read the developer's profile first, which can take a while.
const TIMEOUT: Duration = Duration::from_secs(30);
const SIZE: Size = Size { rows: 24, cols: 80 };

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Everything a terminal reported so far.
#[derive(Default)]
struct Seen {
    output: Vec<u8>,
    exit_code: Option<u32>,
}

#[derive(Default)]
struct Screen {
    seen: Mutex<Seen>,
    changed: Condvar,
}

impl Screen {
    fn open(directory: &Path) -> TestResult<(Terminal, Arc<Self>)> {
        let screen = Arc::new(Self::default());
        let receiver = Arc::clone(&screen);
        let terminal = Terminal::open(directory, SIZE, move |event| {
            let mut seen = receiver.seen();
            match event {
                TerminalEvent::Output(bytes) => seen.output.extend(bytes),
                TerminalEvent::Exited { exit_code } => seen.exit_code = Some(exit_code),
            }
            receiver.changed.notify_all();
        })?;
        Ok((terminal, screen))
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until `found` finds something in what the terminal reported.
    fn wait<T>(&self, mut found: impl FnMut(&Seen) -> Option<T>) -> TestResult<T> {
        let deadline = Instant::now() + TIMEOUT;
        let mut seen = self.seen();
        loop {
            if let Some(value) = found(&seen) {
                return Ok(value);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let output = String::from_utf8_lossy(&seen.output);
                return Err(format!("timed out; output so far:\n{output}").into());
            }
            seen = self
                .changed
                .wait_timeout(seen, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Waits until `found` finds something in the output so far.
    fn wait_for<T>(&self, mut found: impl FnMut(&str) -> Option<T>) -> TestResult<T> {
        self.wait(|seen| found(&String::from_utf8_lossy(&seen.output)))
    }

    fn wait_for_text(&self, text: &str) -> TestResult {
        self.wait_for(|output| output.contains(text).then_some(()))
    }

    fn wait_for_exit(&self) -> TestResult<u32> {
        self.wait(|seen| seen.exit_code)
    }
}

/// Types a command line into the terminal, as the developer would.
fn type_line(terminal: &Terminal, line: &str) -> TestResult {
    Ok(terminal.write(format!("{line}\r").as_bytes())?)
}

/// The number that follows the first `label` that is followed by digits.
/// The echo of the typed command has the label too, but not the number.
fn number_after(output: &str, label: &str) -> Option<u32> {
    output.match_indices(label).find_map(|(start, _)| {
        let rest = &output[start + label.len()..];
        let end = rest
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    })
}

fn is_running(process_id: u32) -> TestResult<bool> {
    Ok(if cfg!(windows) {
        let listed = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {process_id}"), "/NH", "/FO", "CSV"])
            .output()?;
        String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{process_id}\""))
    } else {
        Command::new("kill")
            .args(["-0", &process_id.to_string()])
            .output()?
            .status
            .success()
    })
}

fn wait_until_ended(process_id: u32) -> TestResult {
    let deadline = Instant::now() + TIMEOUT;
    while is_running(process_id)? {
        if Instant::now() >= deadline {
            return Err(
                format!("process {process_id} still runs after the terminal was closed").into(),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[test]
fn the_shell_starts_in_the_given_directory_and_runs_typed_commands() -> TestResult {
    let checkout = tempfile::tempdir()?;
    std::fs::write(checkout.path().join("marker.txt"), "checkout-marker-4711")?;
    let (terminal, screen) = Screen::open(checkout.path())?;

    // `cat` reads files in the shell's directory in sh, zsh and PowerShell.
    type_line(&terminal, "cat marker.txt")?;

    screen.wait_for_text("checkout-marker-4711")
}

#[test]
fn programs_see_the_terminal_size_and_its_changes() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let (terminal, screen) = Screen::open(checkout.path())?;
    // A program that says the size (rows first, then columns), waits for a
    // line of input and says the size again. `stty` fails unless its input
    // is a terminal.
    let say_size_twice = if cfg!(windows) {
        let size =
            r#""size=$($Host.UI.RawUI.WindowSize.Height) $($Host.UI.RawUI.WindowSize.Width)""#;
        format!("{size}; $null = Read-Host; {size}")
    } else {
        r#"sh -c 'echo "size=$(stty size)"; read line; echo "size=$(stty size)"'"#.to_owned()
    };

    type_line(&terminal, &say_size_twice)?;
    screen.wait_for_text("size=24 80")?;
    // The size changes while the program runs, not between two commands: a
    // shell that returns to its prompt may write the size it just read back
    // to the terminal (bash's readline does), which undoes a change made in
    // between.
    terminal.resize(Size {
        rows: 30,
        cols: 100,
    })?;
    type_line(&terminal, "")?;

    screen.wait_for_text("size=30 100")
}

#[test]
fn the_shells_exit_is_reported_with_its_code() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let (terminal, screen) = Screen::open(checkout.path())?;

    type_line(&terminal, "exit 3")?;

    assert_eq!(screen.wait_for_exit()?, 3);
    Ok(())
}

#[test]
fn closing_the_terminal_ends_the_shell_and_the_program_it_runs() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let (terminal, screen) = Screen::open(checkout.path())?;
    let shell = terminal.process_id().ok_or("the shell has no process id")?;
    // A program that runs until it is stopped, and says its process id.
    let long_running = if cfg!(windows) {
        "$p = Start-Process ping.exe -ArgumentList '-n','300','127.0.0.1' -NoNewWindow -PassThru; \
         \"program=$($p.Id)\"; $p.WaitForExit()"
    } else {
        "sh -c 'echo program=$$; exec sleep 300'"
    };
    type_line(&terminal, long_running)?;
    let program = screen.wait_for(|output| number_after(output, "program="))?;
    assert!(is_running(program)?);

    drop(terminal);

    wait_until_ended(shell)?;
    wait_until_ended(program)
}

#[test]
fn a_directory_that_does_not_exist_is_refused() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let missing = checkout.path().join("removed-worktree");

    let opened = Terminal::open(&missing, SIZE, |_| {});

    assert!(matches!(
        opened,
        Err(TerminalError::NotADirectory { directory }) if directory == missing
    ));
    Ok(())
}
