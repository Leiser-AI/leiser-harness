//! A scripted stand-in for an agent CLI, used by the harness service's tests.
//!
//! Usage: `fake-agent-cli <script.json>`. Reads one [`FakeInput`] per line on
//! standard input and answers each message with the next scripted turn,
//! written as one [`AgentEvent`] per line on standard output. Input keeps
//! being read while a turn plays, so a turn can be steered or interrupted.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use leiser_service::AgentEvent;
use leiser_testkit::{FakeInput, Script, Step};

/// How often a turn waiting at a gate looks for the gate file.
const GATE_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("usage: fake-agent-cli <script.json>")]
    Usage,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected input: {0}")]
    Unexpected(&'static str),
    #[error("no steered message to echo")]
    NoSteer,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "fake-agent-cli: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), FakeError> {
    let script_file = std::env::args_os().nth(1).ok_or(FakeError::Usage)?;
    let script: Script = serde_json::from_slice(&std::fs::read(script_file)?)?;
    let inputs = read_inputs();
    let mut turns = script.turns.into_iter();
    let mut out = io::stdout().lock();
    while let Ok(input) = inputs.recv() {
        match input? {
            FakeInput::UserMessage { text } => {
                let mut turn = Turn {
                    message: text,
                    steer: None,
                    supports_steer: script.supports_steer,
                    inputs: &inputs,
                    out: &mut out,
                };
                match turn.play(turns.next().unwrap_or_default())? {
                    Outcome::Ended => emit(&mut out, &AgentEvent::TurnEnded)?,
                    Outcome::Interrupted => emit(&mut out, &AgentEvent::TurnInterrupted)?,
                    Outcome::InputClosed => return Ok(()),
                }
            }
            FakeInput::Steer { .. } => return Err(FakeError::Unexpected("steer between turns")),
            // An interrupt that arrives after its turn ended is harmless.
            FakeInput::Interrupt => {}
        }
    }
    Ok(())
}

/// Reads standard input on its own thread, so a turn can take input while
/// it plays. The channel closes when standard input does.
fn read_inputs() -> Receiver<Result<FakeInput, FakeError>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let input = line
                .map_err(FakeError::from)
                .and_then(|line| Ok(serde_json::from_str(&line)?));
            let failed = input.is_err();
            if sender.send(input).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

enum Outcome {
    Ended,
    Interrupted,
    InputClosed,
}

/// What a turn does with input that arrives while it plays.
enum MidTurn {
    Steered,
    Interrupted,
}

struct Turn<'a, W> {
    /// The message that started the turn.
    message: String,
    /// The last steered message of the turn.
    steer: Option<String>,
    supports_steer: bool,
    inputs: &'a Receiver<Result<FakeInput, FakeError>>,
    out: &'a mut W,
}

impl<W: Write> Turn<'_, W> {
    fn play(&mut self, steps: Vec<Step>) -> Result<Outcome, FakeError> {
        for step in steps {
            if let Some(outcome) = self.take_pending()? {
                return Ok(outcome);
            }
            if let Some(outcome) = self.step(step)? {
                return Ok(outcome);
            }
        }
        Ok(self.take_pending()?.unwrap_or(Outcome::Ended))
    }

    /// Plays one step; returns an outcome when the turn ends early.
    fn step(&mut self, step: Step) -> Result<Option<Outcome>, FakeError> {
        let event = match step {
            Step::Emit { event } => event,
            Step::EchoUserMessage => AgentEvent::Message {
                text: self.message.clone(),
            },
            Step::ReportWorkingDirectory => AgentEvent::Message {
                text: std::env::current_dir()?.display().to_string(),
            },
            Step::EchoSteer => AgentEvent::Message {
                text: self.steer.clone().ok_or(FakeError::NoSteer)?,
            },
            Step::WaitForSteer => return self.wait_for_steer(),
            Step::WaitForGate { gate } => return self.wait_for_gate(&gate),
        };
        emit(self.out, &event)?;
        Ok(None)
    }

    fn wait_for_steer(&mut self) -> Result<Option<Outcome>, FakeError> {
        let Ok(input) = self.inputs.recv() else {
            return Ok(Some(Outcome::InputClosed));
        };
        Ok(match self.mid_turn(input?)? {
            MidTurn::Steered => None,
            MidTurn::Interrupted => Some(Outcome::Interrupted),
        })
    }

    fn wait_for_gate(&mut self, gate: &Path) -> Result<Option<Outcome>, FakeError> {
        while !gate.exists() {
            match self.inputs.recv_timeout(GATE_POLL) {
                Ok(input) => {
                    if let MidTurn::Interrupted = self.mid_turn(input?)? {
                        return Ok(Some(Outcome::Interrupted));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(Some(Outcome::InputClosed)),
            }
        }
        Ok(None)
    }

    /// Handles input that already arrived; returns an outcome when it ends
    /// the turn.
    fn take_pending(&mut self) -> Result<Option<Outcome>, FakeError> {
        loop {
            match self.inputs.try_recv() {
                Ok(input) => {
                    if let MidTurn::Interrupted = self.mid_turn(input?)? {
                        return Ok(Some(Outcome::Interrupted));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => return Ok(Some(Outcome::InputClosed)),
            }
        }
    }

    fn mid_turn(&mut self, input: FakeInput) -> Result<MidTurn, FakeError> {
        match input {
            FakeInput::UserMessage { .. } => {
                Err(FakeError::Unexpected("message while a turn runs"))
            }
            FakeInput::Steer { .. } if !self.supports_steer => Err(FakeError::Unexpected(
                "steer, but steering is not supported",
            )),
            FakeInput::Steer { text } => {
                self.steer = Some(text);
                Ok(MidTurn::Steered)
            }
            FakeInput::Interrupt => Ok(MidTurn::Interrupted),
        }
    }
}

fn emit(out: &mut impl Write, event: &AgentEvent) -> Result<(), FakeError> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
