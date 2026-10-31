//! Running one job's runner on a VM over SSH: detect the guest's shell, install and set up the
//! GitHub runner or forgejo-runner, then run it in the foreground until the job is done.
//!
//! [`Guest::prepare`] and [`Guest::run`] are separate so the agent can track the VM in between
//! (say, mark it as running a job) and wrap `run` to give up when the VM is torn down.
//!
//! Linux, macOS and Windows guests work, Windows over a cmd.exe or PowerShell SSH shell. Agents
//! whose guests need the runner launched another way (tart's macOS GUI session) use the
//! building blocks in [`crate::github_runner`] and [`crate::forgejo_runner`] directly.

use std::time::{Duration, Instant};

use kuiper_agent_proto::ForgejoRunner;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::forgejo_runner;
use crate::github_runner::{Arch, RunnerConfigBuilder};
use crate::runner_cache::RunnerCache;
use crate::ssh::{CommandOutput, SshError, SshSession};
use crate::vm_agent::{JobContext, step};

/// A runner setup or run failure.
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Ssh(#[from] SshError),

    /// The runner failed to install, configure or run
    #[error("{0}")]
    Runner(String),
}

type Result<T, E = RunnerError> = std::result::Result<T, E>;

/// The guest OS and its SSH shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestShell {
    /// Linux, or another non-macOS Unix
    Linux,
    MacOS,
    /// Windows, SSH shell is cmd.exe
    WindowsCmd,
    /// Windows, SSH shell is PowerShell
    WindowsPowerShell,
}

impl GuestShell {
    pub fn is_windows(self) -> bool {
        matches!(self, GuestShell::WindowsCmd | GuestShell::WindowsPowerShell)
    }

    fn describe(self) -> &'static str {
        match self {
            GuestShell::Linux => "Linux (bash/sh)",
            GuestShell::MacOS => "macOS (bash/sh)",
            GuestShell::WindowsCmd => "Windows (cmd.exe)",
            GuestShell::WindowsPowerShell => "Windows (PowerShell)",
        }
    }
}

/// What to run for one job, from the coordinator's create command.
pub struct Job<'a> {
    /// Legacy registration token, for `config.sh` when there's no JIT config
    pub registration_token: &'a str,
    pub labels: &'a [String],
    pub runner_scope_url: &'a str,
    pub runner_name: &'a str,
    /// GitHub JIT config; when set, `config.sh` is skipped
    pub jit_config: &'a str,
    /// Set for Forgejo jobs: run `forgejo-runner one-job` instead of the GitHub runner
    pub forgejo: Option<&'a ForgejoRunner>,
}

/// Which runner versions to install, from the agent's config, and the agent's copies of them.
pub struct RunnerVersions<'a> {
    /// GitHub runner version, or empty/"latest"
    pub github: &'a str,
    /// forgejo-runner version, or "latest"
    pub forgejo: &'a str,
    /// forgejo-runner download URL template
    pub forgejo_url: &'a str,
    pub cache: &'a RunnerCache,
}

/// A runner that's installed and set up for the job, ready for [`Guest::run`].
pub struct PreparedRunner {
    kind: Prepared,
    command: String,
}

enum Prepared {
    GitHub {
        builder: RunnerConfigBuilder,
        jit: bool,
    },
    Forgejo {
        handle: String,
    },
}

/// A VM reached over SSH.
pub struct Guest<'s> {
    session: &'s mut SshSession,
    shell: GuestShell,
    /// For log messages, e.g. "VM 120"
    vm: String,
}

impl<'s> Guest<'s> {
    /// Detect the guest's shell over `session`. `vm` names the VM in log messages.
    pub async fn detect(session: &'s mut SshSession, vm: impl Into<String>) -> Result<Self> {
        let vm = vm.into();
        let shell = detect_shell(session).await?;
        info!("{} detected as {}", vm, shell.describe());
        Ok(Self { session, shell, vm })
    }

    pub fn shell(&self) -> GuestShell {
        self.shell
    }

