//! GitHub Actions runner version fetching and download URL construction.
//!
//! This module provides shared functionality for both kuiper-tart-agent and kuiper-proxmox-agent
//! to fetch the latest runner version and construct platform-specific download URLs.

use crate::shell::{escape_posix, escape_posix_path, escape_powershell};
use tracing::{debug, info};

/// Fallback GitHub Actions runner version if API fetch fails.
pub const FALLBACK_VERSION: &str = "2.321.0";

/// Target platform for the GitHub Actions runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Windows,
    MacOS,
}

impl Platform {
    /// Get the platform string used in GitHub runner download URLs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Linux => "linux",
            Platform::Windows => "win",
            Platform::MacOS => "osx",
        }
    }

    /// Get the file extension for the runner archive.
    pub fn extension(&self) -> &'static str {
        match self {
            Platform::Linux | Platform::MacOS => "tar.gz",
            Platform::Windows => "zip",
        }
    }
}

/// Target architecture for the GitHub Actions runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    /// Get the architecture string used in GitHub runner download URLs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Parse architecture from `uname -m` output.
    ///
    /// Returns `None` for unsupported architectures.
    pub fn from_uname(uname: &str) -> Option<Self> {
        match uname.trim() {
            "x86_64" | "amd64" => Some(Arch::X64),
            "arm64" | "aarch64" => Some(Arch::Arm64),
            _ => None,
        }
    }
}

/// Fetch the latest GitHub Actions runner version from the GitHub API.
///
/// Returns `None` if the fetch fails (caller should fall back to `FALLBACK_VERSION`).
pub async fn fetch_latest_version() -> Option<String> {
    let url = "https://api.github.com/repos/actions/runner/releases/latest";

    debug!("Fetching latest runner version from GitHub API");

    let output = tokio::process::Command::new("curl")
        .args(["-sL", "-H", "Accept: application/vnd.github.v3+json", url])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        debug!("Failed to fetch latest runner version from GitHub API");
        return None;
    }

    let body = String::from_utf8_lossy(&output.stdout);

    // Parse JSON to extract tag_name (e.g., "v2.321.0")
    // Simple parsing without adding serde_json dependency
    // Note: GitHub API returns single-line JSON, so we search the whole body
    if let Some(tag_idx) = body.find("\"tag_name\"") {
        let after_tag = &body[tag_idx..];
        // Extract version from: "tag_name": "v2.321.0" or "tag_name":"v2.321.0"
        if let Some(v_idx) = after_tag
            .find(':')
            .and_then(|colon| after_tag[colon..].find("\"v").map(|v| colon + v + 2))
            && let Some(end) = after_tag[v_idx..].find('"')
        {
            let version = &after_tag[v_idx..v_idx + end];
            info!("Latest GitHub Actions runner version: v{}", version);
            return Some(version.to_string());
        }
    }

    debug!("Could not parse runner version from GitHub API response");
    None
}

/// Construct the download URL for a GitHub Actions runner release.
///
/// # Example
/// ```
/// use kuiper_agent_lib::github_runner::{download_url, Platform, Arch};
///
/// let url = download_url("2.321.0", Platform::Linux, Arch::X64);
/// assert_eq!(
///     url,
///     "https://github.com/actions/runner/releases/download/v2.321.0/actions-runner-linux-x64-2.321.0.tar.gz"
/// );
/// ```
pub fn download_url(version: &str, platform: Platform, arch: Arch) -> String {
    format!(
        "https://github.com/actions/runner/releases/download/v{version}/actions-runner-{platform}-{arch}-{version}.{ext}",
        version = version,
        platform = platform.as_str(),
        arch = arch.as_str(),
        ext = platform.extension(),
    )
}

// Install scripts embedded at compile time
const INSTALL_SCRIPT_UNIX: &str = include_str!("../scripts/install_runner_unix.sh");
const INSTALL_SCRIPT_WINDOWS: &str = include_str!("../scripts/install_runner_windows.ps1");

