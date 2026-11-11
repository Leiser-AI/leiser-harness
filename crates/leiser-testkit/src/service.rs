//! A harness service with the fake agent CLI and its own data directory.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leiser_service::{AgentCli, Driver, Service, ServiceConfig, ServiceHandle, StartError};
use tempfile::TempDir;

use crate::{FakeDriver, MemoryKeychain, Script};

/// Why [`TestService`] could not start.
#[derive(Debug, thiserror::Error)]
pub enum TestServiceError {
    #[error("could not set up the test: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Start(#[from] StartError),
}

/// One driver per agent CLI: new chats start on `default`. `repo-check`
/// runs on `repo_check`, if set.
#[derive(Debug)]
struct Drivers {
    default: (AgentCli, Arc<dyn Driver>),
    others: Vec<(AgentCli, Arc<dyn Driver>)>,
    repo_check: Option<Arc<dyn Driver>>,
}

/// A harness service on drivers, usually [`FakeDriver`]s, a temporary data
/// directory, a [`MemoryKeychain`] and a temporary shared Codex home, all
/// kept across [`TestService::restart`].
#[derive(Debug)]
pub struct TestService {
    service: Service,
    drivers: Drivers,
    data: TempDir,
    keychain: MemoryKeychain,
    codex_home: TempDir,
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
        Self::with(Drivers {
            default,
            others,
            repo_check: None,
        })
    }

    /// A harness service that runs `repo-check` on the fake agent CLI
    /// playing `repo_check`, whose chats play `chats`.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the drivers or the service
    /// cannot be set up.
    pub fn with_repo_check(
        program: &Path,
        chats: &Script,
        repo_check: &Script,
    ) -> Result<Self, TestServiceError> {
        Self::with(Drivers {
            default: (
                AgentCli::ClaudeCode,
                Arc::new(FakeDriver::new(program, chats)?),
            ),
            others: Vec::new(),
            repo_check: Some(Arc::new(FakeDriver::new(program, repo_check)?)),
        })
    }

    fn with(drivers: Drivers) -> Result<Self, TestServiceError> {
        let data = tempfile::tempdir()?;
        let keychain = MemoryKeychain::new();
        let codex_home = tempfile::tempdir()?;
        let service = start(&drivers, &data, &keychain, &codex_home)?;
        Ok(Self {
            service,
            drivers,
            data,
            keychain,
            codex_home,
        })
    }

    /// The keychain the harness service keeps API keys in.
    #[must_use]
    pub fn keychain(&self) -> &MemoryKeychain {
        &self.keychain
    }

    /// The shared Codex home that Codex subscriptions' shadow homes link to.
    #[must_use]
    pub fn codex_home(&self) -> PathBuf {
        self.codex_home.path().to_path_buf()
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
            keychain,
            codex_home,
        } = self;
        service.shutdown().await;
        let service = start(&drivers, &data, &keychain, &codex_home)?;
        Ok(Self {
            service,
            drivers,
            data,
            keychain,
            codex_home,
        })
    }
}

impl TestService {
    /// Updates the harness service as a new version would: waits until no
    /// turn runs ([`Service::stop_for_update`]), then starts the service
    /// again on the same data directory and agent CLIs.
    ///
    /// # Errors
    ///
    /// Returns a [`StartError`] when the service cannot start again.
    pub async fn update(self) -> Result<Self, StartError> {
        let Self {
            service,
            drivers,
            data,
            keychain,
            codex_home,
        } = self;
        service.stop_for_update().await;
        let service = start(&drivers, &data, &keychain, &codex_home)?;
        Ok(Self {
            service,
            drivers,
            data,
            keychain,
            codex_home,
        })
    }

    /// The harness service's data directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        self.data.path()
    }
}

fn start(
    drivers: &Drivers,
    data: &TempDir,
    keychain: &MemoryKeychain,
    codex_home: &TempDir,
) -> Result<Service, StartError> {
    let (agent_cli, driver) = &drivers.default;
    let mut config = drivers.others.iter().fold(
        ServiceConfig::new(*agent_cli, driver.clone(), data.path().to_path_buf())
            .with_keychain(Arc::new(keychain.clone()))
            .with_codex_home(codex_home.path().to_path_buf()),
        |config, (agent_cli, driver)| config.with_driver(*agent_cli, driver.clone()),
    );
    if let Some(driver) = &drivers.repo_check {
        config = config.with_repo_check(driver.clone());
    }
    leiser_service::start(config)
}
