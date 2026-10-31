//! SSH client for configuring GitHub runners on VMs, over the shared transport in
//! [`kuiper_agent_lib::ssh`].
//!
//! Each command opens its own connection, so a VM that reboots or drops a session mid-job
//! only fails the one poll.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use kuiper_agent_lib::forgejo_runner;
use kuiper_agent_lib::github_runner::{Arch, RunnerConfigBuilder};
use kuiper_agent_lib::runner_cache::RunnerCache;
use kuiper_agent_lib::ssh::{CommandOutput, SshClient, SshSession};
use kuiper_agent_lib::vm_agent::{JobContext, step};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info};

use crate::backend::GuestOs;
use crate::error::{Error, Result};

/// SSH authentication method.
#[derive(Debug, Clone, Default)]
pub enum SshAuth {
    /// Password-based authentication
    Password(String),
    /// Key-based authentication with path to private key file
    KeyFile(PathBuf),
    /// No explicit auth - will try default keys
    #[default]
    None,
}

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// SSH username (typically "admin" for Tart VMs)
    pub username: String,
    /// Authentication method
    pub auth: SshAuth,
    /// Connection timeout
    pub timeout: Duration,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            auth: SshAuth::None,
            timeout: Duration::from_secs(30),
        }
    }
}

impl SshConfig {
    /// Create a new SSH config with password authentication.
    #[cfg(test)]
    pub fn with_password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            auth: SshAuth::Password(password.into()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create a new SSH config with key file authentication.
    #[cfg(test)]
    pub fn with_key_file(username: impl Into<String>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            username: username.into(),
            auth: SshAuth::KeyFile(key_path.into()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create from a configuration struct.
    pub fn from_config(config: &crate::config::SshAuthConfig) -> Self {
        let auth = match config.auth_method.as_str() {
            "password" => config
                .password
                .as_ref()
                .map(|p| SshAuth::Password(p.clone()))
                .unwrap_or(SshAuth::None),
            "key" => config
                .private_key
                .as_ref()
                .map(|p| SshAuth::KeyFile(p.clone()))
                .unwrap_or(SshAuth::None),
            _ => SshAuth::None, // "default" or any other value - try default keys
        };

        Self {
            username: config.username.clone(),
            auth,
            timeout: Duration::from_secs(config.timeout_secs),
        }
    }

    /// The SSH client for this config. Keys are read here, so a key fixed on disk is picked up
    /// by the next command.
    fn client(&self) -> Result<SshClient> {
        let auth = match &self.auth {
            SshAuth::Password(password) => {
                kuiper_agent_lib::ssh::SshAuth::Password(password.clone())
            }
            SshAuth::KeyFile(path) => kuiper_agent_lib::ssh::SshAuth::key_file(path)?,
            SshAuth::None => kuiper_agent_lib::ssh::SshAuth::default_key()?,
        };
        Ok(SshClient::new(&self.username, auth).with_timeout(self.timeout))
    }
}

/// Wait for SSH port to become available.
pub async fn wait_for_ssh(ip: Ipv4Addr, timeout: Duration) -> Result<()> {
    kuiper_agent_lib::ssh::wait_for_port(SocketAddr::new(ip.into(), 22), timeout).await?;
    Ok(())
}

async fn connect(ip: Ipv4Addr, config: &SshConfig) -> Result<SshSession> {
    Ok(config.client()?.connect(ip.into()).await?)
}

/// stdout of a command that exited 0, else [`Error::SshCommand`].
fn stdout(output: CommandOutput) -> Result<String> {
    Ok(output.into_stdout()?)
}

/// Execute a command on a remote host via SSH.
pub async fn ssh_exec(ip: Ipv4Addr, config: &SshConfig, command: &str) -> Result<String> {
    stdout(connect(ip, config).await?.execute(command).await?)
}

/// Execute a command that carries a secret (a runner token, a JIT config), logging only
/// `what` it is instead of the command itself.
pub async fn ssh_exec_secret(
    ip: Ipv4Addr,
    config: &SshConfig,
    command: &str,
    what: &str,
) -> Result<String> {
    stdout(
        connect(ip, config)
            .await?
            .execute_secret(command, what)
            .await?,
    )
}

/// Log a failed command's stderr locally, it's kept out of the error message.
fn log_command_stderr(e: &Error) {
    if let Error::SshCommand { stderr, .. } = e
        && !stderr.trim().is_empty()
    {
        error!(
            local_only = tracing::field::Empty,
            "stderr: {}",
            stderr.trim()
        );
    }
}

/// GitHub runner commands for a VM. The runner lives in `~/actions-runner`.
fn runner_commands(os: GuestOs) -> RunnerConfigBuilder {
    match os {
        GuestOs::MacOS => RunnerConfigBuilder::macos("~/actions-runner"),
        GuestOs::Linux => RunnerConfigBuilder::linux("~/actions-runner"),
    }
}

/// Ensure the GitHub Actions runner is installed on the VM.
/// Installs the runner if not present, from `cache` or else having the VM download it, timed as
/// its own step in `ctx`. If `runner_version` is empty or "latest", uses the latest version.
pub async fn ensure_runner_installed(
    ip: Ipv4Addr,
    config: &SshConfig,
    runner_version: &str,
    cache: &RunnerCache,
    os: GuestOs,
    ctx: &JobContext,
) -> Result<()> {
    let commands = runner_commands(os);

    // Check if runner is already installed
    match ssh_exec(ip, config, &commands.check_installed_command()).await {
        Ok(output) if output.trim() == "installed" => {
            debug!("GitHub Actions runner already installed on {}", ip);
            ctx.skip_step(step::INSTALL);
            return Ok(());
        }
        _ => {
            info!("GitHub Actions runner not found on {}, installing...", ip);
        }
    }
    ctx.step(step::INSTALL);

    let version = cache.github_version(runner_version).await;

    // Detect architecture
    let arch_output = ssh_exec(ip, config, "uname -m").await?;
    let arch = Arch::from_uname(&arch_output).ok_or_else(|| {
        Error::Ssh(format!(
            "Unsupported architecture: {}. Expected arm64 or x86_64.",
            arch_output.trim()
        ))
    })?;

    info!(
        "Installing GitHub Actions runner v{} for {:?}-{} on {}",
        version,
        os,
        arch.as_str(),
        ip
    );

    let mut session = connect(ip, config).await?;
    if cache
        .install_github(&mut session, &commands, &version, arch)
        .await
    {
        ctx.step(step::SETUP);
        return Ok(());
    }

    let sha256 = cache
        .github_checksum(&version, commands.platform(), arch)
        .await;
    let install = commands.install_command(&version, arch, sha256.as_deref());
    match ssh_exec(ip, config, &install).await {
        Ok(output) => {
            info!("GitHub Actions runner installed on {}", ip);
            debug!(
                local_only = tracing::field::Empty,
                "Install output: {}", output
            );
            ctx.step(step::SETUP);
            Ok(())
        }
        Err(e) => {
            error!("Failed to install GitHub Actions runner on {}: {}", ip, e);
            log_command_stderr(&e);
            Err(e)
        }
    }
}

/// Configure the GitHub Actions runner on a VM.
/// Automatically installs the runner if not present.
#[allow(clippy::too_many_arguments)]
pub async fn configure_runner(
    ip: Ipv4Addr,
    config: &SshConfig,
    registration_token: &str,
    labels: &[String],
    runner_scope_url: &str,
    runner_name: &str,
    runner_version: &str,
    cache: &RunnerCache,
    os: GuestOs,
    ctx: &JobContext,
) -> Result<()> {
    let labels_str = labels.join(",");

    // Validate inputs
    if registration_token.is_empty() {
        return Err(Error::Ssh(
            "GitHub runner registration token is empty - cannot configure runner".to_string(),
        ));
    }

    if runner_scope_url.is_empty() {
        return Err(Error::Ssh(
            "Runner scope URL is empty - cannot configure runner".to_string(),
        ));
    }

    // Ensure runner is installed first
    ensure_runner_installed(ip, config, runner_version, cache, os, ctx).await?;

    info!(
        "Configuring runner {} with labels: {} (scope: {})",
        runner_name, labels_str, runner_scope_url
    );

    // Configure the runner
    let config_cmd = runner_commands(os).config_command(
        runner_scope_url,
        registration_token,
        labels,
        runner_name,
    );

    match ssh_exec_secret(ip, config, &config_cmd, "config.sh (registration token)").await {
        Ok(output) => {
            debug!(
                local_only = tracing::field::Empty,
                "Runner config output: {}", output
            );
            info!("Runner {} configured successfully", runner_name);
            Ok(())
        }
        Err(e) => {
            error!(
                "Failed to configure runner {} on {}: {}",
                runner_name, ip, e
            );
            log_command_stderr(&e);
            error!(
                "This usually means the GitHub registration token is invalid or expired. \
                 Check that the coordinator has valid GitHub App credentials."
            );
            Err(e)
        }
    }
}

/// Wrapper script for running the GitHub Actions runner. It's plain bash, so linux runs the same script headless.
const RUNNER_SCRIPT: &str = kuiper_agent_lib::github_runner::GUI_RUNNER_SCRIPT_MACOS;

/// Which runner program a VM runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerProgram {
    /// The GitHub Actions runner (`run.sh`)
    GitHub,
    /// `forgejo-runner one-job`, prepared by [`prepare_forgejo_runner`]
    Forgejo,
}

/// Install forgejo-runner on the VM if needed and write its token file and `run-job.sh`
/// for one Forgejo job attempt.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_forgejo_runner(
    ip: Ipv4Addr,
    config: &SshConfig,
    job: &forgejo_runner::OneJob<'_>,
    version: &str,
    url_template: &str,
    cache: &RunnerCache,
    os: GuestOs,
    ctx: &JobContext,
) -> Result<()> {
    match ssh_exec(ip, config, &forgejo_runner::check_installed_command()).await {
        Ok(output) if output.trim() == "installed" => {
            debug!("forgejo-runner already installed on {}", ip);
            ctx.skip_step(step::INSTALL);
        }
        _ => {
            ctx.step(step::INSTALL);
            let version = forgejo_runner::resolve_version(version);
            let arch_output = ssh_exec(ip, config, "uname -m").await?;
            let arch = Arch::from_uname(&arch_output).ok_or_else(|| {
                Error::Ssh(format!(
                    "Unsupported architecture: {}. Expected arm64 or x86_64.",
                    arch_output.trim()
                ))
            })?;
            let fr_os = match os {
                GuestOs::MacOS => forgejo_runner::Os::MacOS,
                GuestOs::Linux => forgejo_runner::Os::Linux,
            };
            let url = forgejo_runner::download_url(url_template, version, fr_os, arch);
            info!(
                "Installing forgejo-runner v{} for {}-{} on {} from {}",
                version,
                fr_os.as_str(),
                arch.as_str(),
                ip,
                url
            );
            let mut session = connect(ip, config).await?;
            let cached = cache
                .install_forgejo(&mut session, &url, version, fr_os, arch)
                .await;
            if !cached {
                match ssh_exec(
                    ip,
                    config,
                    &forgejo_runner::install_command(&url, version, fr_os, arch),
                )
                .await
                {
                    Ok(output) => {
                        info!("forgejo-runner installed on {}", ip);
                        debug!(
                            local_only = tracing::field::Empty,
                            "Install output: {}", output
                        );
                    }
                    Err(e) => {
                        error!("Failed to install forgejo-runner on {}: {}", ip, e);
                        log_command_stderr(&e);
                        return Err(e);
                    }
                }
            }
            ctx.step(step::SETUP);
        }
    }

    let prepare = forgejo_runner::prepare_command(job).map_err(Error::Ssh)?;
    ssh_exec_secret(
        ip,
        config,
        &prepare,
        "write forgejo-runner job files (runner token)",
    )
    .await
    .map_err(|e| {
        error!("Failed to write forgejo-runner job files on {}: {}", ip, e);
        log_command_stderr(&e);
        e
    })?;
    info!(
        "forgejo-runner ready on {} for handle {} (labels {:?})",
        ip, job.handle, job.labels
    );
    Ok(())
}

/// Start the GitHub Actions runner in the background and wait for it to complete.
///
/// On macOS this launches the runner via `open -a Terminal`, which provides access to
/// macOS GUI services (code signing, keychain, notarization, etc.) that don't
/// work in a headless SSH session. On linux it's a detached process.
///
/// For ephemeral runners, the process exits when the job completes.
/// Returns Ok if the runner exited normally (exit code 0), Err otherwise.
///
/// All runner output is captured to the specified log file by polling.
pub async fn start_runner_and_wait(
    ip: Ipv4Addr,
    config: &SshConfig,
    log_path: &Path,
    jit_config: &str,
    os: GuestOs,
    program: RunnerProgram,
) -> Result<()> {
    info!(
        "Starting {:?} runner ({:?}) on {} (logging to {})",
        program,
        os,
        ip,
        log_path.display()
    );

    // Upload the runner wrapper script to the VM
    let script_path = "~/start-runner.sh";
    let script = match program {
        RunnerProgram::GitHub => RUNNER_SCRIPT,
        RunnerProgram::Forgejo => forgejo_runner::JOB_WRAPPER_SCRIPT,
    };
    let upload_cmd = format!(
        "cat > {script_path} << 'RUNNER_SCRIPT_EOF'\n{script}\nRUNNER_SCRIPT_EOF\nchmod +x {script_path}"
    );
    ssh_exec(ip, config, &upload_cmd)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to upload runner script: {e}")))?;
    debug!("Uploaded runner script to {}", script_path);

    // If JIT config is provided, write it to a file on the VM.
    // The runner script checks for this file and passes --jitconfig to run.sh.
    if !jit_config.is_empty() {
        info!("Writing JIT config to VM {} (skipping config.sh)", ip);
        let write_jit_cmd =
            format!("cat > ~/jit-config.txt << 'JIT_CONFIG_EOF'\n{jit_config}\nJIT_CONFIG_EOF");
        ssh_exec_secret(ip, config, &write_jit_cmd, "write JIT config")
            .await
            .map_err(|e| Error::Ssh(format!("Failed to write JIT config: {e}")))?;
    }

    // Clean up any previous signal file
    let _ = ssh_exec(ip, config, "rm -f ~/runner-exit-status").await;

    let launch_cmd = match os {
        GuestOs::MacOS => format!("open -a Terminal {script_path}"),

        // login shell for the user's PATH. setsid + nohup + no open fds so it outlives this SSH session and the
        // exec returns right away
        GuestOs::Linux => {
            format!("setsid nohup bash -l {script_path} > /dev/null 2>&1 < /dev/null &")
        }
    };
    ssh_exec(ip, config, &launch_cmd)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to launch runner: {e}")))?;
    info!("Runner launched on {}", ip);

    // Open local log file for writing
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to open log file: {e}")))?;

    // Write header
    let header = format!(
        "\n=== Runner started at {} ===\n\n",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
    );
    log_file
        .write_all(header.as_bytes())
        .await
        .map_err(|e| Error::Ssh(format!("Failed to write to log file: {e}")))?;

    // Poll for completion with 6-hour timeout (for long-running jobs)
    let timeout = Duration::from_secs(6 * 3600);
    let deadline = tokio::time::Instant::now() + timeout;
    let poll_interval = Duration::from_secs(5);
    let mut last_log_size: u64 = 0;

    loop {
        // Check for timeout
        if tokio::time::Instant::now() > deadline {
            return Err(Error::Timeout("waiting for runner completion (6 hours)"));
        }

        // Try to read new log content and append to local log
        // Use wc -c to get file size, then tail if there's new content
        last_log_size = poll_and_stream_log(ip, config, &mut log_file, last_log_size).await?;

        // Check if runner completed by looking for exit status file
        if let Ok(exit_content) = ssh_exec(ip, config, "cat ~/runner-exit-status 2>/dev/null").await
        {
            let exit_content = exit_content.trim();
            if !exit_content.is_empty() {
                // Runner completed - parse exit code
                let exit_code: i32 = exit_content.parse().unwrap_or(-1);

                // Final log flush - read any remaining content
                let _ = poll_and_stream_log(ip, config, &mut log_file, last_log_size).await;

                // Write footer
                let footer = format!(
                    "\n=== Runner exited at {} with code {} ===\n",
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    exit_code
                );
                log_file
                    .write_all(footer.as_bytes())
                    .await
                    .map_err(|e| Error::Ssh(format!("Failed to write to log: {e}")))?;
                log_file
                    .flush()
                    .await
                    .map_err(|e| Error::Ssh(format!("Failed to flush log: {e}")))?;

                if exit_code == 0 {
                    info!("Runner completed successfully on {}", ip);
                    return Ok(());
                } else {
                    error!("Runner failed on {} with exit code {}", ip, exit_code);
                    return Err(Error::Ssh(format!(
                        "Runner failed (exit code: {}). See log: {}",
                        exit_code,
                        log_path.display()
                    )));
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Poll the remote log file and stream any new content to the local log file.
/// Returns the new log size after polling.
async fn poll_and_stream_log(
    ip: Ipv4Addr,
    config: &SshConfig,
    log_file: &mut tokio::fs::File,
    last_log_size: u64,
) -> Result<u64> {
    // size and new bytes from one snapshot, the runner can write between two separate calls
    let from = last_log_size + 1;
    let cmd = format!(
        "n=$(( $(wc -c < ~/runner.log 2>/dev/null || echo 0) )); echo $n; \
         if [ $n -gt {last_log_size} ]; then tail -c +{from} ~/runner.log | head -c $((n - {last_log_size})); fi"
    );
    let output = match ssh_exec(ip, config, &cmd).await {
        Ok(output) => output,
        Err(_) => return Ok(last_log_size),
    };
    let (size_str, new_content) = output.split_once('\n').unwrap_or((&output, ""));
    let current_size: u64 = match size_str.trim().parse() {
        Ok(s) => s,
        Err(_) => return Ok(last_log_size),
    };

    if current_size <= last_log_size {
        return Ok(last_log_size);
    }

    if !new_content.is_empty() {
        log_file
            .write_all(new_content.as_bytes())
            .await
            .map_err(|e| Error::Ssh(format!("Failed to write to log: {e}")))?;

        // Log to tracing for real-time visibility
        for line in new_content.lines() {
            debug!(local_only = tracing::field::Empty, "[runner] {}", line);
        }
    }

    Ok(current_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_config_default() {
        let config = SshConfig::default();
        assert_eq!(config.username, "admin");
        assert!(matches!(config.auth, SshAuth::None));
        assert_eq!(config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_ssh_config_with_password() {
        let config = SshConfig::with_password("user", "pass123");
        assert_eq!(config.username, "user");
        assert!(matches!(config.auth, SshAuth::Password(ref p) if p == "pass123"));
    }

    #[test]
    fn test_ssh_config_with_key_file() {
        let config = SshConfig::with_key_file("admin", "/path/to/key");
        assert_eq!(config.username, "admin");
        assert!(
            matches!(config.auth, SshAuth::KeyFile(ref p) if p == &PathBuf::from("/path/to/key"))
        );
    }
}
