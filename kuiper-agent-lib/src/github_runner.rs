//! GitHub Actions runner version fetching and download URL construction.
//!
//! This module provides shared functionality for both kuiper-tart-agent and kuiper-proxmox-agent
//! to fetch the latest runner version and construct platform-specific download URLs.

use crate::shell::{encode_powershell_command, escape_posix, escape_posix_path, escape_powershell};
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

/// Script for running the GitHub Actions runner in macOS GUI context via Terminal.app.
pub const GUI_RUNNER_SCRIPT_MACOS: &str = include_str!("../scripts/run_runner_gui_macos.sh");

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

/// Builds the shell commands that install, configure and run the GitHub runner on a VM over SSH.
pub struct RunnerConfigBuilder {
    runner_dir: String,
    platform: Platform,
    /// If Windows, whether the SSH shell is PowerShell (true) or cmd.exe (false)
    shell_is_powershell: bool,
}

impl RunnerConfigBuilder {
    /// Create a new runner config builder for Linux.
    pub fn linux(runner_dir: &str) -> Self {
        Self::new(runner_dir, Platform::Linux, false)
    }

    /// Create a new runner config builder for macOS.
    pub fn macos(runner_dir: &str) -> Self {
        Self::new(runner_dir, Platform::MacOS, false)
    }

    /// Create a new runner config builder for Windows with cmd.exe as the SSH shell.
    pub fn windows(runner_dir: &str) -> Self {
        Self::new(runner_dir, Platform::Windows, false)
    }

    /// Create a new runner config builder for Windows with PowerShell as the SSH shell.
    pub fn windows_powershell(runner_dir: &str) -> Self {
        Self::new(runner_dir, Platform::Windows, true)
    }

    fn new(runner_dir: &str, platform: Platform, shell_is_powershell: bool) -> Self {
        Self {
            runner_dir: runner_dir.to_string(),
            platform,
            shell_is_powershell,
        }
    }

    fn is_windows(&self) -> bool {
        self.platform == Platform::Windows
    }

    /// Wrap a PowerShell command appropriately based on the shell type.
    /// If the shell is already PowerShell, returns the command as-is.
    /// If the shell is cmd.exe, uses `powershell -EncodedCommand <base64>` to avoid
    /// cmd.exe parsing issues with quotes and special characters.
    fn wrap_powershell(&self, ps_command: &str) -> String {
        if self.shell_is_powershell {
            ps_command.to_string()
        } else {
            // Use -EncodedCommand with UTF-16LE Base64 encoding to completely avoid
            // cmd.exe metacharacter interpretation. This prevents injection attacks
            // where embedded " characters could break out of quoted strings.
            let encoded = encode_powershell_command(ps_command);
            format!("powershell -EncodedCommand {encoded}")
        }
    }

