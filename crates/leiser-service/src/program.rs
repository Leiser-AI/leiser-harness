//! Starting the harness service as its own process (ADR 0007). The desktop
//! app starts it when none runs; the service starts its new version after
//! an update. Both use [`ServiceProgram`], so they agree on the arguments.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The file in the data directory that a launched service writes its
/// diagnostics and panics to.
pub const SERVICE_LOG_FILE: &str = "service.log";

/// The harness service program and what it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceProgram {
    program: PathBuf,
    data_dir: PathBuf,
    fake_agent_cli: Option<PathBuf>,
}

/// Why the service program's arguments do not parse.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArgsError {
    #[error("usage: leiser-harness-service --data-dir <dir> [--fake-agent-cli <program>]")]
    Usage,
}

impl ServiceProgram {
    /// The service at `program`, keeping its data in `data_dir`. Its chats
    /// run on the installed agent CLIs.
    #[must_use]
    pub fn new(program: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            program,
            data_dir,
            fake_agent_cli: None,
        }
    }

    /// Runs every chat on the testkit's fake agent CLI at `program`.
    #[must_use]
    pub fn with_fake_agent_cli(mut self, program: PathBuf) -> Self {
        self.fake_agent_cli = Some(program);
        self
    }

    /// Parses the arguments the service program was started with, after
    /// the program name. `program` is the service's own program.
    ///
    /// # Errors
    ///
    /// Returns [`ArgsError::Usage`] for unknown or missing arguments.
    pub fn parse(
        program: PathBuf,
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Self, ArgsError> {
        let mut args = args.into_iter();
        let mut data_dir = None;
        let mut fake_agent_cli = None;
        while let Some(flag) = args.next() {
            let value = args.next().map(PathBuf::from).ok_or(ArgsError::Usage)?;
            match flag.to_str() {
                Some("--data-dir") => data_dir = Some(value),
                Some("--fake-agent-cli") => fake_agent_cli = Some(value),
                _ => return Err(ArgsError::Usage),
            }
        }
        Ok(Self {
            program,
            data_dir: data_dir.ok_or(ArgsError::Usage)?,
            fake_agent_cli,
        })
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn fake_agent_cli(&self) -> Option<&Path> {
        self.fake_agent_cli.as_deref()
    }

    fn args(&self) -> Vec<OsString> {
        let mut args = vec!["--data-dir".into(), self.data_dir.clone().into()];
        if let Some(fake) = &self.fake_agent_cli {
            args.extend(["--fake-agent-cli".into(), fake.clone().into()]);
        }
        args
    }

    /// Starts the service program in the background, apart from the
    /// calling process: it keeps running when the caller exits, also when
    /// that is a terminal's process group. Its error output goes to
    /// [`SERVICE_LOG_FILE`] in the data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory, the log file or the
    /// process cannot be created.
    pub fn launch(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join(SERVICE_LOG_FILE))?;
        let mut command = std::process::Command::new(&self.program);
        command
            .args(self.args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        #[cfg(windows)]
        {
            // No console window, and no Ctrl+C from the caller's console.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            std::os::windows::process::CommandExt::creation_flags(
                &mut command,
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            );
        }
        let mut child = command.spawn()?;
        // Reaps the process once it exits, so it leaves no zombie behind
        // while the caller still runs.
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ArgsError, ServiceProgram};

    #[test]
    fn launch_arguments_parse_back_to_the_same_program() {
        let program = ServiceProgram::new("/bin/service".into(), "/data".into())
            .with_fake_agent_cli("/bin/fake".into());

        let parsed = ServiceProgram::parse(PathBuf::from("/bin/service"), program.args());

        assert_eq!(parsed, Ok(program));
    }

    #[test]
    fn a_data_directory_is_required() {
        let parsed = ServiceProgram::parse(PathBuf::from("/bin/service"), Vec::new());

        assert_eq!(parsed, Err(ArgsError::Usage));
    }
}
