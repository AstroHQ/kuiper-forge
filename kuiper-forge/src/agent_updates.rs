//! Newest agent releases on GitHub, so the admin UI can flag agents running an older version.
//!
//! Each agent crate is released under its own tag, e.g. `kuiper-proxmox-agent-v0.5.0`. A background task polls the
//! repository's releases (every page, since the agents share the repo with the other crates) and keeps the newest
//! version of each.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use semver::Version;
use serde::Deserialize;
use tracing::{debug, info, warn};

const RELEASES_URL: &str =
    "https://api.github.com/repos/AstroHQ/kuiper-forge/releases?per_page=100";
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);
/// Stop following `next` links after this many pages (5,000 releases at 100 a page).
const MAX_PAGES: usize = 50;

/// Newest release of an agent crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRelease {
    pub version: Version,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
}

/// An agent running something older than the newest release.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    pub latest: String,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
}

/// What the releases API returns that we use.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
}

/// Newest agent releases, keyed by crate name. Empty until the first successful check, or when checking is off.
#[derive(Default)]
pub struct AgentUpdates {
    latest: RwLock<HashMap<String, AgentRelease>>,
}

impl AgentUpdates {
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest release when an agent of `agent_type` (e.g. `proxmox`) reporting `version` is behind it. None
    /// when it's current, when its version is unknown or isn't semver, or when no release is known yet.
    pub fn check(&self, agent_type: &str, version: Option<&str>) -> Option<UpdateAvailable> {
        let running = Version::parse(version?.trim_start_matches('v')).ok()?;
        let latest = self.latest.read().unwrap();
        let release = latest.get(&crate_name(agent_type))?;
        (release.version > running).then(|| UpdateAvailable {
            latest: release.version.to_string(),
            url: release.url.clone(),
            published_at: release.published_at,
        })
    }

    fn set(&self, releases: HashMap<String, AgentRelease>) {
        *self.latest.write().unwrap() = releases;
    }

