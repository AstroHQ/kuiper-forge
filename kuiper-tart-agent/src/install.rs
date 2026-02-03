//! macOS LaunchAgent installation for kuiper-tart-agent.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// The launchd service label.
pub const SERVICE_LABEL: &str = "com.astropad.kuiper-tart-agent";

/// Get the LaunchAgents directory.
pub fn launch_agents_dir() -> PathBuf {
    dirs::home_dir()
        .expect("No home directory")
        .join("Library/LaunchAgents")
}

/// Get the plist file path.
pub fn plist_path() -> PathBuf {
    launch_agents_dir().join(format!("{SERVICE_LABEL}.plist"))
}

/// Get the launchctl domain target for the current user.
fn domain_target() -> String {
    // Get UID from home directory metadata
    let uid = dirs::home_dir()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.uid())
        .unwrap_or(501); // Default to typical macOS user UID
    format!("gui/{uid}")
}

/// Check if the binary is in PATH and return its absolute path.
pub fn find_binary_in_path() -> Result<PathBuf> {
    let output = Command::new("which")
        .arg("kuiper-tart-agent")
        .output()
        .map_err(|e| Error::Install(format!("Failed to run 'which': {e}")))?;

    if !output.status.success() {
        return Err(Error::Install(
            "kuiper-tart-agent not found in PATH. \
             Install the binary to a directory in PATH (e.g., /usr/local/bin/) first."
                .to_string(),
        ));
    }

    let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path_str))
}

/// Generate the plist XML content.
pub fn generate_plist(binary_path: &Path, config_path: &Path, log_dir: &Path) -> String {
    let home_dir = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/Users/nobody".to_string());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>--config</string>
        <string>{config}</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>

    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>StandardOutPath</key>
    <string>{log_dir}/launchd-stdout.log</string>

    <key>StandardErrorPath</key>
    <string>{log_dir}/launchd-stderr.log</string>

    <key>WorkingDirectory</key>
    <string>{home_dir}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home_dir}</string>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
</dict>
</plist>
"#,
        binary = binary_path.display(),
        config = config_path.display(),
        log_dir = log_dir.display(),
    )
}

/// Load the service using launchctl.
pub fn load_service() -> Result<()> {
    let plist = plist_path();
    let domain = domain_target();

    // Use modern bootstrap syntax (macOS 10.10+)
    let output = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist.display().to_string()])
        .output()
        .map_err(|e| Error::Install(format!("Failed to run launchctl: {e}")))?;

    if !output.status.success() {
        // Fall back to legacy 'load' command
        let output = Command::new("launchctl")
            .args(["load", &plist.display().to_string()])
            .output()
            .map_err(|e| Error::Install(format!("Failed to run launchctl load: {e}")))?;

        if !output.status.success() {
            return Err(Error::Install(format!(
                "launchctl load failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }

    Ok(())
}

/// Unload the service using launchctl.
pub fn unload_service() -> Result<()> {
    let domain = domain_target();
    let service_target = format!("{domain}/{SERVICE_LABEL}");

    // Use modern bootout syntax
    let output = Command::new("launchctl")
        .args(["bootout", &service_target])
        .output()
        .map_err(|e| Error::Install(format!("Failed to run launchctl: {e}")))?;

    if !output.status.success() {
        // Fall back to legacy 'unload' command
        let plist = plist_path();
        let _ = Command::new("launchctl")
            .args(["unload", &plist.display().to_string()])
            .output();
        // Ignore errors - service might not be loaded
    }

    Ok(())
}

/// Check if the service is currently running.
pub fn is_service_running() -> bool {
    let domain = domain_target();
    let service_target = format!("{domain}/{SERVICE_LABEL}");

    let output = Command::new("launchctl")
        .args(["print", &service_target])
        .output();

    output.map(|o| o.status.success()).unwrap_or(false)
}