    /// Build the runner configuration command.
    pub fn config_command(&self, url: &str, token: &str, labels: &[String], name: &str) -> String {
        let labels_str = labels.join(",");

        if self.is_windows() {
            let ps_cmd = format!(
                "Set-Location {}; .\\config.cmd --url {} --token {} --labels {} --name {} --ephemeral --unattended",
                escape_powershell(&self.runner_dir),
                escape_powershell(url),
                escape_powershell(token),
                escape_powershell(&labels_str),
                escape_powershell(name),
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            format!(
                "cd {} && ./config.sh --url {} --token {} --labels {} --name {} --ephemeral --unattended",
                escape_posix_path(&self.runner_dir),
                escape_posix(url),
                escape_posix(token),
                escape_posix(&labels_str),
                escape_posix(name),
            )
        }
    }

    /// Build the runner command for direct (blocking) execution.
    /// This runs the runner and waits for it to complete - simpler for ephemeral runners.
    pub fn run_command_direct(&self) -> String {
        if self.is_windows() {
            // Run directly - will block until runner completes
            let ps_cmd = format!(
                "Set-Location {}; .\\run.cmd",
                escape_powershell(&self.runner_dir),
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            // Use a login shell to ensure PATH includes user-installed tools
            format!(
                "bash -l -c 'cd {} && ./run.sh'",
                escape_posix_path(&self.runner_dir)
            )
        }
    }

    fn jit_config_path(&self) -> String {
        if self.is_windows() {
            format!("{}\\jit-config.txt", self.runner_dir)
        } else {
            format!("{}/jit-config.txt", self.runner_dir)
        }
    }

    /// Build a command to write the JIT config blob to a file on the VM.
    /// Uses a file to avoid shell escaping and command-line length issues.
    pub fn write_jit_config_command(&self, jit_config: &str) -> String {
        let jit_path = self.jit_config_path();

        if self.is_windows() {
            let ps_cmd = format!(
                "Set-Content -Path {} -Value {} -NoNewline",
                escape_powershell(&jit_path),
                escape_powershell(jit_config),
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            format!(
                "cat > {} << 'JIT_CONFIG_EOF'\n{}\nJIT_CONFIG_EOF",
                escape_posix_path(&jit_path),
                jit_config,
            )
        }
    }

    /// Build the runner command for JIT execution, reading the config from a file.
    /// The JIT config must have been written to jit-config.txt first via `write_jit_config_command`.
    pub fn run_command_jit_from_file(&self) -> String {
        if self.is_windows() {
            let jit_path = self.jit_config_path();
            let ps_cmd = format!(
                "Set-Location {}; $jit = Get-Content {} -Raw; Remove-Item {}; .\\run.cmd --jitconfig $jit",
                escape_powershell(&self.runner_dir),
                escape_powershell(&jit_path),
                escape_powershell(&jit_path),
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            format!(
                "bash -l -c 'cd {} && jit=$(cat jit-config.txt) && rm -f jit-config.txt && ./run.sh --jitconfig \"$jit\"'",
                escape_posix_path(&self.runner_dir),
            )
        }
    }

    /// Build command to read the last N lines of the most recent runner diagnostic log.
    pub fn diag_log_command(&self, tail_lines: u32) -> String {
        if self.is_windows() {
            let ps_cmd = format!(
                "Get-ChildItem {}\\_diag\\*.log -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1 | ForEach-Object {{ Get-Content $_.FullName -Tail {} }}",
                escape_powershell(&self.runner_dir),
                tail_lines,
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            format!(
                "f=$(ls -t {}/_diag/*.log 2>/dev/null | head -1); if [ -n \"$f\" ]; then tail -{} \"$f\"; else echo 'no diag logs'; fi",
                escape_posix_path(&self.runner_dir),
                tail_lines,
            )
        }
    }

    /// Build command to check if runner is installed. Prints `installed` if it is.
    pub fn check_installed_command(&self) -> String {
        if self.is_windows() {
            let escaped_dir = escape_powershell(&self.runner_dir);
            let ps_cmd = format!(
                "if (Test-Path (Join-Path {escaped_dir} 'run.cmd')) {{ Write-Output 'installed' }}",
            );
            self.wrap_powershell(&ps_cmd)
        } else {
            let escaped_dir = escape_posix_path(&self.runner_dir);
            format!("test -d {escaped_dir} && test -f {escaped_dir}/run.sh && echo 'installed'",)
        }
    }

    /// Build command to download and install the runner.
    ///
    /// # Arguments
    /// * `version` - The runner version (e.g., "2.321.0")
    /// * `arch` - The target architecture
    pub fn install_command(&self, version: &str, arch: Arch) -> String {
        let cmd = install_command(&self.runner_dir, version, self.platform, arch);

        if self.is_windows() {
            // Wrap with powershell invocation if SSH shell is cmd.exe
            self.wrap_powershell(&cmd)
        } else {
            cmd
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

    #[test]
    fn test_linux_config_command() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "linux".to_string()],
            "runner-001",
        );

        assert!(cmd.contains("./config.sh"));
        assert!(cmd.contains("--ephemeral"));
        assert!(cmd.contains("--unattended"));
        assert!(cmd.contains("'self-hosted,linux'"));
        // Values should be single-quoted
        assert!(cmd.contains("'https://github.com/org/repo'"));
        assert!(cmd.contains("'TOKEN123'"));
        assert!(cmd.contains("'runner-001'"));
    }

    #[test]
    fn test_linux_config_command_escapes_single_quotes() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN'injection",
            &["self-hosted".to_string()],
            "runner-001",
        );

        // Single quote in token must be escaped
        assert!(cmd.contains("'TOKEN'\\''injection'"));
    }

    #[test]
    fn test_linux_config_command_escapes_shell_metacharacters() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "$(rm -rf /)".to_string()],
            "runner-001",
        );

