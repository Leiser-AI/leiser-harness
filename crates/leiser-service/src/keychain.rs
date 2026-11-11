//! Where the harness service keeps API keys: the OS keychain, never a file.
//!
//! The service talks to it through the [`Keychain`] interface, so tests can
//! give it a keychain in memory and never touch the developer's real one.

use std::sync::Arc;

use crate::ApiKey;

/// The keychain service name of every entry the harness creates.
pub const KEYCHAIN_SERVICE: &str = "leiser-harness";

/// Stores API keys under an account name, one key per account.
pub trait Keychain: Send + Sync + std::fmt::Debug {
    /// Stores `key`, replacing any key the account had.
    ///
    /// # Errors
    ///
    /// Returns a [`KeychainError`] when the keychain refuses.
    fn store(&self, account: &str, key: &ApiKey) -> Result<(), KeychainError>;

    /// Reads the account's key.
    ///
    /// # Errors
    ///
    /// Returns [`KeychainError::Missing`] when the account has no key, or
    /// another [`KeychainError`] when the keychain refuses.
    fn read(&self, account: &str) -> Result<ApiKey, KeychainError>;

    /// Deletes the account's key; an account without one is not an error.
    ///
    /// # Errors
    ///
    /// Returns a [`KeychainError`] when the keychain refuses.
    fn delete(&self, account: &str) -> Result<(), KeychainError>;
}

/// Why the keychain could not store, read or delete a key. Never contains
/// the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeychainError {
    #[error("the keychain has no key for {account}")]
    Missing { account: String },
    #[error("this platform has no OS keychain the harness supports")]
    Unsupported,
    #[error("the keychain failed: {reason}")]
    Failed { reason: String },
}

/// The OS keychain: Keychain on macOS, Credential Manager on Windows.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsKeychain;

impl OsKeychain {
    fn entry(account: &str) -> Result<keyring_core::Entry, KeychainError> {
        let store = os_store()?;
        store
            .build(KEYCHAIN_SERVICE, account, None)
            .map_err(|error| failed(&error))
    }
}

impl Keychain for OsKeychain {
    fn store(&self, account: &str, key: &ApiKey) -> Result<(), KeychainError> {
        Self::entry(account)?
            .set_password(key.expose())
            .map_err(|error| failed(&error))
    }

    fn read(&self, account: &str) -> Result<ApiKey, KeychainError> {
        match Self::entry(account)?.get_password() {
            Ok(key) => Ok(ApiKey::new(key)),
            Err(keyring_core::Error::NoEntry) => Err(KeychainError::Missing {
                account: account.to_owned(),
            }),
            Err(error) => Err(failed(&error)),
        }
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(failed(&error)),
        }
    }
}

#[cfg(target_os = "macos")]
fn os_store() -> Result<Arc<keyring_core::CredentialStore>, KeychainError> {
    let store: Arc<keyring_core::CredentialStore> =
        apple_native_keyring_store::keychain::Store::new().map_err(|error| failed(&error))?;
    Ok(store)
}

#[cfg(target_os = "windows")]
fn os_store() -> Result<Arc<keyring_core::CredentialStore>, KeychainError> {
    let store: Arc<keyring_core::CredentialStore> =
        windows_native_keyring_store::Store::new().map_err(|error| failed(&error))?;
    Ok(store)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_store() -> Result<Arc<keyring_core::CredentialStore>, KeychainError> {
    Err(KeychainError::Unsupported)
}

/// The keychain's error as text. Errors that carry stored bytes, which may
/// be a key, are reduced to their kind.
fn failed(error: &keyring_core::Error) -> KeychainError {
    let reason = match error {
        keyring_core::Error::BadEncoding(_) | keyring_core::Error::BadDataFormat(..) => {
            "the stored key could not be read".to_owned()
        }
        error => error.to_string(),
    };
    KeychainError::Failed { reason }
}

/// The account of a subscription's API key.
pub(crate) fn account(subscription: u64) -> String {
    format!("subscription-{subscription}")
}