    /// Force a time sync on Windows. Cloned VMs often have stale clocks, which makes JIT
    /// token validation fail ("token is not valid until ..."). Failures are only logged.
    pub async fn sync_clock(&mut self) {
        let cmd = match self.shell {
            GuestShell::Linux | GuestShell::MacOS => return,
            GuestShell::WindowsPowerShell => "net start w32time; w32tm /resync /force",
            GuestShell::WindowsCmd => {
                "powershell -Command \"net start w32time; w32tm /resync /force\""
            }
        };
        info!("Syncing Windows clock on {}", self.vm);
        match self.session.execute(cmd).await {
            Ok(output) if output.exit_code == 0 => {
                debug!("Time sync output ({}): {}", self.vm, output.stdout.trim());
            }
            Ok(output) => {
                warn!(
                    "Time sync failed on {} (exit {}): {}",
                    self.vm,
                    output.exit_code,
                    output.stderr.trim()
                );
            }
            Err(e) => warn!("Time sync command failed on {}: {}", self.vm, e),
        }
    }

    /// Install the job's runner if needed and set it up: the JIT config file or `config.sh`
    /// for GitHub, the token file and job script for Forgejo. The download is timed as its own step in `ctx`.
    pub async fn prepare(
        &mut self,
        job: &Job<'_>,
        versions: &RunnerVersions<'_>,
        ctx: &JobContext,
    ) -> Result<PreparedRunner> {
        match job.forgejo {
            Some(forgejo) => {
                self.prepare_forgejo(forgejo, job.labels, versions, ctx)
                    .await
            }
            None => self.prepare_github(job, versions, ctx).await,
        }
    }

    /// Run the prepared runner in the foreground until it exits. Errors if it exits non-zero.
    pub async fn run(&mut self, runner: PreparedRunner) -> Result<()> {
        match runner.kind {
            Prepared::GitHub { builder, jit } => {
                self.run_github(&builder, &runner.command, jit).await
            }
            Prepared::Forgejo { handle } => self.run_forgejo(&runner.command, &handle).await,
        }
    }

    fn runner_builder(&self) -> RunnerConfigBuilder {
        match self.shell {
            GuestShell::Linux => RunnerConfigBuilder::linux("~/actions-runner"),
            GuestShell::MacOS => RunnerConfigBuilder::macos("~/actions-runner"),
            GuestShell::WindowsCmd => RunnerConfigBuilder::windows(r"C:\actions-runner"),
            GuestShell::WindowsPowerShell => {
                RunnerConfigBuilder::windows_powershell(r"C:\actions-runner")
            }
        }
    }

