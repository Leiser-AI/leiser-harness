//! A harness service with the fake agent CLI and its own data directory.

use std::io;
use std::path::Path;
use std::sync::Arc;

use leiser_service::{AgentCli, Driver, Service, ServiceConfig, ServiceHandle, StartError};
use tempfile::TempDir;

use crate::{FakeDriver, Script};

/// Why [`TestService`] could not start.
#[derive(Debug, thiserror::Error)]
pub enum TestServiceError {
    #[error("could not set up the test: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Start(#[from] StartError),
}

/// One driver per agent CLI: new chats start on `default`.
#[derive(Debug)]
struct Drivers {
    default: (AgentCli, Arc<dyn Driver>),
    others: Vec<(AgentCli, Arc<dyn Driver>)>,
}

/// A harness service on drivers, usually [`FakeDriver`]s, and a temporary
/// data directory, all kept across [`TestService::restart`].
#[derive(Debug)]
pub struct TestService {
    service: Service,
    drivers: Drivers,
    data: TempDir,
}

impl TestService {
    /// A harness service whose chats run on Claude Code, played by the
    /// fake agent CLI. `program` is the `fake-agent-cli` binary;
    /// integration tests get it from `env!("CARGO_BIN_EXE_fake-agent-cli")`.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the driver or the service cannot
    /// be set up.
    pub fn start(program: &Path, script: &Script) -> Result<Self, TestServiceError> {
        Self::with_driver(
            AgentCli::ClaudeCode,
            Arc::new(FakeDriver::new(program, script)?),
        )
    }

    /// A harness service on any driver, such as a real agent CLI's.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the data directory or the
    /// service cannot be set up.
    pub fn with_driver(
        agent_cli: AgentCli,
        driver: Arc<dyn Driver>,
    ) -> Result<Self, TestServiceError> {
        Self::with_drivers((agent_cli, driver), Vec::new())
    }

    /// A harness service whose chats start on `default` and can change to
    /// the agent CLIs of `others`.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the data directory or the
    /// service cannot be set up.
    pub fn with_drivers(
        default: (AgentCli, Arc<dyn Driver>),
        others: Vec<(AgentCli, Arc<dyn Driver>)>,
    ) -> Result<Self, TestServiceError> {
        let drivers = Drivers { default, others };
        let data = tempfile::tempdir()?;
        let service = start(&drivers, &data)?;
        Ok(Self {
            service,
            drivers,
            data,
        })
    }

    /// Opens a connection, as the desktop app does.
    #[must_use]
    pub fn connect(&self) -> ServiceHandle {
        self.service.connect()
    }

    /// Shuts the harness service down and starts it again on the same data
    /// directory and agent CLIs.
    ///
    /// # Errors
    ///
    /// Returns a [`StartError`] when the service cannot start again.
    pub async fn restart(self) -> Result<Self, StartError> {
        let Self {
            service,
            drivers,
            data,
        } = self;
        service.shutdown().await;
        let service = start(&drivers, &data)?;
        Ok(Self {
            service,
            drivers,
            data,
        })
    }
}

fn start(drivers: &Drivers, data: &TempDir) -> Result<Service, StartError> {
    let (agent_cli, driver) = &drivers.default;
    let config = drivers.others.iter().fold(
        ServiceConfig::new(*agent_cli, driver.clone(), data.path().to_path_buf()),
        |config, (agent_cli, driver)| config.with_driver(*agent_cli, driver.clone()),
    );
    leiser_service::start(config)
}
