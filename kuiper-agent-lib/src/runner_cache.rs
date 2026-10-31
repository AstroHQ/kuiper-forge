//! Copies of runner releases kept on the agent, so that a VM gets its runner over SSH from the
//! agent instead of downloading it, and the latest runner version remembered for a while so that
//! not every job asks GitHub (60 requests an hour per IP without a token).
//!
//! The cache is only ever a shortcut: anything going wrong here is logged, and the VM downloads
//! the runner itself like it would without one.
//!
//! In the cache directory:
//!
//! ```text
//! github-latest.json                            what "latest" meant, and when that was checked
//! github/linux-x64/actions-runner-*.tar.gz      newest copy per platform, older ones are removed
//! github/linux-x64/actions-runner-*.tar.gz.sha256
//! forgejo/linux-amd64/forgejo-runner-*
//! forgejo/linux-amd64/forgejo-runner-*.sha256
//! ```
//!
//! A copy is checked against its `.sha256` every time it's used, and downloaded again if it
//! doesn't match, so a bad file can't stick around.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::forgejo_runner::{self, Os};
use crate::github_runner::{self, Arch, FALLBACK_VERSION, Platform, RunnerConfigBuilder};
use crate::ssh::SshSession;

const GITHUB_RELEASES: &str = "https://api.github.com/repos/actions/runner/releases";

const LATEST_FILE: &str = "github-latest.json";

/// Longest a download may take. Every job for its platform waits on it, without the cache each VM
/// would only be held up by its own.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// How long jobs skip the download after one failed. They'd otherwise each try in turn, so with a
/// stalled GitHub the last job in line would wait out every timeout before it.
const RETRY_DOWNLOAD_AFTER: Duration = Duration::from_secs(300);

/// The `[runner_cache]` config section.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerCacheConfig {
    /// Keep copies of runner downloads and upload them to VMs
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// How long a looked up "latest" runner version is used before asking GitHub again
    #[serde(default = "default_version_ttl_secs")]
    pub version_ttl_secs: u64,
}

fn default_enabled() -> bool {
    true
}

fn default_version_ttl_secs() -> u64 {
    3600
}

impl Default for RunnerCacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            version_ttl_secs: default_version_ttl_secs(),
        }
    }
}

#[derive(Debug, Error)]
enum CacheError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("no checksum for {0}")]
    NoChecksum(String),

    #[error("the download failed recently, not trying again yet")]
    RecentlyFailed,

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },
}

type Result<T, E = CacheError> = std::result::Result<T, E>;

/// A GitHub runner release as the API has it.
#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
}

/// What "latest" meant the last time we asked.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct LatestRelease {
    version: String,
    /// By platform, e.g. `linux-x64`
    sha256: HashMap<String, String>,
    /// Seconds since the Unix epoch
    checked_at: u64,
}

impl LatestRelease {
    fn new(release: GitHubRelease) -> Self {
        Self {
            version: release.tag_name.trim_start_matches('v').to_string(),
            sha256: checksums(release.body.as_deref().unwrap_or_default()),
            checked_at: now(),
        }
    }

