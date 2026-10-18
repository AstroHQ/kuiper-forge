//! forgejo-runner download and `one-job` commands, for Forgejo jobs on Linux VMs.
//!
//! Each runner is registered by the coordinator for one job attempt and started
//! with `forgejo-runner one-job --handle`, so it runs exactly that job. Jobs run
//! directly on the VM (`label:host`), like GitHub's runner does.
//!
//! Only Linux (amd64, arm64) has release binaries.

use crate::github_runner::Arch;
use crate::shell::{escape_posix, escape_posix_path};
use tracing::{debug, info};

/// Fallback forgejo-runner version if the release API can't be reached.
pub const FALLBACK_VERSION: &str = "13.2.0";

/// Oldest forgejo-runner with `one-job --handle`.
pub const MIN_VERSION: &str = "13.1.0";

/// Where the runner is installed on the VM.
pub const RUNNER_DIR: &str = "~/forgejo-runner";

const INSTALL_SCRIPT: &str = include_str!("../scripts/install_forgejo_runner.sh");

/// What a VM needs to run one Forgejo job.
pub struct OneJob<'a> {
    pub instance_url: &'a str,
    pub uuid: &'a str,
    pub token: &'a str,
    pub handle: &'a str,
    pub labels: &'a [String],
}

fn arch_str(arch: Arch) -> &'static str {
    match arch {
        Arch::X64 => "amd64",
        Arch::Arm64 => "arm64",
    }
}

/// Fetch the latest forgejo-runner version from code.forgejo.org.
///
/// Returns `None` if the fetch fails (caller should fall back to `FALLBACK_VERSION`).
pub async fn fetch_latest_version() -> Option<String> {
    let url = "https://code.forgejo.org/api/v1/repos/forgejo/runner/releases/latest";
    debug!("Fetching latest forgejo-runner version");

    let output = tokio::process::Command::new("curl")
        .args(["-sfL", "--max-time", "15", url])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        debug!(
            "Failed to fetch latest forgejo-runner version (curl exit {:?})",
            output.status.code()
        );
        return None;
    }

    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    match serde_json::from_slice::<Release>(&output.stdout) {
        Ok(release) => {
            let version = release.tag_name.trim_start_matches('v').to_string();
            info!("Latest forgejo-runner version: v{}", version);
            Some(version)
        }
        Err(e) => {
            debug!("Could not parse forgejo-runner release: {}", e);
            None
        }
    }
}

/// The release binary's file name.
pub fn file_name(version: &str, arch: Arch) -> String {
    format!("forgejo-runner-{version}-linux-{}", arch_str(arch))
}

/// The release binary's download URL.
///
/// ```
/// use kuiper_agent_lib::forgejo_runner::download_url;
/// use kuiper_agent_lib::github_runner::Arch;
///
/// assert_eq!(
///     download_url("13.2.0", Arch::X64),
///     "https://code.forgejo.org/forgejo/runner/releases/download/v13.2.0/forgejo-runner-13.2.0-linux-amd64"
/// );
/// ```
pub fn download_url(version: &str, arch: Arch) -> String {
    format!(
        "https://code.forgejo.org/forgejo/runner/releases/download/v{version}/{}",
        file_name(version, arch)
    )
}

/// Command that prints `installed` if forgejo-runner is installed.
pub fn check_installed_command() -> String {
    format!(
        "test -x {}/forgejo-runner && echo 'installed'",
        escape_posix_path(RUNNER_DIR)
    )
}

/// Command that downloads, checksums and installs forgejo-runner.
pub fn install_command(version: &str, arch: Arch) -> String {
    format!(
        "bash -s -- {} {} {} << 'INSTALL_FORGEJO_RUNNER_EOF'\n{INSTALL_SCRIPT}\nINSTALL_FORGEJO_RUNNER_EOF",
        escape_posix_path(RUNNER_DIR),
        escape_posix(&download_url(version, arch)),
        escape_posix(&file_name(version, arch)),
    )
}

/// A runner label for `one-job --label`: plain labels run on the VM itself.
fn host_label(label: &str) -> String {
    if label.contains(':') {
        label.to_string()
    } else {
        format!("{label}:host")
    }
}