        // Shell metacharacters in labels must be safely quoted
        assert!(cmd.contains("'self-hosted,$(rm -rf /)'"));
    }

    #[test]
    fn test_linux_tilde_expansion_preserved() {
        // Tilde should not be quoted so shell can expand it to home directory
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string()],
            "runner-001",
        );

        // Should have ~/... not '~/...' - tilde must be unquoted for expansion
        assert!(cmd.contains("cd ~/'actions-runner'"));
        // But NOT fully quoted which would prevent tilde expansion
        assert!(!cmd.contains("'~/actions-runner'"));
    }

    #[test]
    fn test_linux_absolute_path_fully_escaped() {
        // Absolute paths should be fully escaped (no tilde expansion needed)
        let builder = RunnerConfigBuilder::linux("/opt/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string()],
            "runner-001",
        );

        assert!(cmd.contains("cd '/opt/actions-runner'"));
    }

    #[test]
    fn test_windows_config_command() {
        // Test with cmd.exe as SSH shell (wraps with powershell -EncodedCommand)
        let builder = RunnerConfigBuilder::windows(r"C:\actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "windows".to_string()],
            "runner-001",
        );

        // Should use -EncodedCommand for safe cmd.exe invocation
        assert!(cmd.starts_with("powershell -EncodedCommand "));
        // The encoded command should be valid base64 (alphanumeric + /+=)
        let encoded = cmd.strip_prefix("powershell -EncodedCommand ").unwrap();
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
    }

    #[test]
    fn test_windows_config_command_escapes_dangerous_chars() {
        // Test that dangerous characters in inputs don't break cmd.exe parsing
        let builder = RunnerConfigBuilder::windows(r"C:\actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            r#"TOKEN"&calc&""#, // Would break cmd.exe if not properly encoded
            &["self-hosted".to_string()],
            "runner-001",
        );

        // With -EncodedCommand, the entire payload is base64, so no cmd.exe metacharacters
        assert!(cmd.starts_with("powershell -EncodedCommand "));
        let encoded = cmd.strip_prefix("powershell -EncodedCommand ").unwrap();
        // Base64 contains no cmd.exe special chars like &, |, ", etc.
        assert!(!encoded.contains('&'));
        assert!(!encoded.contains('"'));
        assert!(!encoded.contains('|'));
    }

    #[test]
    fn test_windows_powershell_config_command() {
        // Test with PowerShell as SSH shell (no wrapper needed)
        let builder = RunnerConfigBuilder::windows_powershell(r"C:\actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "windows".to_string()],
            "runner-001",
        );

        // Should NOT use -EncodedCommand since shell is already PowerShell
        assert!(!cmd.starts_with("powershell"));
        assert!(cmd.contains("Set-Location"));
        assert!(cmd.contains("config.cmd"));
        assert!(cmd.contains("--ephemeral"));
        assert!(cmd.contains("--unattended"));
    }

    #[test]
    fn test_windows_powershell_escapes_single_quotes() {
        // When shell is already PowerShell, single quotes are escaped by doubling
        let builder = RunnerConfigBuilder::windows_powershell(r"C:\actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN'injection",
            &["self-hosted".to_string()],
            "runner-001",
        );

        // PowerShell escapes single quotes by doubling them
        assert!(cmd.contains("'TOKEN''injection'"));
    }

    #[test]
    fn test_linux_write_jit_config() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.write_jit_config_command("ENCODED_JIT_BLOB_123");
        assert!(cmd.contains("jit-config.txt"));
        assert!(cmd.contains("ENCODED_JIT_BLOB_123"));
    }

    #[test]
    fn test_linux_run_command_jit_from_file() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.run_command_jit_from_file();
        assert!(cmd.contains("./run.sh --jitconfig"));
        assert!(cmd.contains("jit-config.txt"));
        assert!(!cmd.contains("config.sh"));
    }

    #[test]
    fn test_windows_write_jit_config() {
        let builder = RunnerConfigBuilder::windows(r"C:\actions-runner");
        let cmd = builder.write_jit_config_command("ENCODED_JIT_BLOB_123");
        assert!(cmd.starts_with("powershell -EncodedCommand"));
    }

    #[test]
    fn test_windows_run_command_jit_from_file() {
        let builder = RunnerConfigBuilder::windows(r"C:\actions-runner");
        let cmd = builder.run_command_jit_from_file();
        assert!(cmd.starts_with("powershell -EncodedCommand"));
    }

    #[test]
    fn test_windows_powershell_run_command_jit_from_file() {
        let builder = RunnerConfigBuilder::windows_powershell(r"C:\actions-runner");
        let cmd = builder.run_command_jit_from_file();
        assert!(!cmd.starts_with("powershell"));
        assert!(cmd.contains("run.cmd --jitconfig"));
        assert!(cmd.contains("jit-config.txt"));
    }

    #[test]
    fn test_macos_install_uses_osx_archive() {
        let builder = RunnerConfigBuilder::macos("~/actions-runner");
        let cmd = builder.install_command("2.321.0", Arch::Arm64);
        assert!(cmd.contains("actions-runner-osx-arm64-2.321.0.tar.gz"));
        assert!(cmd.starts_with("bash -s --"));
    }
}