    fn age(&self) -> Duration {
        Duration::from_secs(now().saturating_sub(self.checked_at))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// SHA-256 per platform from a runner release's notes, which have lines like
/// `- actions-runner-linux-x64-2.338.2.tar.gz <!-- BEGIN SHA linux-x64 -->abc...<!-- END SHA linux-x64 -->`.
/// The API's per-asset `digest` would be neater, but older releases don't have one.
fn checksums(notes: &str) -> HashMap<String, String> {
    notes
        .split("<!-- BEGIN SHA ")
        .skip(1)
        .filter_map(|entry| {
            let (platform, rest) = entry.split_once(" -->")?;
            let sha256 = rest.get(..64)?;
            sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then(|| (platform.to_string(), sha256.to_ascii_lowercase()))
        })
        .collect()
}

/// Locked during a download, holds when the last one failed.
type DownloadSlot = Arc<Mutex<Option<Instant>>>;

/// Runner downloads kept in a directory, see the [module docs](self).
pub struct RunnerCache {
    dir: PathBuf,
    enabled: bool,
    version_ttl: Duration,
    releases_url: String,
    http: reqwest::Client,
    /// Locked for the whole lookup, so concurrent jobs share one
    latest: Mutex<Option<LatestRelease>>,
    /// Checksums per platform of other releases by version, so they're only looked up once
    release_checksums: Mutex<HashMap<String, HashMap<String, String>>>,
    /// One per platform directory, so concurrent jobs share one download
    downloads: Mutex<HashMap<PathBuf, DownloadSlot>>,
    retry_download_after: Duration,
}

impl RunnerCache {
    /// A cache in `dir`, which is created when there's something to put in it.
    pub fn new(dir: PathBuf, config: &RunnerCacheConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kuiper-agent/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            // between reads, a whole download gets DOWNLOAD_TIMEOUT
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            dir,
            enabled: config.enabled,
            version_ttl: Duration::from_secs(config.version_ttl_secs),
            releases_url: GITHUB_RELEASES.to_string(),
            http,
            latest: Mutex::new(None),
            release_checksums: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            retry_download_after: RETRY_DOWNLOAD_AFTER,
        })
    }

    /// The GitHub runner version to install: `configured`, or the latest release when it's empty
    /// or "latest". GitHub is asked at most once per `version_ttl_secs`, and if it can't be reached
    /// the last answer is used, then [`FALLBACK_VERSION`] if there isn't one.
    pub async fn github_version(&self, configured: &str) -> String {
        let configured = configured.trim();
        if !configured.is_empty() && configured != "latest" {
            return configured.trim_start_matches('v').to_string();
        }

        let mut latest = self.latest.lock().await;
        if latest.is_none() {
            *latest = self.load_latest().await;
        }
        if let Some(known) = latest.as_ref()
            && known.age() < self.version_ttl
        {
            return known.version.clone();
        }

        match self.github_release("latest").await {
            Ok(release) => {
                let release = LatestRelease::new(release);
                info!("Latest GitHub Actions runner version: v{}", release.version);
                self.save_latest(&release).await;
                latest.insert(release).version.clone()
            }
            Err(e) => match latest.as_mut() {
                Some(known) => {
                    warn!(
                        "Could not fetch latest runner version, staying on v{}: {e}",
                        known.version
                    );
                    // not every job should wait on this while GitHub is rate limiting us
                    known.checked_at = now();
                    let known = known.clone();
                    self.save_latest(&known).await;
                    known.version
                }
                None => {
                    warn!(
                        "Could not fetch latest runner version, using fallback v{FALLBACK_VERSION}: {e}"
                    );
                    FALLBACK_VERSION.to_string()
                }
            },
        }
    }

    async fn load_latest(&self) -> Option<LatestRelease> {
        let saved = fs::read(self.dir.join(LATEST_FILE)).await.ok()?;
        serde_json::from_slice(&saved)
            .inspect_err(|e| warn!("Ignoring unreadable {LATEST_FILE}: {e}"))
            .ok()
    }

    /// Only costs a lookup after a restart if this fails, so that's not worth failing for.
    async fn save_latest(&self, latest: &LatestRelease) {
        let saved = async {
            fs::create_dir_all(&self.dir).await?;
            let path = self.dir.join(LATEST_FILE);
            let partial = path.with_extension("json.part");
            fs::write(
                &partial,
                serde_json::to_vec(latest).map_err(std::io::Error::other)?,
            )
            .await?;
            fs::rename(&partial, &path).await
        };
        if let Err(e) = saved.await {
            warn!("Failed to save {LATEST_FILE}: {e}");
        }
    }