    async fn prepare_github(
        &mut self,
        job: &Job<'_>,
        versions: &RunnerVersions<'_>,
        ctx: &JobContext,
    ) -> Result<PreparedRunner> {
        let vm = &self.vm;
        let builder = self.runner_builder();

        let check = self
            .session
            .execute(&builder.check_installed_command())
            .await?;
        if check.stdout.trim() != "installed" {
            ctx.step(step::INSTALL);
            let version = versions.cache.github_version(versions.github).await;

            // Windows is x64 only for now; elsewhere ask uname
            let arch = if self.shell.is_windows() {
                Arch::X64
            } else {
                let uname = self.session.execute("uname -m").await?;
                Arch::from_uname(&uname.stdout).unwrap_or(Arch::X64)
            };

            info!(
                "GitHub Actions runner not installed on {}, installing v{} ({})...",
                vm,
                version,
                arch.as_str()
            );
            let cached = versions
                .cache
                .install_github(self.session, &builder, &version, arch)
                .await;
            if !cached {
                let sha256 = versions
                    .cache
                    .github_checksum(&version, builder.platform(), arch)
                    .await;
                let install = self
                    .session
                    .execute(&builder.install_command(&version, arch, sha256.as_deref()))
                    .await?;
                if install.exit_code != 0 {
                    error!(
                        local_only = tracing::field::Empty,
                        "Runner installation failed: stdout={}, stderr={}",
                        install.stdout,
                        install.stderr
                    );
                    return Err(RunnerError::Runner(format!(
                        "Failed to install runner (exit code {})",
                        install.exit_code
                    )));
                }
                info!(
                    local_only = tracing::field::Empty,
                    "Runner installed successfully: {}",
                    install.stdout.trim()
                );
            }
            ctx.step(step::SETUP);
        } else {
            debug!("GitHub Actions runner already installed on {}", vm);
            ctx.skip_step(step::INSTALL);
        }

        // JIT: skip config.sh and run with --jitconfig. Legacy: config.sh, then run.sh
        let jit = !job.jit_config.is_empty();
        let command = if jit {
            info!("Using JIT config on {} (skipping config.sh)", vm);

            // a file avoids shell escaping and command-line length issues
            info!(
                "Writing JIT config file on {} ({} bytes)",
                vm,
                job.jit_config.len()
            );
            let write = self
                .session
                .execute_secret(
                    &builder.write_jit_config_command(job.jit_config),
                    "write JIT config",
                )
                .await?;
            if write.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "JIT config write failed on {}: {}",
                    vm,
                    write.stderr.trim()
                );
                return Err(RunnerError::Runner(format!(
                    "Failed to write JIT config to VM (exit code {})",
                    write.exit_code
                )));
            }
            if !write.stdout.is_empty() {
                info!(
                    local_only = tracing::field::Empty,
                    "JIT config write stdout ({}): {}",
                    vm,
                    write.stdout.trim()
                );
            }
            if !write.stderr.is_empty() {
                warn!(
                    local_only = tracing::field::Empty,
                    "JIT config write stderr ({}): {}",
                    vm,
                    write.stderr.trim()
                );
            }
            builder.run_command_jit_from_file()
        } else {
            let config_cmd = builder.config_command(
                job.runner_scope_url,
                job.registration_token,
                job.labels,
                job.runner_name,
            );
            info!("Running configuration command on {}", vm);
            let output = self
                .session
                .execute_secret(&config_cmd, "config.sh (registration token)")
                .await?;
            if output.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "Runner configuration failed (exit {}): stdout={}, stderr={}",
                    output.exit_code,
                    output.stdout,
                    output.stderr
                );
                return Err(RunnerError::Runner(format!(
                    "Configuration failed with exit code {}",
                    output.exit_code
                )));
            }
            if !output.stdout.is_empty() {
                info!(
                    local_only = tracing::field::Empty,
                    "Config output: {}",
                    output.stdout.trim()
                );
            }
            builder.run_command_direct()
        };

        Ok(PreparedRunner {
            kind: Prepared::GitHub { builder, jit },
            command,
        })
    }

    async fn run_github(
        &mut self,
        builder: &RunnerConfigBuilder,
        command: &str,
        jit: bool,
    ) -> Result<()> {
        let vm = self.vm.clone();
        info!("Running runner on {} (will block until job completes)", vm);
        let started = Instant::now();
        let output = self.session.execute(command).await?;
        let elapsed = started.elapsed();

        info!(
            "Runner completed on {} with exit code {} in {:.1}s",
            vm,
            output.exit_code,
            elapsed.as_secs_f64()
        );
        if !output.stdout.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "Runner stdout ({}): {}",
                vm,
                output.stdout.trim()
            );
        }
        if !output.stderr.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "Runner stderr ({}): {}",
                vm,
                output.stderr.trim()
            );
        }

        // a suspiciously fast exit: grab what diagnostics we can before the VM is destroyed
        if elapsed < Duration::from_secs(30) {
            self.fast_exit_diagnostics(builder, &output, elapsed, jit)
                .await;
        }

        if output.exit_code != 0 {
            return Err(RunnerError::Runner(format!(
                "Runner exited with code {}",
                output.exit_code
            )));
        }
        Ok(())
    }

    async fn fast_exit_diagnostics(
        &mut self,
        builder: &RunnerConfigBuilder,
        output: &CommandOutput,
        elapsed: Duration,
        jit: bool,
    ) {
        let vm = &self.vm;
        if jit {
            warn!(
                "JIT runner on {} exited in {:.1}s — runner was pre-assigned a job, \
                 this likely means the runner binary failed to start",
                vm,
                elapsed.as_secs_f64()
            );
        } else {
            warn!(
                "Runner on {} exited in {:.1}s — may not have picked up a job",
                vm,
                elapsed.as_secs_f64()
            );
        }

        // Always log stdout/stderr at WARN level for fast exits (even if empty)
        warn!(
            local_only = tracing::field::Empty,
            "Early exit diagnostics ({}): exit_code={}, stdout_len={}, stderr_len={}, \
             stdout={:?}, stderr={:?}",
            vm,
            output.exit_code,
            output.stdout.len(),
            output.stderr.len(),
            output.stdout.chars().take(2000).collect::<String>(),
            output.stderr.chars().take(2000).collect::<String>(),
        );

        match self.session.execute(&builder.diag_log_command(100)).await {
            Ok(diag) if diag.stdout.trim().is_empty() => {
                warn!(
                    "Runner _diag ({}): no diagnostic logs found \
                     (runner may not have started at all)",
                    vm
                );
            }
            Ok(diag) => {
                warn!(
                    local_only = tracing::field::Empty,
                    "Runner _diag ({}): {}",
                    vm,
                    diag.stdout.trim()
                );
            }
            Err(e) => {
                warn!(
                    "Runner _diag ({}): failed to fetch diagnostic logs: {}",
                    vm, e
                );
            }
        }

        // On Windows, also check the runner binary is still there
        if self.shell.is_windows()
            && let Ok(check) = self
                .session
                .execute(&builder.check_installed_command())
                .await
        {
            warn!(
                "Runner install check ({}): {}",
                vm,
                if check.stdout.trim() == "installed" {
                    "runner binary present"
                } else {
                    "runner binary NOT FOUND"
                }
            );
        }
    }

    /// Linux/macOS use `~/forgejo-runner` and bash; Windows uses `C:\forgejo-runner` and
    /// PowerShell scripts sent with `-EncodedCommand`, which work from cmd.exe and PowerShell
    /// SSH shells alike.
    async fn prepare_forgejo(
        &mut self,
        forgejo: &ForgejoRunner,
        labels: &[String],
        versions: &RunnerVersions<'_>,
        ctx: &JobContext,
    ) -> Result<PreparedRunner> {
        let vm = &self.vm;
        let is_windows = self.shell.is_windows();
        let win_dir = forgejo_runner::WINDOWS_RUNNER_DIR;

        let check_cmd = if is_windows {
            forgejo_runner::windows_check_installed_command(win_dir)
        } else {
            forgejo_runner::check_installed_command()
        };
        let check = self.session.execute(&check_cmd).await?;
        if check.stdout.trim() != "installed" {
            ctx.step(step::INSTALL);
            let version = forgejo_runner::resolve_version(versions.forgejo).to_string();
            let (os, arch) = if is_windows {
                // like the GitHub runner: x64 Windows only for now
                (forgejo_runner::Os::Windows, Arch::X64)
            } else {
                let uname = self.session.execute("uname -m").await?;
                let arch = Arch::from_uname(&uname.stdout).ok_or_else(|| {
                    RunnerError::Runner(format!(
                        "forgejo-runner has no release for architecture '{}'",
                        uname.stdout.trim()
                    ))
                })?;
                let os = if self.shell == GuestShell::MacOS {
                    forgejo_runner::Os::MacOS
                } else {
                    forgejo_runner::Os::Linux
                };
                (os, arch)
            };
            let url = forgejo_runner::download_url(versions.forgejo_url, &version, os, arch);
            info!(
                "forgejo-runner not installed on {}, installing v{} ({}) from {}...",
                vm,
                version,
                arch.as_str(),
                url
            );
            let cached = versions
                .cache
                .install_forgejo(self.session, &url, &version, os, arch)
                .await;
            if !cached {
                let install_cmd = if is_windows {
                    forgejo_runner::windows_install_command(&url, win_dir)
                } else {
                    forgejo_runner::install_command(&url, &version, os, arch)
                };
                let install = self.session.execute(&install_cmd).await?;
                if install.exit_code != 0 {
                    error!(
                        local_only = tracing::field::Empty,
                        "forgejo-runner installation failed on {}: stdout={}, stderr={}",
                        vm,
                        install.stdout,
                        install.stderr
                    );
                    return Err(RunnerError::Runner(format!(
                        "Failed to install forgejo-runner v{version} (exit code {})",
                        install.exit_code
                    )));
                }
                info!(
                    local_only = tracing::field::Empty,
                    "forgejo-runner installed on {}: {}",
                    vm,
                    install.stdout.trim()
                );
            }
            ctx.step(step::SETUP);
        } else {
            debug!("forgejo-runner already installed on {}", vm);
            ctx.skip_step(step::INSTALL);
        }

        let job = forgejo_runner::OneJob {
            instance_url: &forgejo.instance_url,
            uuid: &forgejo.uuid,
            token: &forgejo.token,
            handle: &forgejo.handle,
            labels,
        };
        let prepare = if is_windows {
            forgejo_runner::windows_prepare_command(&job, win_dir)
        } else {
            forgejo_runner::prepare_command(&job)
        }
        .map_err(RunnerError::Runner)?;
        let prepared = self
            .session
            .execute_secret(&prepare, "write forgejo-runner job files (runner token)")
            .await?;
        if prepared.exit_code != 0 {
            error!(
                local_only = tracing::field::Empty,
                "Writing forgejo-runner job files failed on {}: {}",
                vm,
                prepared.stderr.trim()
            );
            return Err(RunnerError::Runner(format!(
                "Failed to write forgejo-runner job files (exit code {})",
                prepared.exit_code
            )));
        }

        let command = if is_windows {
            forgejo_runner::windows_run_command(win_dir)
        } else {
            forgejo_runner::run_command()
        };
        Ok(PreparedRunner {
            kind: Prepared::Forgejo {
                handle: forgejo.handle.clone(),
            },
            command,
        })
    }

    async fn run_forgejo(&mut self, command: &str, handle: &str) -> Result<()> {
        let vm = &self.vm;
        info!(
            "Running forgejo-runner one-job on {} for handle {} (will block until the job completes)",
            vm, handle
        );
        let started = Instant::now();
        let output = self.session.execute(command).await?;
        let elapsed = started.elapsed();
        info!(
            "forgejo-runner on {} exited with code {} in {:.1}s",
            vm,
            output.exit_code,
            elapsed.as_secs_f64()
        );
        // forgejo-runner logs to stderr; keep the tail of both for diagnosis
        if !output.stdout.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "forgejo-runner stdout ({}): {}",
                vm,
                tail(output.stdout.trim(), 4000)
            );
        }
        if !output.stderr.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "forgejo-runner stderr ({}): {}",
                vm,
                tail(output.stderr.trim(), 4000)
            );
        }
        if output.exit_code != 0 {
            warn!(
                "forgejo-runner on {} failed after {:.1}s (exit {}) for handle {}",
                vm,
                elapsed.as_secs_f64(),
                output.exit_code,
                handle
            );
            return Err(RunnerError::Runner(format!(
                "forgejo-runner exited with code {}",
                output.exit_code
            )));
        }
        Ok(())
    }
}

