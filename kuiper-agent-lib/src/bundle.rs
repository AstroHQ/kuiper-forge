//! Registration bundle: combines token + server trust + coordinator URL into a single string.
//!
//! Format: `kfr1_<base64url-encoded JSON>`
//!
//! The JSON payload has the structure:
//! ```json
//! {"t":"reg_xxx...","ca":"-----BEGIN CERTIFICATE-----\n...","m":"ca","u":"https://coordinator:9443"}
//! ```

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// Prefix for kuiper-forge registration bundles (version 1).
const BUNDLE_PREFIX: &str = "kfr1_";

/// A registration bundle containing everything an agent needs to securely register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationBundle {
    /// Registration token (e.g., "reg_xxx...")
    #[serde(rename = "t")]
    pub token: String,

    /// Optional CA certificate PEM for verifying the coordinator's TLS certificate
    #[serde(rename = "ca", default, skip_serializing_if = "Option::is_none")]
    pub server_ca_pem: Option<String>,

    /// Server trust mode ("ca" or "chain")
    #[serde(rename = "m", default)]
    pub server_trust_mode: ServerTrustMode,

    /// Coordinator URL (e.g., "https://coordinator.example.com:9443")
    #[serde(rename = "u")]
    pub coordinator_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ServerTrustMode {
    #[default]
    Ca,
    Chain,
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
        if matches!(bundle.server_trust_mode, ServerTrustMode::Ca)
            && bundle
                .server_ca_pem
                .as_ref()
                .map(|p| p.is_empty())
                .unwrap_or(true)
        {
            return Err(BundleError::MissingField("server_ca_pem"));
        }
        if bundle.coordinator_url.is_empty() {
            return Err(BundleError::MissingField("coordinator_url"));
        }

        Ok(bundle)
    }

    /// Validate that the bundle's coordinator URL matches the expected coordinator URL.
    ///
    /// This ensures the CA certificate and token in the bundle are actually for the
    /// coordinator we're connecting to, preventing misconfiguration.
    ///
    /// URLs are normalized before comparison (lowercase, default ports removed, trailing slashes removed).
    pub fn validate_url(&self, expected_url: &str) -> Result<(), BundleError> {
        let bundle_normalized = normalize_url(&self.coordinator_url);
        let expected_normalized = normalize_url(expected_url);

        if bundle_normalized != expected_normalized {
            return Err(BundleError::UrlMismatch {
                bundle_url: self.coordinator_url.clone(),
                expected_url: expected_url.to_string(),
            });
        }

        Ok(())
    }
}

/// Normalize a URL for comparison purposes.
///
/// Normalization includes:
/// - Converting to lowercase
/// - Removing default ports (443 for https, 80 for http)
/// - Removing trailing slashes
fn normalize_url(url_str: &str) -> String {
    let Ok(mut url) = url::Url::parse(url_str) else {
        // If parsing fails, fall back to simple normalization
        return url_str.trim_end_matches('/').to_lowercase();
    };

    // Remove default ports (443 for https, 80 for http)
    if let Some(port) = url.port() {
        let is_default = matches!((url.scheme(), port), ("https", 443) | ("http", 80));
        if is_default {
            let _ = url.set_port(None);
        }
    }

    // Host is already lowercase from url crate, just format without trailing slash
    url.as_str().trim_end_matches('/').to_string()
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
    #[error(
        "Registration bundle URL mismatch: bundle was generated for '{bundle_url}' but config specifies '{expected_url}'. \
         The bundle's server trust and token are bound to a specific coordinator URL."
    )]
    UrlMismatch {
        bundle_url: String,
        expected_url: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let bundle = RegistrationBundle {
            token: "reg_abcdef1234567890abcdef1234567890".to_string(),
            server_ca_pem: Some(
                "-----BEGIN CERTIFICATE-----\nMIIBxDCCAWq...\n-----END CERTIFICATE-----\n"
                    .to_string(),
            ),
            server_trust_mode: ServerTrustMode::Ca,
            coordinator_url: "https://coordinator.example.com:9443".to_string(),
        };

        let encoded = bundle.encode().unwrap();
        assert!(RegistrationBundle::is_bundle(&encoded));
        assert!(encoded.starts_with("kfr1_"));

        let decoded = RegistrationBundle::decode(&encoded).unwrap();
        assert_eq!(decoded.token, bundle.token);
        assert_eq!(decoded.server_ca_pem, bundle.server_ca_pem);
        assert_eq!(decoded.server_trust_mode, bundle.server_trust_mode);
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
        let json = serde_json::json!({"t": "", "m": "ca", "ca": "cert", "u": "url"});
        let encoded = URL_SAFE_NO_PAD.encode(json.to_string().as_bytes());
        let err = RegistrationBundle::decode(&format!("kfr1_{encoded}")).unwrap_err();
        assert!(matches!(err, BundleError::MissingField("token")));
    }
}