    /// Fetch every page of releases from `url` and replace what's known. When any page fails, what's known stays
    /// as it was, so a partial list never replaces a full one.
    async fn refresh(&self, client: &reqwest::Client, url: &str) -> anyhow::Result<()> {
        let started = Instant::now();
        let releases = fetch(client, url).await?;
        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            latest = ?releases
                .iter()
                .map(|(name, r)| format!("{name} {}", r.version))
                .collect::<Vec<_>>(),
            "Checked GitHub for agent releases"
        );
        self.set(releases);
        Ok(())
    }

    /// Poll GitHub releases forever. Keeps the last good answer when a check fails.
    pub async fn run(self: std::sync::Arc<Self>) {
        let client = match reqwest::Client::builder()
            .user_agent(concat!("kuiper-forge/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Agent update check disabled, couldn't build HTTP client: {e}");
                return;
            }
        };

        loop {
            let started = Instant::now();
            let wait = match self.refresh(&client, RELEASES_URL).await {
                Ok(()) => REFRESH_EVERY,
                Err(e) => {
                    warn!(
                        url = RELEASES_URL,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        retry_in_secs = RETRY_AFTER.as_secs(),
                        "Agent update check failed: {e:#}"
                    );
                    RETRY_AFTER
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// `proxmox` -> `kuiper-proxmox-agent`, matching the release tags.
fn crate_name(agent_type: &str) -> String {
    format!("kuiper-{agent_type}-agent")
}

/// Every release, following GitHub's `Link: rel="next"` pages from `url`. Fails if any page does.
async fn fetch(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<HashMap<String, AgentRelease>> {
    let mut releases = Vec::new();
    let mut next = Some(url.to_owned());
    let mut page = 0;
    while let Some(url) = next.take() {
        page += 1;
        if page > MAX_PAGES {
            warn!(
                max_pages = MAX_PAGES,
                next_url = %url,
                fetched = releases.len(),
                "Stopped following GitHub release pages at the cap; older releases are ignored"
            );
            break;
        }
        let started = Instant::now();
        let (batch, next_url) = fetch_page(client, &url).await.with_context(|| {
            format!(
                "page {page} ({url}) after {}ms",
                started.elapsed().as_millis()
            )
        })?;
        debug!(
            page,
            count = batch.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            has_next = next_url.is_some(),
            "Fetched a page of GitHub releases"
        );
        releases.extend(batch);
        next = next_url;
    }
    Ok(newest_agent_releases(releases))
}

/// One page of releases and the URL of the next page, if any.
async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(Vec<Release>, Option<String>)> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "GitHub returned {status}: {}",
            body.chars().take(200).collect::<String>()
        );
    }
    let next = response
        .headers()
        .get(reqwest::header::LINK)
        .and_then(|v| v.to_str().ok())
        .and_then(next_link);
    let releases: Vec<Release> = response
        .json()
        .await
        .with_context(|| format!("decoding releases ({status})"))?;
    Ok((releases, next))
}

/// The `rel="next"` URL from a `Link` header like `<https://..?page=2>; rel="next", <https://..?page=5>; rel="last"`.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|link| {
        let (target, params) = link.split_once(';')?;
        params
            .split(';')
            .any(|p| {
                let p = p.trim();
                p == r#"rel="next""# || p == "rel=next"
            })
            .then(|| {
                target
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

/// Newest non-draft, non-prerelease version of each `kuiper-*-agent` crate.
fn newest_agent_releases(releases: Vec<Release>) -> HashMap<String, AgentRelease> {
    let mut newest: HashMap<String, AgentRelease> = HashMap::new();
    for r in releases {
        if r.draft || r.prerelease {
            continue;
        }
        let Some((name, version)) = r.tag_name.rsplit_once("-v") else {
            continue;
        };
        if !(name.starts_with("kuiper-") && name.ends_with("-agent")) {
            continue;
        }
        let Ok(version) = Version::parse(version) else {
            continue;
        };
        if !version.pre.is_empty() {
            continue;
        }
        if newest.get(name).is_some_and(|n| n.version >= version) {
            continue;
        }
        newest.insert(
            name.to_owned(),
            AgentRelease {
                version,
                url: r.html_url,
                published_at: r.published_at,
            },
        );
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn releases(json: &str) -> Vec<Release> {
        serde_json::from_str(json).unwrap()
    }

    const RELEASES: &str = r#"[
        {"tag_name": "kuiper-proxmox-agent-v0.6.0-rc.1", "html_url": "u-rc", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-proxmox-agent-v0.5.1", "html_url": "u-draft", "draft": true, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-libvirt-agent-v0.2.0", "html_url": "u-libvirt", "draft": false, "prerelease": false, "published_at": "2026-10-30T13:38:55Z"},
        {"tag_name": "kuiper-proxmox-agent-v0.5.0", "html_url": "u-pve-050", "draft": false, "prerelease": false, "published_at": "2026-10-30T13:38:41Z"},
        {"tag_name": "kuiper-agent-lib-v0.6.0", "html_url": "u-lib", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-forge-v0.8.0", "html_url": "u-forge", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-proxmox-agent-v0.4.3", "html_url": "u-pve-043", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-tart-agent-v0.5.10", "html_url": "u-tart-510", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "kuiper-tart-agent-v0.5.9", "html_url": "u-tart-59", "draft": false, "prerelease": false, "published_at": null},
        {"tag_name": "nightly", "html_url": "u-n", "draft": false, "prerelease": true, "published_at": null}
    ]"#;

    #[test]
    fn test_newest_agent_releases() {
        let newest = newest_agent_releases(releases(RELEASES));
        let mut names: Vec<_> = newest.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "kuiper-libvirt-agent",
                "kuiper-proxmox-agent",
                "kuiper-tart-agent"
            ]
        );
        // drafts and pre-releases are skipped, versions compare as semver, not text
        assert_eq!(newest["kuiper-proxmox-agent"].url, "u-pve-050");
        assert_eq!(newest["kuiper-tart-agent"].version, Version::new(0, 5, 10));
    }

    #[test]
    fn test_check() {
        let updates = AgentUpdates::new();
        // nothing known yet
        assert!(updates.check("proxmox", Some("0.1.0")).is_none());

        updates.set(newest_agent_releases(releases(RELEASES)));
        let update = updates.check("proxmox", Some("0.4.3")).unwrap();
        assert_eq!(update.latest, "0.5.0");
        assert_eq!(update.url, "u-pve-050");

        assert!(updates.check("proxmox", Some("0.5.0")).is_none());
        // a dev build ahead of the newest release
        assert!(updates.check("proxmox", Some("0.6.0")).is_none());
        assert!(updates.check("tart", Some("0.5.9")).is_some());
        assert!(updates.check("proxmox", None).is_none());
        assert!(updates.check("proxmox", Some("garbage")).is_none());
        assert!(updates.check("unknown", Some("0.0.1")).is_none());
    }

    /// A pretend GitHub releases API on a local port. `pages[n]` is served for `?page=n+1` (page 1 when there's no
    /// `page` param) as `(status, body)`, with a `Link: rel="next"` header on every page but the last.
    async fn mock_github(pages: Vec<(u16, String)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let base_for_task = base.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&buf);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let page: usize = path
                    .split(['?', '&'])
                    .find_map(|kv| kv.strip_prefix("page="))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1);
                let (status, body) = pages.get(page - 1).cloned().unwrap_or((404, "{}".into()));
                let link = if page < pages.len() {
                    format!(
                        "Link: <{base_for_task}/releases?per_page=100&page={}>; rel=\"next\", <{base_for_task}/releases?per_page=100&page={}>; rel=\"last\"\r\n",
                        page + 1,
                        pages.len()
                    )
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{link}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("{base}/releases?per_page=100")
    }

    fn release_json(tag: &str) -> String {
        format!(
            r#"[{{"tag_name": "{tag}", "html_url": "u-{tag}", "draft": false, "prerelease": false, "published_at": null}}]"#
        )
    }

    #[tokio::test]
    async fn test_fetch_follows_pagination() {
        // an agent whose only release is past the first page of a busy multi-crate repo
        let url = mock_github(vec![
            (200, release_json("kuiper-tart-agent-v0.6.0")),
            (200, release_json("kuiper-forge-v0.8.0")),
            (200, release_json("kuiper-proxmox-agent-v0.5.0")),
        ])
        .await;
        let client = reqwest::Client::new();
        let newest = fetch(&client, &url).await.unwrap();
        assert_eq!(newest["kuiper-tart-agent"].version, Version::new(0, 6, 0));
        assert_eq!(
            newest
                .get("kuiper-proxmox-agent")
                .map(|r| r.version.clone()),
            Some(Version::new(0, 5, 0)),
            "release on page 3 was missed"
        );
    }

    #[tokio::test]
    async fn test_failed_page_keeps_last_good_releases() {
        let updates = AgentUpdates::new();
        updates.set(newest_agent_releases(releases(RELEASES)));

        let url = mock_github(vec![
            (200, release_json("kuiper-proxmox-agent-v0.9.0")),
            (500, "boom".into()),
        ])
        .await;
        let err = updates
            .refresh(&reqwest::Client::new(), &url)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("page 2"), "{err:#}");
        // still the old answer, not page 1 on its own
        assert_eq!(
            updates.check("proxmox", Some("0.4.3")).unwrap().latest,
            "0.5.0"
        );
        assert!(updates.check("tart", Some("0.5.9")).is_some());

        // a page that isn't a release list fails the same way
        let url = mock_github(vec![
            (200, release_json("kuiper-proxmox-agent-v0.9.0")),
            (200, r#"{"message": "nope"}"#.into()),
        ])
        .await;
        assert!(
            updates
                .refresh(&reqwest::Client::new(), &url)
                .await
                .is_err()
        );
        assert_eq!(
            updates.check("proxmox", Some("0.4.3")).unwrap().latest,
            "0.5.0"
        );
    }

    #[tokio::test]
    async fn test_refresh_replaces_releases() {
        let updates = AgentUpdates::new();
        updates.set(newest_agent_releases(releases(RELEASES)));
        let url = mock_github(vec![
            (200, release_json("kuiper-forge-v0.9.0")),
            (200, release_json("kuiper-proxmox-agent-v0.9.0")),
        ])
        .await;
        updates
            .refresh(&reqwest::Client::new(), &url)
            .await
            .unwrap();
        assert_eq!(
            updates.check("proxmox", Some("0.5.0")).unwrap().latest,
            "0.9.0"
        );
    }

    #[test]
    fn test_next_link() {
        let header = r#"<https://api.github.com/repositories/1/releases?per_page=100&page=2>; rel="next", <https://api.github.com/repositories/1/releases?per_page=100&page=4>; rel="last""#;
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://api.github.com/repositories/1/releases?per_page=100&page=2")
        );
        let last_page =
            r#"<https://x/releases?page=1>; rel="prev", <https://x/releases?page=1>; rel="first""#;
        assert_eq!(next_link(last_page), None);
        assert_eq!(next_link(""), None);
    }
}