/// The last `max` chars of `s`.
fn tail(s: &str, max: usize) -> String {
    let skip = s.chars().count().saturating_sub(max);
    s.chars().skip(skip).collect()
}

/// Work out the guest's OS and SSH shell by probing with commands that only work in one.
pub async fn detect_shell(session: &mut SshSession) -> Result<GuestShell, SshError> {
    // $PSVersionTable only exists in PowerShell
    let ps_check = session.execute("echo $PSVersionTable").await?;
    if ps_check.stdout.contains("PSVersion") {
        info!("Detected PowerShell as SSH default shell (Windows)");
        return Ok(GuestShell::WindowsPowerShell);
    }

    // cmd.exe expands %OS%
    let output = session.execute("echo %OS%").await?;
    if output.stdout.contains("Windows_NT") {
        info!("Detected Windows OS via %OS% (cmd.exe shell)");
        return Ok(GuestShell::WindowsCmd);
    }

    let output = session
        .execute("powershell -Command \"Write-Output $env:OS\"")
        .await?;
    if output.stdout.contains("Windows") {
        info!("Detected Windows OS via PowerShell probe (cmd.exe shell)");
        return Ok(GuestShell::WindowsCmd);
    }

    let output = session.execute("uname -s").await?;
    if output.stdout.contains("Linux") {
        info!("Detected Linux OS");
        return Ok(GuestShell::Linux);
    }
    if output.stdout.contains("Darwin") {
        info!("Detected macOS");
        return Ok(GuestShell::MacOS);
    }
    // uname works but says something else: still Unix-like
    if output.exit_code == 0 && !output.stdout.trim().is_empty() {
        info!("Detected Unix-like OS: {}", output.stdout.trim());
        return Ok(GuestShell::Linux);
    }

    // uname failed and nothing else matched
    info!("Could not detect OS, defaulting to Windows with cmd.exe shell");
    Ok(GuestShell::WindowsCmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tail() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        // counts chars, not bytes
        assert_eq!(tail("ééé", 2), "éé");
    }

    #[test]
    fn test_guest_shell_is_windows() {
        assert!(!GuestShell::Linux.is_windows());
        assert!(!GuestShell::MacOS.is_windows());
        assert!(GuestShell::WindowsCmd.is_windows());
        assert!(GuestShell::WindowsPowerShell.is_windows());
    }
}
