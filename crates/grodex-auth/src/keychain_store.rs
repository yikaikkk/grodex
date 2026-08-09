//! macOS Keychain-backed SecretStore using the `security-framework` crate.

use async_trait::async_trait;
use security_framework::base::Error as SfError;
use security_framework::passwords;

use crate::secret_store::{SecretStore, SecretStoreError};

pub struct MacKeychainSecretStore {
    service: String,
    access_group: Option<String>,
}

impl MacKeychainSecretStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            access_group: None,
        }
    }

    pub fn with_access_group(mut self, access_group: impl Into<String>) -> Self {
        self.access_group = Some(access_group.into());
        self
    }
}

impl Default for MacKeychainSecretStore {
    fn default() -> Self {
        Self::new("grodex")
    }
}

fn map_sf_error(e: SfError) -> SecretStoreError {
    let code = e.code();
    match code {
        -25300 => SecretStoreError::NotFound,
        -67061 | -67062 | -25293 | -25294 | -25295 => SecretStoreError::AccessDenied,
        -25291 | -25292 => SecretStoreError::BackendUnavailable,
        _ => SecretStoreError::IoError(format!(
            "security-framework (code {code}): {e}"
        )),
    }
}

#[async_trait]
impl SecretStore for MacKeychainSecretStore {
    async fn store(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        let service = self.service.clone();
        let account = key.to_string();
        let password = value.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || -> Result<(), SecretStoreError> {
            passwords::set_generic_password(&service, &account, &password)
                .map_err(map_sf_error)
        })
        .await
        .map_err(|e| SecretStoreError::IoError(format!("spawn_blocking: {e}")))?
    }

    async fn retrieve(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        let service = self.service.clone();
        let account = key.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<String>, SecretStoreError> {
            match passwords::get_generic_password(&service, &account) {
                Ok(bytes) => {
                    let s = String::from_utf8(bytes)
                        .map_err(|e| SecretStoreError::IoError(format!("utf8: {e}")))?;
                    Ok(Some(s))
                }
                Err(e) => {
                    let se = map_sf_error(e);
                    if matches!(se, SecretStoreError::NotFound) {
                        Ok(None)
                    } else {
                        Err(se)
                    }
                }
            }
        })
        .await
        .map_err(|e| SecretStoreError::IoError(format!("spawn_blocking: {e}")))?
    }

    async fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        let service = self.service.clone();
        let account = key.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), SecretStoreError> {
            passwords::delete_generic_password(&service, &account).map_err(map_sf_error)
        })
        .await
        .map_err(|e| SecretStoreError::IoError(format!("spawn_blocking: {e}")))?
    }
}

pub async fn list_ids(_service: &str) -> Result<Vec<String>, SecretStoreError> {
    Ok(Vec::new())
}