/// Generate the install command for a GitHub Actions runner.
///
/// Returns a shell command string ready to be executed via SSH.
/// The embedded scripts are invoked with proper arguments.
///
/// # Arguments
/// * `runner_dir` - Directory where the runner will be installed (e.g., "~/actions-runner" or "C:\\actions-runner")
/// * `version` - The runner version (e.g., "2.321.0")
/// * `platform` - Target platform
/// * `arch` - Target architecture
pub fn install_command(runner_dir: &str, version: &str, platform: Platform, arch: Arch) -> String {
    let url = download_url(version, platform, arch);

    match platform {
        Platform::Windows => {
            // PowerShell: invoke script with named parameters via -Command
            // Strip the param() block since we're inlining values
            let script_body: String = INSTALL_SCRIPT_WINDOWS
                .lines()
                .skip_while(|l| !l.trim().starts_with("$ErrorActionPreference"))
                .collect::<Vec<_>>()
                .join("\n");

            format!(
                "$RunnerDir = {}; $Url = {}; {}",
                escape_powershell(runner_dir),
                escape_powershell(&url),
                script_body.replace('\n', "; ").trim()
            )
        }
        Platform::Linux | Platform::MacOS => {
            // Bash: use heredoc to pass script with arguments
            // Use escape_posix_path for runner_dir to preserve tilde expansion (~/actions-runner)
            format!(
                "bash -s -- {} {} << 'INSTALL_RUNNER_EOF'\n{INSTALL_SCRIPT_UNIX}\nINSTALL_RUNNER_EOF",
                escape_posix_path(runner_dir),
                escape_posix(&url),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_url_linux_x64() {
        let url = download_url("2.321.0", Platform::Linux, Arch::X64);
        assert_eq!(
            url,
            "https://github.com/actions/runner/releases/download/v2.321.0/actions-runner-linux-x64-2.321.0.tar.gz"
        );
    }

    #[test]
    fn test_download_url_linux_arm64() {
        let url = download_url("2.321.0", Platform::Linux, Arch::Arm64);
        assert_eq!(
            url,
            "https://github.com/actions/runner/releases/download/v2.321.0/actions-runner-linux-arm64-2.321.0.tar.gz"
        );
    }

    #[test]
    fn test_download_url_windows_x64() {
        let url = download_url("2.321.0", Platform::Windows, Arch::X64);
        assert_eq!(
            url,
            "https://github.com/actions/runner/releases/download/v2.321.0/actions-runner-win-x64-2.321.0.zip"
        );
    }

    #[test]
    fn test_download_url_macos_arm64() {
        let url = download_url("2.321.0", Platform::MacOS, Arch::Arm64);
        assert_eq!(
            url,
            "https://github.com/actions/runner/releases/download/v2.321.0/actions-runner-osx-arm64-2.321.0.tar.gz"
        );
    }

    #[test]
    fn test_arch_from_uname() {
        assert_eq!(Arch::from_uname("x86_64"), Some(Arch::X64));
        assert_eq!(Arch::from_uname("amd64"), Some(Arch::X64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Arm64));
        assert_eq!(Arch::from_uname("aarch64"), Some(Arch::Arm64));
        assert_eq!(Arch::from_uname("  x86_64\n"), Some(Arch::X64));
        assert_eq!(Arch::from_uname("i386"), None);
    }

    #[test]
    fn test_install_command_unix() {
        let cmd = install_command("~/actions-runner", "2.321.0", Platform::Linux, Arch::X64);
        // Should use heredoc with bash -s
        // Tilde should be unquoted for shell expansion, path portion quoted
        assert!(cmd.contains("bash -s -- ~/'actions-runner'"));
        assert!(cmd.contains("actions-runner-linux-x64-2.321.0.tar.gz"));
        assert!(cmd.contains("INSTALL_RUNNER_EOF"));
        // Script should use $1, $2 for arguments
        assert!(cmd.contains("$1"));
        assert!(cmd.contains("$2"));
    }

    #[test]
    fn test_install_command_unix_absolute_path() {
        let cmd = install_command("/opt/actions-runner", "2.321.0", Platform::Linux, Arch::X64);
        // Absolute paths should be fully quoted
        assert!(cmd.contains("bash -s -- '/opt/actions-runner'"));
    }

    #[test]
    fn test_install_command_windows() {
        let cmd = install_command(
            r"C:\actions-runner",
            "2.321.0",
            Platform::Windows,
            Arch::X64,
        );
        // Should set variables and include script body
        assert!(cmd.contains(r"$RunnerDir = 'C:\actions-runner'"));
        assert!(cmd.contains("actions-runner-win-x64-2.321.0.zip"));
        assert!(cmd.contains("Expand-Archive"));
    }
}
