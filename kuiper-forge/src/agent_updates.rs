//! Newest agent releases on GitHub, so the admin UI can flag agents running an older version.
//!
//! Each agent crate is released under its own tag, e.g. `kuiper-proxmox-agent-v0.5.0`. A background task polls the
//! repository's releases and keeps the newest version of each.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::Deserialize;
use tracing::{debug, info, warn};

const RELEASES_URL: &str =
    "https://api.github.com/repos/AstroHQ/kuiper-forge/releases?per_page=100";
const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

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
            let wait = match fetch(&client, RELEASES_URL).await {
                Ok(releases) => {
                    info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        latest = ?releases
                            .iter()
                            .map(|(name, r)| format!("{name} {}", r.version))
                            .collect::<Vec<_>>(),
                        "Checked GitHub for agent releases"
                    );
                    self.set(releases);
                    REFRESH_EVERY
                }
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

async fn fetch(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<HashMap<String, AgentRelease>> {
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
    let releases: Vec<Release> = response.json().await?;
    debug!(count = releases.len(), "Fetched GitHub releases");
    Ok(newest_agent_releases(releases))
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
}
