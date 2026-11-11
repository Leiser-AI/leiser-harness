//! A keychain in memory, so tests never touch the developer's real one.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use leiser_service::{ApiKey, Keychain, KeychainError};

/// Keeps API keys in memory. Clones share their keys, so a test can keep
/// one clone to look into what the harness service stored.
#[derive(Debug, Clone, Default)]
pub struct MemoryKeychain {
    keys: Arc<Mutex<BTreeMap<String, ApiKey>>>,
}

impl MemoryKeychain {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored key by account.
    #[must_use]
    pub fn keys(&self) -> BTreeMap<String, ApiKey> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Keychain for MemoryKeychain {
    fn store(&self, account: &str, key: &ApiKey) -> Result<(), KeychainError> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(account.to_owned(), key.clone());
        Ok(())
    }

    fn read(&self, account: &str) -> Result<ApiKey, KeychainError> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(account)
            .cloned()
            .ok_or_else(|| KeychainError::Missing {
                account: account.to_owned(),
            })
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(account);
        Ok(())
    }
}