    /// `release` is `latest` or `tags/v{version}`.
    async fn github_release(&self, release: &str) -> Result<GitHubRelease> {
        debug!("Asking GitHub for runner release {}", release);
        Ok(self
            .http
            .get(format!("{}/{release}", self.releases_url))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Install the GitHub runner on the VM from the cache, downloading it first if needed. False if
    /// that didn't work out, in which case the VM has to download it itself.
    pub async fn install_github(
        &self,
        session: &mut SshSession,
        builder: &RunnerConfigBuilder,
        version: &str,
        arch: Arch,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let archive = match self.github_archive(version, builder.platform(), arch).await {
            Ok(archive) => archive,
            Err(e) => {
                warn!("No cached copy of the GitHub runner v{version}: {e}");
                return false;
            }
        };
        install(
            session,
            &archive,
            &builder.upload_name(),
            &builder.install_upload_command(),
        )
        .await
    }

    /// Install forgejo-runner on the VM from the cache, downloading it from `url` first if needed.
    /// False if that didn't work out, in which case the VM has to download it itself.
    pub async fn install_forgejo(
        &self,
        session: &mut SshSession,
        url: &str,
        version: &str,
        os: Os,
        arch: Arch,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let binary = match self.forgejo_binary(url, version, os, arch).await {
            Ok(binary) => binary,
            Err(e) => {
                warn!("No cached copy of forgejo-runner v{version}: {e}");
                return false;
            }
        };
        let command = match os {
            Os::Windows => {
                forgejo_runner::windows_install_upload_command(forgejo_runner::WINDOWS_RUNNER_DIR)
            }
            Os::Linux | Os::MacOS => forgejo_runner::install_upload_command(),
        };
        install(session, &binary, forgejo_runner::UPLOAD_NAME, &command).await
    }

    async fn github_archive(
        &self,
        version: &str,
        platform: Platform,
        arch: Arch,
    ) -> Result<PathBuf> {
        let key = github_platform(platform, arch);
        let path = self
            .dir
            .join("github")
            .join(&key)
            .join(github_runner::archive_name(version, platform, arch));
        let url = github_runner::download_url(version, platform, arch);
        self.cached(path, &url, self.github_sha256(version, &key))
            .await
    }

    /// The published checksum of a GitHub runner archive, for a VM downloading it itself to check.
    /// `None` if it can't be had, a VM can do without it.
    pub async fn github_checksum(
        &self,
        version: &str,
        platform: Platform,
        arch: Arch,
    ) -> Option<String> {
        let key = github_platform(platform, arch);
        self.github_sha256(version, &key)
            .await
            .inspect_err(|e| warn!("VM can't check its download of runner v{version}: {e}"))
            .ok()
    }

    /// Checksum of a GitHub runner archive. `latest` usually has it already; a pinned version or
    /// the fallback is looked up the first time it's needed.
    async fn github_sha256(&self, version: &str, key: &str) -> Result<String> {
        if let Some(latest) = self.latest.lock().await.as_ref()
            && latest.version == version
            && let Some(sha256) = latest.sha256.get(key)
        {
            return Ok(sha256.clone());
        }
        let mut known = self.release_checksums.lock().await;
        if !known.contains_key(version) {
            let release = self.github_release(&format!("tags/v{version}")).await?;
            let found = checksums(release.body.as_deref().unwrap_or_default());
            known.insert(version.to_string(), found);
        }
        known
            .get(version)
            .and_then(|found| found.get(key))
            .cloned()
            .ok_or_else(|| CacheError::NoChecksum(format!("runner v{version} {key}")))
    }

    async fn forgejo_binary(
        &self,
        url: &str,
        version: &str,
        os: Os,
        arch: Arch,
    ) -> Result<PathBuf> {
        let path = self
            .dir
            .join("forgejo")
            .join(forgejo_runner::platform_name(os, arch))
            .join(forgejo_runner::file_name(version, os, arch));
        self.cached(path, url, self.sha256_file(url)).await
    }

    /// The checksum in the `.sha256` file next to `url`.
    async fn sha256_file(&self, url: &str) -> Result<String> {
        let url = format!("{url}.sha256");
        let file = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        file.split_whitespace()
            .next()
            .map(str::to_ascii_lowercase)
            .ok_or(CacheError::NoChecksum(url))
    }

    /// `path`, downloaded from `url` unless a good copy is there. `sha256` is only awaited then.
    async fn cached(
        &self,
        path: PathBuf,
        url: &str,
        sha256: impl Future<Output = Result<String>>,
    ) -> Result<PathBuf> {
        let dir = path.parent().unwrap_or(&self.dir).to_path_buf();
        let slot = self.downloads.lock().await.entry(dir).or_default().clone();
        let mut last_failure = slot.lock().await;

        if intact(&path).await {
            debug!("Using cached {}", path.display());
            return Ok(path);
        }
        if let Some(failed) = *last_failure
            && failed.elapsed() < self.retry_download_after
        {
            return Err(CacheError::RecentlyFailed);
        }
        let downloaded = async { self.download(url, &sha256.await?, &path).await }.await;
        *last_failure = downloaded.is_err().then(Instant::now);
        downloaded.map(|()| path)
    }

    /// Download `url` to `path`, which with its checksum is all that's left in its directory after.
    /// A VM streaming an older copy keeps its open file.
    async fn download(&self, url: &str, sha256: &str, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(&self.dir);
        fs::create_dir_all(dir).await?;

        info!("Downloading {} to the runner cache", url);
        let started = Instant::now();
        let mut response = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let partial = path.with_extension("part");
        let mut file = fs::File::create(&partial).await?;
        let mut hasher = Sha256::new();
        let mut size = 0;
        while let Some(chunk) = response.chunk().await? {
            hasher.update(&chunk);
            size += chunk.len();
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        let actual = hex::encode(hasher.finalize());
        if actual != sha256 {
            let _ = fs::remove_file(&partial).await;
            return Err(CacheError::ChecksumMismatch {
                url: url.to_string(),
                expected: sha256.to_string(),
                actual,
            });
        }
        let checksum = checksum_path(path);
        fs::write(&checksum, sha256).await?;
        fs::rename(&partial, path).await?;
        info!(
            "Cached {} ({:.1} MB in {:.1}s)",
            path.display(),
            size as f64 / 1_000_000.0,
            started.elapsed().as_secs_f64()
        );

        let mut entries = fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path() != path && entry.path() != checksum {
                debug!("Removing old runner copy {}", entry.path().display());
                fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }
}

/// A platform in runner release file names and checksums, e.g. `linux-x64`.
fn github_platform(platform: Platform, arch: Arch) -> String {
    format!("{}-{}", platform.as_str(), arch.as_str())
}

/// Where the checksum `path` was downloaded with is kept.
fn checksum_path(path: &Path) -> PathBuf {
    path.with_added_extension("sha256")
}

/// Whether the copy at `path` is there and still what was downloaded.
async fn intact(path: &Path) -> bool {
    let Ok(expected) = fs::read_to_string(checksum_path(path)).await else {
        return false;
    };
    match file_sha256(path).await {
        Ok(actual) if actual == expected => true,
        Ok(actual) => {
            warn!(
                "Cached {} is corrupt, replacing it: expected {expected}, got {actual}",
                path.display()
            );
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!("Can't read cached {}, replacing it: {e}", path.display());
            false
        }
    }
}

async fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        match file.read(&mut buffer).await? {
            0 => return Ok(hex::encode(hasher.finalize())),
            read => hasher.update(&buffer[..read]),
        }
    }
}

/// Upload `file` to the VM as `upload_name` and run `command` to install it.
async fn install(session: &mut SshSession, file: &Path, upload_name: &str, command: &str) -> bool {
    let started = Instant::now();
    if let Err(e) = session.upload(file, upload_name).await {
        warn!("Failed to upload {}: {e}", file.display());
        return false;
    }
    let uploaded = started.elapsed();
    match session.execute(command).await {
        Ok(output) if output.exit_code == 0 => {
            info!(
                "Runner installed from cache in {:.1}s ({:.1}s upload)",
                started.elapsed().as_secs_f64(),
                uploaded.as_secs_f64()
            );
            true
        }
        Ok(output) => {
            warn!(
                "Installing cached {} failed (exit code {})",
                file.display(),
                output.exit_code
            );
            warn!(
                local_only = tracing::field::Empty,
                "stdout={}, stderr={}",
                output.stdout.trim(),
                output.stderr.trim()
            );
            false
        }
        Err(e) => {
            warn!("Installing cached {} failed: {e}", file.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    const SHA_LINUX: &str = "baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40";

    fn notes() -> String {
        format!(
            "## SHA-256 Checksums\r\n\r\n\
             - actions-runner-win-x64-2.338.2.zip <!-- BEGIN SHA win-x64 -->{}<!-- END SHA win-x64 -->\r\n\
             - actions-runner-linux-x64-2.338.2.tar.gz <!-- BEGIN SHA linux-x64 -->{SHA_LINUX}<!-- END SHA linux-x64 -->\r\n\
             - broken <!-- BEGIN SHA osx-x64 -->nothex<!-- END SHA osx-x64 -->\r\n",
            "A".repeat(64),
        )
    }

    fn release_json(version: &str) -> String {
        serde_json::json!({"tag_name": format!("v{version}"), "body": notes()}).to_string()
    }

    /// A local HTTP server answering each request with what `respond` makes of the path.
    struct Server {
        url: String,
        requests: Arc<AtomicUsize>,
    }

    impl Server {
        async fn start<F>(respond: F) -> Self
        where
            F: Fn(&str) -> (u16, Vec<u8>) + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let respond = Arc::new(respond);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let count = count.clone();
                    let respond = respond.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buf = [0; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut buf).await {
                                Ok(n) if n > 0 => request.extend_from_slice(&buf[..n]),
                                _ => return,
                            }
                        }
                        count.fetch_add(1, Ordering::SeqCst);
                        let request = String::from_utf8_lossy(&request);
                        let path = request.split_whitespace().nth(1).unwrap_or_default();
                        let (status, body) = respond(path);
                        let head = format!(
                            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&body).await;
                    });
                }
            });
            Self { url, requests }
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    fn cache(dir: &Path, releases_url: &str, ttl: u64) -> RunnerCache {
        let config = RunnerCacheConfig {
            enabled: true,
            version_ttl_secs: ttl,
        };
        let mut cache = RunnerCache::new(dir.to_path_buf(), &config).unwrap();
        cache.releases_url = releases_url.to_string();
        cache
    }

