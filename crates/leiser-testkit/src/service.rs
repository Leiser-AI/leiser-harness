//! A harness service with the fake agent CLI and its own data directory.

use std::io;
use std::path::Path;
use std::sync::Arc;

use leiser_service::{Driver, Service, ServiceConfig, ServiceHandle, StartError};
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

/// A harness service on a driver, usually a [`FakeDriver`], and a temporary
/// data directory, both kept across [`TestService::restart`].
#[derive(Debug)]
pub struct TestService {
    service: Service,
    driver: Arc<dyn Driver>,
    data: TempDir,
}

impl TestService {
    /// `program` is the `fake-agent-cli` binary; integration tests get it
    /// from `env!("CARGO_BIN_EXE_fake-agent-cli")`.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the driver or the service cannot
    /// be set up.
    pub fn start(program: &Path, script: &Script) -> Result<Self, TestServiceError> {
        Self::with_driver(Arc::new(FakeDriver::new(program, script)?))
    }

    /// A harness service on any driver, such as a real agent CLI's.
    ///
    /// # Errors
    ///
    /// Returns a [`TestServiceError`] when the data directory or the
    /// service cannot be set up.
    pub fn with_driver(driver: Arc<dyn Driver>) -> Result<Self, TestServiceError> {
        let data = tempfile::tempdir()?;
        let service = start(&driver, &data)?;
        Ok(Self {
            service,
            driver,
            data,
        })
    }

    /// Opens a connection, as the desktop app does.
    #[must_use]
    pub fn connect(&self) -> ServiceHandle {
        self.service.connect()
    }

    /// Shuts the harness service down and starts it again on the same data
    /// directory and agent CLI.
    ///
    /// # Errors
    ///
    /// Returns a [`StartError`] when the service cannot start again.
    pub async fn restart(self) -> Result<Self, StartError> {
        let Self {
            service,
            driver,
            data,
        } = self;
        service.shutdown().await;
        let service = start(&driver, &data)?;
        Ok(Self {
            service,
            driver,
            data,
        })
    }
}

fn start(driver: &Arc<dyn Driver>, data: &TempDir) -> Result<Service, StartError> {
    leiser_service::start(ServiceConfig {
        driver: driver.clone(),
        data_dir: data.path().to_path_buf(),
    })
}