/// Command that writes the runner token (mode 600) and a `run-job.sh` that runs
/// the job, so the token never appears on a command line.
///
/// Errors if a value contains a newline, which could end the script early.
pub fn prepare_command(job: &OneJob<'_>) -> Result<String, String> {
    let values = [job.instance_url, job.uuid, job.token, job.handle];
    if values
        .iter()
        .copied()
        .chain(job.labels.iter().map(String::as_str))
        .any(|v| v.contains('\n') || v.contains('\r'))
    {
        return Err("Forgejo runner values must not contain newlines".to_string());
    }
    if job.handle.is_empty() || job.uuid.is_empty() || job.token.is_empty() {
        return Err("Forgejo runner needs a handle, uuid and token".to_string());
    }

    let dir = escape_posix_path(RUNNER_DIR);
    let labels: String = job
        .labels
        .iter()
        .map(|l| format!(" --label {}", escape_posix(&host_label(l))))
        .collect();
    Ok(format!(
        "umask 077 && mkdir -p {dir} && printf '%s' {token} > {dir}/.runner-token && \
         cat > {dir}/run-job.sh << 'KUIPER_FORGEJO_EOF'\n\
         set -eu\n\
         cd {dir}\n\
         exec ./forgejo-runner one-job --url {url} --uuid {uuid} --token-url \"file://$PWD/.runner-token\" --handle {handle} --wait{labels}\n\
         KUIPER_FORGEJO_EOF",
        token = escape_posix(job.token),
        url = escape_posix(job.instance_url),
        uuid = escape_posix(job.uuid),
        handle = escape_posix(job.handle),
    ))
}

/// Command that runs the job written by [`prepare_command`] and blocks until it's
/// done. A login shell, so PATH has user-installed tools.
pub fn run_command() -> String {
    format!("bash -l {}/run-job.sh", escape_posix_path(RUNNER_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job<'a>(labels: &'a [String]) -> OneJob<'a> {
        OneJob {
            instance_url: "https://git.example.com",
            uuid: "1b4e28ba-2fa1-11d2-883f-0016d3cca427",
            token: "abc123",
            handle: "h-1",
            labels,
        }
    }

    #[test]
    fn download_url_arm64() {
        assert_eq!(
            download_url("13.2.0", Arch::Arm64),
            "https://code.forgejo.org/forgejo/runner/releases/download/v13.2.0/forgejo-runner-13.2.0-linux-arm64"
        );
    }

    #[test]
    fn install_command_checks_the_checksum() {
        let cmd = install_command("13.2.0", Arch::X64);
        assert!(cmd.starts_with("bash -s -- ~/'forgejo-runner' "));
        assert!(cmd.contains("'forgejo-runner-13.2.0-linux-amd64'"));
        assert!(cmd.contains("sha256sum -c"));
    }

    #[test]
    fn prepare_command_writes_token_file_and_script() {
        let labels = vec!["linux".to_string(), "ubuntu:docker://node:20".to_string()];
        let cmd = prepare_command(&job(&labels)).unwrap();
        assert!(cmd.starts_with("umask 077 && mkdir -p ~/'forgejo-runner'"));
        assert!(cmd.contains("printf '%s' 'abc123' > ~/'forgejo-runner'/.runner-token"));
        assert!(cmd.contains("--url 'https://git.example.com'"));
        assert!(cmd.contains("--token-url \"file://$PWD/.runner-token\""));
        assert!(cmd.contains("--handle 'h-1' --wait"));
        assert!(cmd.contains("--label 'linux:host'"));
        // labels that name a schema are passed through
        assert!(cmd.contains("--label 'ubuntu:docker://node:20'"));
        // the token is only in the token file, not the script
        let script = cmd.split("KUIPER_FORGEJO_EOF").nth(1).unwrap();
        assert!(!script.contains("abc123"));
        assert!(cmd.ends_with("\nKUIPER_FORGEJO_EOF"));
    }

    #[test]
    fn prepare_command_rejects_newlines() {
        let labels = vec!["linux\nKUIPER_FORGEJO_EOF\nrm -rf ~".to_string()];
        assert!(prepare_command(&job(&labels)).is_err());
        let mut bad = job(&[]);
        bad.handle = "";
        assert!(prepare_command(&bad).is_err());
    }

    #[test]
    fn run_command_uses_login_shell() {
        assert_eq!(run_command(), "bash -l ~/'forgejo-runner'/run-job.sh");
    }
}
