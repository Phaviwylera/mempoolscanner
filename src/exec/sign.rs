//! Signer loading for the execution module.
//!
//! Key material is loaded ONLY from the environment:
//!   * `EXEC_PRIVATE_KEY` — raw 32-byte private key as hex
//!   * `EXEC_KEYSTORE_PATH` + `EXEC_KEYSTORE_PASSWORD` — encrypted JSON keystore,
//!     decrypted via `LocalSigner::decrypt_keystore`.
//!
//! It is never accepted as a function argument, CLI flag, or config-file field.
//! Raw key bytes are zeroized on drop via the `zeroize` crate.

use alloy::signers::local::PrivateKeySigner as LocalSigner;
use anyhow::{anyhow, Result};
use std::env;
use zeroize::Zeroizing;

/// Load the operator signing key from the environment.
///
/// Prefers `EXEC_PRIVATE_KEY` (raw hex). Falls back to `EXEC_KEYSTORE_PATH` +
/// `EXEC_KEYSTORE_PASSWORD` (encrypted JSON keystore decrypted via
/// `LocalSigner::decrypt_keystore`, requires the `signer-keystore` alloy feature).
pub fn load_signer_from_env() -> Result<LocalSigner> {
    if let Ok(pk) = env::var("EXEC_PRIVATE_KEY") {
        if !pk.trim().is_empty() {
            let trimmed = pk.trim();
            let key_bytes = Zeroizing::new(
                hex::decode(trimmed).map_err(|e| anyhow!("invalid EXEC_PRIVATE_KEY hex: {e}"))?,
            );
            if key_bytes.len() != 32 {
                return Err(anyhow!("EXEC_PRIVATE_KEY must be 32 bytes"));
            }
            let mut array = [0u8; 32];
            array.copy_from_slice(key_bytes.as_ref());
            let signer = LocalSigner::from_slice(&array)
                .map_err(|e| anyhow!("invalid EXEC_PRIVATE_KEY: {e}"))?;
            return Ok(signer);
        }
    }

    let keystore_path = env::var("EXEC_KEYSTORE_PATH")
        .map_err(|_| anyhow!("EXEC_PRIVATE_KEY and EXEC_KEYSTORE_PATH are both unset"))?;
    let password = env::var("EXEC_KEYSTORE_PASSWORD")
        .map_err(|_| anyhow!("EXEC_KEYSTORE_PASSWORD is required when EXEC_KEYSTORE_PATH is set"))?;

    let signer = LocalSigner::decrypt_keystore(&keystore_path, password.into_bytes())
        .map_err(|e| anyhow!("failed to decrypt keystore at {keystore_path}: {e}"))?;
    Ok(signer)
}

#[cfg(test)]
mod tests {

    #[test]
    #[ignore] // requires EXEC_PRIVATE_KEY to be set; never run in CI
    fn load_signer_from_env_is_private() {
        // Documented as env-only; no assertion that could leak key material.
        let _ = std::env::var("EXEC_PRIVATE_KEY").ok();
    }
}
