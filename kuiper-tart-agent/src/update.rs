//! Self-update from GitHub releases.
//!
//! The repo releases several crates, so GitHub's "latest release" is often not ours. We list releases and pick the
//! newest `kuiper-tart-agent-v*` tag instead.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow, bail, ensure};
use dialoguer::Confirm;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::install;

const REPO: &str = "AstroHQ/kuiper-forge";
const BIN: &str = "kuiper-tart-agent";
const TAG_PREFIX: &str = "kuiper-tart-agent-v";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn version(&self) -> Option<Version> {
        Version::parse(self.tag_name.strip_prefix(TAG_PREFIX)?).ok()
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Handle the update subcommand. `version` pins a specific release (allows downgrades), otherwise the newest one.
pub async fn run(version: Option<String>, check: bool, yes: bool) -> anyhow::Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let client = client()?;

    let (release, new) = match version {
        Some(v) => {
            let v = Version::parse(v.trim_start_matches('v')).context("invalid version")?;
            (fetch_tag(&client, &v).await?, v)
        }
        None => {
            let release = fetch_latest(&client).await?;
            let new = release.version().expect("filtered on parse");
            if new <= current {
                println!("kuiper-tart-agent {current} is up to date");
                return Ok(());
            }
            (release, new)
        }
    };

    if check {
        println!("kuiper-tart-agent {new} is available (installed: {current})");
        println!("Run `kuiper-tart-agent update` to install it");
        return Ok(());
    }

    let target = format!("{}-apple-darwin", std::env::consts::ARCH);
    let archive_name = format!("{BIN}-v{new}-{target}.tar.gz");
    let archive = release
        .asset(&archive_name)
        .ok_or_else(|| anyhow!("release {} has no {archive_name}", release.tag_name))?;

    println!("Downloading {archive_name}...");
    let bytes = download(&client, &archive.browser_download_url).await?;

    // releases before checksums were added to CI don't have one
    match release.asset(&format!("{archive_name}.sha256")) {
        Some(sum) => {
            let text = String::from_utf8(download(&client, &sum.browser_download_url).await?)?;
            let expected = text
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_lowercase();
            let actual = hex::encode(Sha256::digest(&bytes));
            ensure!(
                expected == actual,
                "checksum mismatch for {archive_name}: expected {expected}, got {actual}"
            );
            println!("Checksum OK");
        }
        None => {
            println!("Warning: no checksum published for {archive_name}, skipping verification")
        }
    }

    let tmp = std::env::temp_dir().join(format!("{BIN}-update-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let result = install_from_archive(&bytes, &tmp);
    let _ = fs::remove_dir_all(&tmp);
    let exe = result?;
    println!("\u{2713} Updated {} from {current} to {new}", exe.display());

    restart_service(yes)
}

/// Extracts the archive into `tmp` and swaps it in for the running binary. Returns the binary's path.
fn install_from_archive(bytes: &[u8], tmp: &Path) -> anyhow::Result<std::path::PathBuf> {
    let archive_path = tmp.join("agent.tar.gz");
    fs::write(&archive_path, bytes)?;

    // macOS always ships bsdtar, not worth pulling in tar + flate2
    let status = Command::new("tar")
        .arg("xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(tmp)
        .status()
        .context("failed to run tar")?;
    ensure!(status.success(), "failed to extract archive");

    let new_bin = tmp.join(BIN);
    ensure!(new_bin.exists(), "archive didn't contain {BIN}");

    // catches a wrong-arch or broken download before it replaces a working binary
    let output = Command::new(&new_bin)
        .arg("--version")
        .output()
        .context("downloaded binary failed to run")?;
    ensure!(output.status.success(), "downloaded binary failed to run");

    let exe = std::env::current_exe()?.canonicalize()?;
    replace_binary(&new_bin, &exe)?;
    Ok(exe)
}

/// Swaps `new` in at `exe`, falling back to sudo if the install dir isn't writable (e.g. root-owned
/// `/usr/local/bin`). Both paths unlink/rename rather than write over the file: macOS caches code signatures per
/// vnode, so rewriting a signed binary in place gets the next launch SIGKILLed.
fn replace_binary(new: &Path, exe: &Path) -> anyhow::Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("binary has no parent dir"))?;
    let staged = dir.join(format!(".{BIN}.new"));

    match fs::copy(new, &staged) {
        Ok(_) => {
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
            fs::rename(&staged, exe)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("{} isn't writable, using sudo", dir.display());
            let status = Command::new("sudo")
                .args(["install", "-m", "755"])
                .arg(new)
                .arg(exe)
                .status()
                .context("failed to run sudo")?;
            ensure!(status.success(), "sudo install failed");
        }
        Err(e) => return Err(e).with_context(|| format!("failed to write {}", staged.display())),
    }
    Ok(())
}

fn restart_service(yes: bool) -> anyhow::Result<()> {
    if !install::is_service_running() {
        return Ok(());
    }

    // shutdown destroys every VM, so a restart fails whatever jobs are running on this host
    let restart = yes
        || Confirm::new()
            .with_prompt(
                "Restart the LaunchAgent now? Running VMs get destroyed, failing their jobs",
            )
            .default(false)
            .interact()
            .unwrap_or(false);

    if restart {
        install::restart_service().map_err(|e| anyhow!("{e}"))?;
        println!("\u{2713} Service restarted");
    } else {
        println!("The service still runs the old version until it restarts:");
        println!("  launchctl kickstart -k {}", install::service_target());
    }
    Ok(())
}

fn client() -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Accept", "application/vnd.github+json".parse()?);

    // unauthenticated API calls are limited to 60/hour per IP, which a fleet behind one NAT can hit
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        headers.insert("Authorization", format!("Bearer {token}").parse()?);
    }

    Ok(reqwest::Client::builder()
        .user_agent(concat!("kuiper-tart-agent/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()?)
}

async fn fetch_latest(client: &reqwest::Client) -> anyhow::Result<Release> {
    // newest first, so the first page with any of ours has the newest one
    for page in 1..=5 {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100&page={page}");
        let releases: Vec<Release> = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if releases.is_empty() {
            break;
        }
        let newest = releases
            .into_iter()
            .filter(|r| !r.draft && !r.prerelease)
            .filter_map(|r| r.version().map(|v| (v, r)))
            // release.yml never sets the prerelease flag, so skip -rc style tags too (matches the install script)
            .filter(|(v, _)| v.pre.is_empty())
            .max_by(|a, b| a.0.cmp(&b.0));
        if let Some((_, release)) = newest {
            return Ok(release);
        }
    }
    bail!("no kuiper-tart-agent release found in {REPO}")
}

async fn fetch_tag(client: &reqwest::Client, version: &Version) -> anyhow::Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/tags/{TAG_PREFIX}{version}");
    let resp = client.get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no release for kuiper-tart-agent {version}");
    }
    Ok(resp.error_for_status()?.json().await?)
}

async fn download(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec())
}
