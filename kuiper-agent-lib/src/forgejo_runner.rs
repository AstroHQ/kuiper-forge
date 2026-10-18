//! forgejo-runner download and `one-job` commands, for Forgejo jobs on Linux VMs.
//!
//! Each runner is registered by the coordinator for one job attempt and started
//! with `forgejo-runner one-job --handle`, so it runs exactly that job. Jobs run
//! directly on the VM (`label:host`), like GitHub's runner does.
//!
//! Binaries come from kuiper-forge's own `forgejo-runner-v*` releases (built by
//! `.github/workflows/forgejo-runner.yml` from the unmodified upstream tag), since
//! upstream only publishes Linux builds. Every agent uses the same pinned version.

use crate::github_runner::Arch;
use crate::shell::{escape_posix, escape_posix_path};

/// The forgejo-runner version agents install unless configured otherwise. Must have a
/// `forgejo-runner-v{version}` release on kuiper-forge.
pub const DEFAULT_VERSION: &str = "13.2.0";

/// Oldest forgejo-runner with `one-job --handle`.
pub const MIN_VERSION: &str = "13.1.0";

/// Where agents download forgejo-runner from. `{version}`, `{os}` (`linux`, `darwin`,
/// `windows`), `{arch}` (`amd64`, `arm64`) and `{ext}` (`.exe` on Windows) are filled
/// in; a `.sha256` next to the binary must hold its checksum.
pub const DEFAULT_URL_TEMPLATE: &str = "https://github.com/AstroHQ/kuiper-forge/releases/download/forgejo-runner-v{version}/forgejo-runner-{version}-{os}-{arch}{ext}";

/// The operating system a forgejo-runner binary is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOS,
    Windows,
}

impl Os {
    /// The name in release file names (Go's GOOS).
    pub fn as_str(&self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::MacOS => "darwin",
            Os::Windows => "windows",
        }
    }

    fn ext(&self) -> &'static str {
        match self {
            Os::Windows => ".exe",
            Os::Linux | Os::MacOS => "",
        }
    }
}

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

/// The version to install for a configured value: empty or `latest` means
/// [`DEFAULT_VERSION`] (there's no "latest" lookup, so agents stay on a version
/// kuiper has published), anything else is used as given.
pub fn resolve_version(configured: &str) -> &str {
    match configured.trim() {
        "" | "latest" => DEFAULT_VERSION,
        v => v.trim_start_matches('v'),
    }
}

/// The release binary's file name.
pub fn file_name(version: &str, os: Os, arch: Arch) -> String {
    format!(
        "forgejo-runner-{version}-{}-{}{}",
        os.as_str(),
        arch_str(arch),
        os.ext()
    )
}

/// The release binary's download URL, from a URL template (see [`DEFAULT_URL_TEMPLATE`]).
///
/// ```
/// use kuiper_agent_lib::forgejo_runner::{DEFAULT_URL_TEMPLATE, Os, download_url};
/// use kuiper_agent_lib::github_runner::Arch;
///
/// assert_eq!(
///     download_url(DEFAULT_URL_TEMPLATE, "13.2.0", Os::Linux, Arch::X64),
///     "https://github.com/AstroHQ/kuiper-forge/releases/download/forgejo-runner-v13.2.0/forgejo-runner-13.2.0-linux-amd64"
/// );
/// ```
pub fn download_url(template: &str, version: &str, os: Os, arch: Arch) -> String {
    template
        .replace("{version}", version)
        .replace("{os}", os.as_str())
        .replace("{arch}", arch_str(arch))
        .replace("{ext}", os.ext())
}

/// Command that prints `installed` if forgejo-runner is installed.
pub fn check_installed_command() -> String {
    format!(
        "test -x {}/forgejo-runner && echo 'installed'",
        escape_posix_path(RUNNER_DIR)
    )
}

/// Command that downloads, checksums and installs forgejo-runner on a Linux or
/// macOS VM, from `url` (see [`download_url`]) and its `.sha256`.
pub fn install_command(url: &str, version: &str, os: Os, arch: Arch) -> String {
    format!(
        "bash -s -- {} {} {} << 'INSTALL_FORGEJO_RUNNER_EOF'\n{INSTALL_SCRIPT}\nINSTALL_FORGEJO_RUNNER_EOF",
        escape_posix_path(RUNNER_DIR),
        escape_posix(url),
        escape_posix(&file_name(version, os, arch)),
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
    fn download_urls_per_platform() {
        let url = |os, arch| download_url(DEFAULT_URL_TEMPLATE, "13.2.0", os, arch);
        let base =
            "https://github.com/AstroHQ/kuiper-forge/releases/download/forgejo-runner-v13.2.0";
        assert_eq!(
            url(Os::Linux, Arch::Arm64),
            format!("{base}/forgejo-runner-13.2.0-linux-arm64")
        );
        assert_eq!(
            url(Os::MacOS, Arch::Arm64),
            format!("{base}/forgejo-runner-13.2.0-darwin-arm64")
        );
        assert_eq!(
            url(Os::Windows, Arch::X64),
            format!("{base}/forgejo-runner-13.2.0-windows-amd64.exe")
        );
        // a mirror can use any layout
        assert_eq!(
            download_url(
                "https://mirror.example/fr/{os}/{arch}/v{version}{ext}",
                "13.1.0",
                Os::Windows,
                Arch::Arm64
            ),
            "https://mirror.example/fr/windows/arm64/v13.1.0.exe"
        );
    }

    #[test]
    fn latest_means_the_pinned_version() {
        assert_eq!(resolve_version(""), DEFAULT_VERSION);
        assert_eq!(resolve_version("latest"), DEFAULT_VERSION);
        assert_eq!(resolve_version("v13.1.0"), "13.1.0");
        assert_eq!(resolve_version("13.3.0"), "13.3.0");
    }

    #[test]
    fn install_command_checks_the_checksum() {
        let url = download_url(DEFAULT_URL_TEMPLATE, "13.2.0", Os::Linux, Arch::X64);
        let cmd = install_command(&url, "13.2.0", Os::Linux, Arch::X64);
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
