//! Host environment checks for the Tart agent.
//!
//! Validates macOS host settings required for proper VM operation.

use std::collections::HashSet;
use std::process::Command;

/// Required major version of Tart CLI.
const REQUIRED_TART_MAJOR: u32 = 2;

/// Expected DHCP lease time in seconds for Internet Sharing.
const EXPECTED_LEASE_SECS: u32 = 600;

/// Path to the Internet Sharing plist file.
const PLIST_PATH: &str =
    "/Library/Preferences/SystemConfiguration/com.apple.InternetSharing.default.plist";

/// Check that tart CLI is installed and is version 2.x.
///
/// Returns `Ok(version_string)` if tart is present and version 2.x,
/// or `Err` with a descriptive message if not found or wrong version.
pub fn check_tart_version() -> Result<String, String> {
    let output = Command::new("tart")
        .arg("--version")
        .output()
        .map_err(|e| {
            format!("Tart CLI not found: {e}. Install from https://github.com/cirruslabs/tart")
        })?;

    if !output.status.success() {
        return Err("Tart CLI failed to report version".to_string());
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version = version_output.trim();

    // Parse major version from output like "2.12.0" or "tart 2.12.0"
    let version_str = version
        .split_whitespace()
        .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(version);

    let major = version_str
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok());

    match major {
        Some(v) if v == REQUIRED_TART_MAJOR => Ok(version_str.to_string()),
        Some(v) => Err(format!(
            "Tart version {version_str} not supported (found major version {v}, require {REQUIRED_TART_MAJOR}.x)"
        )),
        None => Err(format!("Could not parse Tart version from: {version}")),
    }
}

/// Check that all configured local images exist.
///
/// OCI images (containing a registry domain like `ghcr.io/`) are skipped
/// since tart will pull them automatically. Local images must already exist.
///
/// Returns `Ok(())` if all local images exist, or `Err` listing missing images.
pub fn check_local_images(images: &[&str]) -> Result<(), String> {
    // Get list of local images from tart
    let output = Command::new("tart")
        .args(["list", "--format", "json"])
        .output()
        .map_err(|e| format!("Failed to run 'tart list': {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "Failed to list tart images: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let local_images: HashSet<String> = serde_json::from_slice::<Vec<TartImage>>(&output.stdout)
        .map_err(|e| format!("Failed to parse tart list output: {e}"))?
        .into_iter()
        .map(|img| img.name)
        .collect();

    // Check each configured image
    let mut missing = Vec::new();
    for image in images {
        if !is_oci_image(image) && !local_images.contains(*image) {
            missing.push(*image);
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Local image(s) not found: {}. Pull with 'tart pull <image>' or use OCI URLs.",
            missing.join(", ")
        ))
    }
}

#[derive(serde::Deserialize)]
struct TartImage {
    #[serde(rename = "Name", alias = "name")]
    name: String,
}

/// Check if an image reference is an OCI URL (will be pulled automatically).
///
/// OCI images have a registry domain before the first `/`, e.g.:
/// - `ghcr.io/cirruslabs/macos-sequoia-base:latest` -> OCI
/// - `docker.io/library/image` -> OCI
/// - `macos-sequoia-base` -> local
/// - `my-image:latest` -> local
fn is_oci_image(image: &str) -> bool {
    // Look for pattern: domain.tld/path or domain:port/path
    if let Some(slash_pos) = image.find('/') {
        let prefix = &image[..slash_pos];
        // Check if prefix looks like a domain (contains . or :)
        prefix.contains('.') || prefix.contains(':')
    } else {
        false
    }
}

/// Check if DHCP lease time is configured correctly for Internet Sharing.
///
/// Returns `Ok(())` if the setting is present and <= `EXPECTED_LEASE_SECS`,
/// or `Err` with a descriptive message if not configured or too high.
pub fn check_dhcp_lease_time() -> Result<(), String> {
    let output = Command::new("defaults")
        .args(["read", PLIST_PATH, "bootpd"])
        .output()
        .map_err(|e| format!("Failed to run 'defaults' command: {e}"))?;

    if !output.status.success() {
        // The plist or key doesn't exist
        return Err(format!(
            "DHCP lease time not configured. Internet Sharing bootpd settings not found at {PLIST_PATH}"
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse the plist output for DHCPLeaseTimeSecs
    // Output format is like:
    // {
    //     DHCPLeaseTimeSecs = 600;
    // }
    if let Some(lease_time) = parse_dhcp_lease_time(&stdout) {
        if lease_time <= EXPECTED_LEASE_SECS {
            Ok(())
        } else {
            Err(format!(
                "DHCP lease time is {lease_time}s, should be <= {EXPECTED_LEASE_SECS}s for reliable VM IP acquisition"
            ))
        }
    } else {
        Err(format!(
            "DHCPLeaseTimeSecs not set in Internet Sharing configuration (expected <= {EXPECTED_LEASE_SECS}s)"
        ))
    }
}

/// Parse DHCPLeaseTimeSecs from `defaults read` output.
fn parse_dhcp_lease_time(output: &str) -> Option<u32> {
    // Look for "DHCPLeaseTimeSecs = <number>;" pattern
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("DHCPLeaseTimeSecs") {
            // Extract the number after '='
            if let Some(eq_pos) = trimmed.find('=') {
                let value_part = trimmed[eq_pos + 1..].trim();
                // Remove trailing semicolon if present
                let value_str = value_part.trim_end_matches(';').trim();
                return value_str.parse().ok();
            }
        }
    }
    None
}

/// Get the command to fix DHCP lease time.
pub fn dhcp_lease_fix_command() -> String {
    format!(
        "sudo defaults write {PLIST_PATH} bootpd -dict DHCPLeaseTimeSecs -int {EXPECTED_LEASE_SECS}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dhcp_lease_time() {
        let output = r#"{
    DHCPLeaseTimeSecs = 600;
    SomeOtherKey = value;
}"#;
        assert_eq!(parse_dhcp_lease_time(output), Some(600));
    }

    #[test]
    fn test_parse_dhcp_lease_time_missing() {
        let output = r#"{
    SomeOtherKey = value;
}"#;
        assert_eq!(parse_dhcp_lease_time(output), None);
    }

    #[test]
    fn test_parse_dhcp_lease_time_different_value() {
        let output = "    DHCPLeaseTimeSecs = 86400;";
        assert_eq!(parse_dhcp_lease_time(output), Some(86400));
    }

    #[test]
    fn test_parse_tart_version() {
        // Test helper to extract version parsing logic
        fn parse_major(version_output: &str) -> Option<u32> {
            let version = version_output.trim();
            let version_str = version
                .split_whitespace()
                .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .unwrap_or(version);
            version_str.split('.').next().and_then(|s| s.parse().ok())
        }

        // Plain version
        assert_eq!(parse_major("2.12.0"), Some(2));
        // With prefix
        assert_eq!(parse_major("tart 2.12.0"), Some(2));
        // Version 1
        assert_eq!(parse_major("1.5.0"), Some(1));
        // With newline
        assert_eq!(parse_major("2.12.0\n"), Some(2));
    }

    #[test]
    fn test_is_oci_image() {
        // OCI images (have registry domain)
        assert!(is_oci_image("ghcr.io/cirruslabs/macos-sequoia-base:latest"));
        assert!(is_oci_image("docker.io/library/image"));
        assert!(is_oci_image("registry.example.com:5000/image"));

        // Local images (no registry domain)
        assert!(!is_oci_image("macos-sequoia-base"));
        assert!(!is_oci_image("my-image:latest"));
        assert!(!is_oci_image("local/image")); // no dot in prefix = local
    }
}