    fn sha256(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    #[test]
    fn test_config_defaults() {
        let config: RunnerCacheConfig = serde_json::from_str("{}").unwrap();
        assert!(config.enabled);
        assert_eq!(config.version_ttl_secs, 3600);
    }

    #[test]
    fn test_checksums() {
        let found = checksums(&notes());
        assert_eq!(found["linux-x64"], SHA_LINUX);
        assert_eq!(found["win-x64"], "a".repeat(64));
        assert!(!found.contains_key("osx-x64"));
        assert!(checksums("no checksums here").is_empty());
    }

    #[tokio::test]
    async fn test_pinned_version_never_asks() {
        let dir = tempfile::tempdir().unwrap();
        // nothing listens here, asking would fall back
        let cache = cache(dir.path(), "http://127.0.0.1:1", 3600);
        assert_eq!(cache.github_version("2.300.0").await, "2.300.0");
        assert_eq!(cache.github_version("v2.300.0").await, "2.300.0");
        assert_eq!(cache.github_version(" v2.300.0\n").await, "2.300.0");
    }

    #[tokio::test]
    async fn test_latest_is_remembered_for_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, release_json("2.338.2").into_bytes())).await;

        let first = cache(dir.path(), &server.url, 3600);
        assert_eq!(first.github_version("latest").await, "2.338.2");
        assert_eq!(first.github_version("").await, "2.338.2");
        assert_eq!(server.requests(), 1);

        // also after a restart
        let restarted = cache(dir.path(), &server.url, 3600);
        assert_eq!(restarted.github_version("latest").await, "2.338.2");
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_latest_is_checked_again_after_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let answers = Arc::new(AtomicUsize::new(0));
        let server = Server::start(move |_| {
            let version = format!("2.33{}.0", answers.fetch_add(1, Ordering::SeqCst));
            (200, release_json(&version).into_bytes())
        })
        .await;

        let cache = cache(dir.path(), &server.url, 0);
        assert_eq!(cache.github_version("latest").await, "2.330.0");
        assert_eq!(cache.github_version("latest").await, "2.331.0");
    }

    #[tokio::test]
    async fn test_concurrent_lookups_share_one_request() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, release_json("2.338.2").into_bytes())).await;
        let cache = cache(dir.path(), &server.url, 3600);

        let (a, b, c) = tokio::join!(
            cache.github_version("latest"),
            cache.github_version("latest"),
            cache.github_version("latest"),
        );
        assert_eq!(
            (a.as_str(), b.as_str(), c.as_str()),
            ("2.338.2", "2.338.2", "2.338.2")
        );
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_failed_check_keeps_the_last_answer_and_waits_a_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let expired = LatestRelease {
            version: "2.337.0".to_string(),
            sha256: HashMap::new(),
            checked_at: now() - 7200,
        };
        std::fs::write(
            dir.path().join(LATEST_FILE),
            serde_json::to_vec(&expired).unwrap(),
        )
        .unwrap();
        // what a rate limited client gets
        let server = Server::start(|_| (403, b"{\"message\": \"rate limit\"}".to_vec())).await;

        let cache = cache(dir.path(), &server.url, 3600);
        assert_eq!(cache.github_version("latest").await, "2.337.0");
        assert_eq!(cache.github_version("latest").await, "2.337.0");
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_nothing_known_uses_the_fallback_and_keeps_trying() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (403, Vec::new())).await;
        let cache = cache(dir.path(), &server.url, 3600);
        assert_eq!(cache.github_version("latest").await, FALLBACK_VERSION);
        assert_eq!(cache.github_version("latest").await, FALLBACK_VERSION);
        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn test_unreadable_state_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LATEST_FILE), "{nope").unwrap();
        let server = Server::start(|_| (200, release_json("2.338.2").into_bytes())).await;
        let cache = cache(dir.path(), &server.url, 3600);
        assert_eq!(cache.github_version("latest").await, "2.338.2");
    }

    #[tokio::test]
    async fn test_github_sha256_from_latest_or_its_tag() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|path| {
            let version = path.rsplit_once("/tags/v").map_or("2.338.2", |(_, v)| v);
            (200, release_json(version).into_bytes())
        })
        .await;
        let cache = cache(dir.path(), &server.url, 3600);
        cache.github_version("latest").await;
        assert_eq!(server.requests(), 1);

        assert_eq!(
            cache.github_sha256("2.338.2", "linux-x64").await.unwrap(),
            SHA_LINUX
        );
        assert_eq!(server.requests(), 1);

        assert_eq!(
            cache.github_sha256("2.300.0", "linux-x64").await.unwrap(),
            SHA_LINUX
        );
        assert_eq!(server.requests(), 2);

        assert!(matches!(
            cache.github_sha256("2.300.0", "osx-x64").await,
            Err(CacheError::NoChecksum(_))
        ));

        // only looked up once
        cache.github_sha256("2.300.0", "linux-x64").await.unwrap();
        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn test_github_checksum_for_a_vm() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, release_json("2.338.2").into_bytes())).await;
        let reachable = cache(dir.path(), &server.url, 3600);
        let checksum = reachable
            .github_checksum("2.338.2", Platform::Linux, Arch::X64)
            .await;
        assert_eq!(checksum.as_deref(), Some(SHA_LINUX));

        // the VM goes without, like it would have before
        let unreachable = cache(dir.path(), "http://127.0.0.1:1", 3600);
        let checksum = unreachable
            .github_checksum("2.338.2", Platform::Linux, Arch::X64)
            .await;
        assert_eq!(checksum, None);
    }

    #[tokio::test]
    async fn test_download_is_kept_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, b"runner".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("github/linux-x64/runner-1.tar.gz");

        for _ in 0..2 {
            let got = cache
                .cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) })
                .await
                .unwrap();
            assert_eq!(std::fs::read(got).unwrap(), b"runner");
        }
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_checksum_is_not_needed_for_a_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("runner");
        std::fs::write(&path, "runner").unwrap();
        std::fs::write(checksum_path(&path), sha256(b"runner")).unwrap();
        let got = cache
            .cached(path.clone(), "http://127.0.0.1:1", async {
                Err(CacheError::NoChecksum("not asked for".to_string()))
            })
            .await
            .unwrap();
        assert_eq!(got, path);
    }

    /// Download a runner, `damage` the copy and ask for it again. Returns what the file ends up
    /// holding and the requests made.
    async fn after_damage(damage: impl FnOnce(&Path)) -> (Vec<u8>, usize) {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, b"runner".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner.tar.gz");
        let get = || cache.cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) });

        get().await.unwrap();
        damage(&path);
        let got = get().await.unwrap();
        (std::fs::read(got).unwrap(), server.requests())
    }

    #[tokio::test]
    async fn test_corrupt_copy_is_replaced() {
        let damaged = after_damage(|path| std::fs::write(path, "runnex").unwrap()).await;
        assert_eq!(damaged, (b"runner".to_vec(), 2));
    }

    #[tokio::test]
    async fn test_truncated_copy_is_replaced() {
        let damaged = after_damage(|path| std::fs::write(path, "run").unwrap()).await;
        assert_eq!(damaged, (b"runner".to_vec(), 2));
    }

    #[tokio::test]
    async fn test_deleted_copy_is_replaced() {
        let damaged = after_damage(|path| std::fs::remove_file(path).unwrap()).await;
        assert_eq!(damaged, (b"runner".to_vec(), 2));
    }

    #[tokio::test]
    async fn test_copy_without_its_checksum_is_replaced() {
        let damaged = after_damage(|path| std::fs::remove_file(checksum_path(path)).unwrap()).await;
        assert_eq!(damaged, (b"runner".to_vec(), 2));
    }

    #[tokio::test]
    async fn test_intact_copy_is_not_replaced() {
        let untouched = after_damage(|_| {}).await;
        assert_eq!(untouched, (b"runner".to_vec(), 1));
    }

    #[tokio::test]
    async fn test_failing_to_replace_a_corrupt_copy_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (503, Vec::new())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("runner");
        std::fs::write(&path, "runnex").unwrap();
        std::fs::write(checksum_path(&path), sha256(b"runner")).unwrap();

        // never handed out, so the VM downloads the runner itself
        let result = cache
            .cached(path, &server.url, async { Ok(sha256(b"runner")) })
            .await;
        assert!(matches!(result, Err(CacheError::Http(_))));
    }

    #[tokio::test]
    async fn test_failed_download_is_left_alone_for_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (503, Vec::new())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner");
        let get = || cache.cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) });

        assert!(matches!(get().await, Err(CacheError::Http(_))));
        assert!(matches!(get().await, Err(CacheError::RecentlyFailed)));
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_queued_jobs_dont_each_retry_a_failed_download() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (503, Vec::new())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner");

        let get = || cache.cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) });
        let (a, b, c) = tokio::join!(get(), get(), get());
        assert!(a.is_err() && b.is_err() && c.is_err());
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_failed_checksum_lookup_is_left_alone_too() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner");
        let lookups = AtomicUsize::new(0);
        let get = || {
            cache.cached(path.clone(), "http://127.0.0.1:1", async {
                lookups.fetch_add(1, Ordering::SeqCst);
                Err(CacheError::NoChecksum("rate limited".to_string()))
            })
        };

        assert!(matches!(get().await, Err(CacheError::NoChecksum(_))));
        assert!(matches!(get().await, Err(CacheError::RecentlyFailed)));
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_download_is_tried_again_after_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (503, Vec::new())).await;
        let mut cache = cache(dir.path(), "", 3600);
        cache.retry_download_after = Duration::ZERO;
        let path = dir.path().join("linux-x64/runner");
        let get = || cache.cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) });

        assert!(matches!(get().await, Err(CacheError::Http(_))));
        assert!(matches!(get().await, Err(CacheError::Http(_))));
        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn test_good_copy_is_used_after_a_failed_download() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (503, Vec::new())).await;
        let cache = cache(dir.path(), "", 3600);
        let platform = dir.path().join("linux-x64");
        let failed = platform.join("runner-2");
        assert!(
            cache
                .cached(failed, &server.url, async { Ok(sha256(b"runner")) })
                .await
                .is_err()
        );

        let good = platform.join("runner-1");
        std::fs::create_dir_all(&platform).unwrap();
        std::fs::write(&good, "runner").unwrap();
        std::fs::write(checksum_path(&good), sha256(b"runner")).unwrap();
        let got = cache
            .cached(good.clone(), &server.url, async { Ok(sha256(b"runner")) })
            .await
            .unwrap();
        assert_eq!(got, good);
    }

    #[tokio::test]
    async fn test_concurrent_jobs_share_one_download() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, b"runner".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner");

        let get = || cache.cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) });
        let (a, b, c) = tokio::join!(get(), get(), get());
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn test_bad_download_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, b"tampered".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner.tar.gz");

        let err = cache
            .cached(path.clone(), &server.url, async { Ok(sha256(b"runner")) })
            .await
            .unwrap_err();
        assert!(matches!(err, CacheError::ChecksumMismatch { .. }));
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn test_http_error_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (404, b"Not Found".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let path = dir.path().join("linux-x64/runner.tar.gz");

        let err = cache
            .cached(path.clone(), &server.url, async {
                Ok(sha256(b"Not Found"))
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CacheError::Http(_)));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_new_version_replaces_the_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|_| (200, b"runner".to_vec())).await;
        let cache = cache(dir.path(), "", 3600);
        let platform = dir.path().join("github/linux-x64");
        let other_platform = dir.path().join("github/osx-arm64/runner-1.tar.gz");
        std::fs::create_dir_all(other_platform.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&platform).unwrap();
        std::fs::write(&other_platform, "other").unwrap();
        std::fs::write(platform.join("runner-1.tar.gz"), "old").unwrap();
        std::fs::write(platform.join("runner-1.tar.gz.sha256"), "old").unwrap();
        std::fs::write(platform.join("runner-0.part"), "interrupted").unwrap();

        let new = cache
            .cached(platform.join("runner-2.tar.gz"), &server.url, async {
                Ok(sha256(b"runner"))
            })
            .await
            .unwrap();

        let mut kept: Vec<_> = std::fs::read_dir(&platform)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        kept.sort();
        assert_eq!(kept, vec![new.clone(), checksum_path(&new)]);
        assert!(other_platform.exists());
    }

    #[tokio::test]
    async fn test_forgejo_sha256_file() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(|path| {
            assert_eq!(path, "/forgejo-runner.sha256");
            (
                200,
                format!("{}  forgejo-runner\n", "AB".repeat(32)).into_bytes(),
            )
        })
        .await;
        let cache = cache(dir.path(), "", 3600);
        let url = format!("{}/forgejo-runner", server.url);
        assert_eq!(cache.sha256_file(&url).await.unwrap(), "ab".repeat(32));
    }
}
