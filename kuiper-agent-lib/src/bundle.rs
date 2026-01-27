//! Registration bundle: combines token + CA cert + coordinator URL into a single string.
//!
//! Format: `kfr1_<base64url-encoded JSON>`
//!
//! The JSON payload has the structure:
//! ```json
//! {"t":"reg_xxx...","ca":"-----BEGIN CERTIFICATE-----\n...","u":"https://coordinator:9443"}
//! ```

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

/// Prefix for kuiper-forge registration bundles (version 1).
const BUNDLE_PREFIX: &str = "kfr1_";

/// A registration bundle containing everything an agent needs to securely register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationBundle {
    /// Registration token (e.g., "reg_xxx...")
    #[serde(rename = "t")]
    pub token: String,

    /// CA certificate PEM for verifying the coordinator's TLS certificate
    #[serde(rename = "ca")]
    pub ca_cert_pem: String,

    /// Coordinator URL (e.g., "https://coordinator.example.com:9443")
    #[serde(rename = "u")]
    pub coordinator_url: String,
}

impl RegistrationBundle {
    /// Check if a string looks like a registration bundle.
    pub fn is_bundle(input: &str) -> bool {
        input.starts_with(BUNDLE_PREFIX)
    }

    /// Encode the bundle into a prefixed base64url string.
    pub fn encode(&self) -> Result<String, BundleError> {
        let json = serde_json::to_string(self).map_err(BundleError::Serialize)?;
        let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
        Ok(format!("{BUNDLE_PREFIX}{encoded}"))
    }

    /// Decode a prefixed base64url string into a registration bundle.
    pub fn decode(input: &str) -> Result<Self, BundleError> {
        let payload = input
            .strip_prefix(BUNDLE_PREFIX)
            .ok_or(BundleError::InvalidPrefix)?;

        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(BundleError::Base64)?;

        let bundle: Self = serde_json::from_slice(&bytes).map_err(BundleError::Deserialize)?;

        if bundle.token.is_empty() {
            return Err(BundleError::MissingField("token"));
        }
        if bundle.ca_cert_pem.is_empty() {
            return Err(BundleError::MissingField("ca_cert_pem"));
        }
        if bundle.coordinator_url.is_empty() {
            return Err(BundleError::MissingField("coordinator_url"));
        }

        Ok(bundle)
    }
}

/// Errors that can occur when encoding/decoding registration bundles.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("invalid bundle prefix (expected 'kfr1_')")]
    InvalidPrefix,
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JSON serialize error: {0}")]
    Serialize(serde_json::Error),
    #[error("JSON deserialize error: {0}")]
    Deserialize(serde_json::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let bundle = RegistrationBundle {
            token: "reg_abcdef1234567890abcdef1234567890".to_string(),
            ca_cert_pem: "-----BEGIN CERTIFICATE-----\nMIIBxDCCAWq...\n-----END CERTIFICATE-----\n".to_string(),
            coordinator_url: "https://coordinator.example.com:9443".to_string(),
        };

        let encoded = bundle.encode().unwrap();
        assert!(RegistrationBundle::is_bundle(&encoded));
        assert!(encoded.starts_with("kfr1_"));

        let decoded = RegistrationBundle::decode(&encoded).unwrap();
        assert_eq!(decoded.token, bundle.token);
        assert_eq!(decoded.ca_cert_pem, bundle.ca_cert_pem);
        assert_eq!(decoded.coordinator_url, bundle.coordinator_url);
    }

    #[test]
    fn test_is_bundle() {
        assert!(RegistrationBundle::is_bundle("kfr1_abc"));
        assert!(!RegistrationBundle::is_bundle("reg_abc"));
        assert!(!RegistrationBundle::is_bundle(""));
    }

    #[test]
    fn test_invalid_prefix() {
        let err = RegistrationBundle::decode("bad_prefix").unwrap_err();
        assert!(matches!(err, BundleError::InvalidPrefix));
    }

    #[test]
    fn test_invalid_base64() {
        let err = RegistrationBundle::decode("kfr1_!!!invalid!!!").unwrap_err();
        assert!(matches!(err, BundleError::Base64(_)));
    }

    #[test]
    fn test_missing_fields() {
        let json = serde_json::json!({"t": "", "ca": "cert", "u": "url"});
        let encoded = URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());
        let err = RegistrationBundle::decode(&format!("kfr1_{encoded}")).unwrap_err();
        assert!(matches!(err, BundleError::MissingField("token")));
    }
}
