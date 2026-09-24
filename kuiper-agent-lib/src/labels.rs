//! Label matching for capability advertisement and resource selection.
//!
//! A VM agent maps a job's labels to a provider-specific resource (e.g. a Tart
//! image or a Proxmox template VMID) through an ordered list of mappings, and
//! advertises its capabilities to the coordinator as "label sets". Sharing the
//! matching and label-set derivation here keeps that behaviour identical across
//! agents — the in-tree ones and any a third party builds.

/// A label-based mapping rule. The rule matches a job when every label in
/// [`labels`](LabelMapping::labels) is present in the job's labels
/// (case-insensitive).
pub trait LabelMapping {
    /// The labels that must all be present in a job's labels for this rule to match.
    fn labels(&self) -> &[String];

    /// Runners to keep for this mapping in fixed-capacity mode, if configured.
    fn pool(&self) -> Option<u32> {
        None
    }

    /// The resource a runner for this mapping is created from, e.g. an image name or template VMID.
    fn id(&self) -> String;
}

/// Return the first mapping whose capability set covers `job_labels` — i.e. the
/// first mapping that the job both *requests* and is *covered by*:
///
/// - **requests**: the job contains at least one of the mapping's labels that
///   isn't a base label — so a job carrying only base labels (e.g. a
///   fixed-capacity runner created from `agent.labels`, or a webhook job omitting
///   mapping-specific labels) matches no mapping and the caller falls back to its
///   default resource. The coordinator copies this rule to know which set's
///   limits a job uses.
/// - **covered**: every job label is in `base` or in that mapping's labels — so a
///   job needing a label this agent/mapping can't provide is skipped. This
///   mirrors how the coordinator routes (job labels ⊆ an advertised set =
///   `base` + a mapping's labels), without the job having to list every label of
///   the mapping.
///
/// Matching is case-insensitive. `None` means no mapping applies; the caller
/// falls back to its default (e.g. `base_image` / `template_vmid`).
pub fn select_mapping<'a, M: LabelMapping>(
    base: &[String],
    mappings: &'a [M],
    job_labels: &[String],
) -> Option<&'a M> {
    mappings.iter().find(|mapping| {
        let mapping_labels = mapping.labels();

        // The job must request at least one of this mapping's own (non-base) labels.
        let requests = job_labels.iter().any(|jl| {
            !base.iter().any(|b| b.eq_ignore_ascii_case(jl))
                && mapping_labels.iter().any(|ml| ml.eq_ignore_ascii_case(jl))
        });
        if !requests {
            return false;
        }

        // ...and every job label must be covered by base + this mapping.
        job_labels.iter().all(|jl| {
            base.iter().any(|b| b.eq_ignore_ascii_case(jl))
                || mapping_labels.iter().any(|ml| ml.eq_ignore_ascii_case(jl))
        })
    })
}

/// One capability the agent advertises to the coordinator: a label set, the resource behind it and its pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// Lowercased and de-duplicated. A job matches when its labels are a subset
    pub labels: Vec<String>,
    /// The resource runners for this set are created from. Pool commands send it back, since labels alone can't
    /// tell two mappings apart when one's labels cover the other's
    pub id: String,
    /// Fixed-capacity pool size. Setting `pool` on any mapping switches the agent to explicit pools, and sets
    /// without it get 0. With no `pool` anywhere it's `None` everywhere, which tells the coordinator to use the legacy
    /// pool (base labels, `max_vms` runners)
    pub pool: Option<u32>,
    /// The default resource, for jobs that don't ask for any mapping. Always the last capability
    pub is_default: bool,
}

/// Derive the capabilities the agent advertises to the coordinator.
///
/// One per mapping (`base` plus that mapping's labels), so each mapping is a distinct capability the coordinator
/// can route to, then `base` alone for the default resource `base_id`. Jobs with only base labels fall back to the
/// default, so the coordinator needs to know about it too, e.g. for its OS limits.
pub fn capabilities<M: LabelMapping>(
    base: &[String],
    base_id: &str,
    mappings: &[M],
) -> Vec<Capability> {
    let base: Vec<String> = base.iter().map(|l| l.to_lowercase()).collect();
    let explicit = mappings.iter().any(|m| m.pool().is_some());

    mappings
        .iter()
        .map(|mapping| {
            let mut labels = base.clone();
            for label in mapping.labels() {
                let lower = label.to_lowercase();
                if !labels.iter().any(|l| l.eq_ignore_ascii_case(&lower)) {
                    labels.push(lower);
                }
            }
            Capability {
                labels,
                id: mapping.id(),
                pool: explicit.then(|| mapping.pool().unwrap_or(0)),
                is_default: false,
            }
        })
        .chain(std::iter::once(Capability {
            labels: base.clone(),
            id: base_id.to_string(),
            pool: explicit.then_some(0),
            is_default: true,
        }))
        .collect()
}

/// The mapping to create a runner from. A fixed-capacity pool command names its capability's `id`, anything else
/// goes by [`select_mapping`]. `None` means the default resource.
pub fn mapping_for<'a, M: LabelMapping>(
    base: &[String],
    base_id: &str,
    mappings: &'a [M],
    set_id: &str,
    job_labels: &[String],
) -> Option<&'a M> {
    if !set_id.is_empty() {
        if let Some(mapping) = mappings.iter().find(|m| m.id() == set_id) {
            return Some(mapping);
        }
        if set_id == base_id {
            return None;
        }

        // config changed since the coordinator saw this id
        tracing::warn!(
            "Unknown label set id {:?}, selecting by labels {:?}",
            set_id,
            job_labels
        );
    }
    select_mapping(base, mappings, job_labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mapping {
        labels: Vec<String>,
        value: u32,
        pool: Option<u32>,
    }

    impl LabelMapping for Mapping {
        fn labels(&self) -> &[String] {
            &self.labels
        }

        fn pool(&self) -> Option<u32> {
            self.pool
        }

        fn id(&self) -> String {
            self.value.to_string()
        }
    }

    fn mapping(labels: &[&str], value: u32) -> Mapping {
        Mapping {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            value,
            pool: None,
        }
    }

    fn pooled(labels: &[&str], pool: Option<u32>) -> Mapping {
        Mapping {
            pool,
            ..mapping(labels, 0)
        }
    }

    fn labels(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    fn pools(caps: &[Capability]) -> Vec<Option<u32>> {
        caps.iter().map(|c| c.pool).collect()
    }

    #[test]
    fn pools_legacy_without_any_pool() {
        let base = labels(&["self-hosted"]);
        assert_eq!(pools(&capabilities::<Mapping>(&base, "b", &[])), vec![None]);
        let mappings = [pooled(&["macos"], None), pooled(&["linux"], None)];
        assert_eq!(
            pools(&capabilities(&base, "b", &mappings)),
            vec![None, None, None]
        );
    }

    #[test]
    fn pools_explicit_once_any_mapping_has_one() {
        let base = labels(&["self-hosted"]);
        let mappings = [pooled(&["macos"], Some(1)), pooled(&["linux"], None)];
        assert_eq!(
            pools(&capabilities(&base, "b", &mappings)),
            vec![Some(1), Some(0), Some(0)]
        );
    }

    #[test]
    fn select_ignores_mapping_labels_that_are_base_labels() {
        // a base-only job goes to the default even when a mapping repeats a base label
        let base = labels(&["self-hosted"]);
        let mappings = [mapping(&["self-hosted", "windows"], 104)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted"])).map(|m| m.value),
            None
        );
    }

    #[test]
    fn mapping_for_uses_the_id_over_labels() {
        let base = labels(&["self-hosted"]);

        // [linux] alone is covered by the first mapping, so labels can't reach the second
        let mappings = [mapping(&["linux", "noble"], 1), mapping(&["linux"], 2)];
        let job = labels(&["self-hosted", "linux"]);
        let pick = |id: &str| mapping_for(&base, "0", &mappings, id, &job).map(|m| m.value);
        assert_eq!(pick(""), Some(1));
        assert_eq!(pick("2"), Some(2));
        assert_eq!(pick("0"), None);
        assert_eq!(pick("gone"), Some(1));
    }

    #[test]
    fn select_picks_first_mapping_whose_set_covers_the_job() {
        let base = labels(&["self-hosted", "arm64"]);
        let mappings = [mapping(&["windows"], 104), mapping(&["linux"], 9000)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted", "linux"])).map(|m| m.value),
            Some(9000)
        );
    }

    #[test]
    fn select_does_not_require_full_mapping_labels_in_job() {
        // The job omits "debian" but is still covered by base + [linux, debian].
        let base = labels(&["self-hosted", "arm64"]);
        let mappings = [mapping(&["linux", "debian"], 9000)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted", "linux"])).map(|m| m.value),
            Some(9000)
        );
    }

    #[test]
    fn select_still_matches_when_job_specifies_full_labels() {
        let base = labels(&["self-hosted", "arm64"]);
        let mappings = [
            mapping(&["macos", "sequoia"], 1),
            mapping(&["linux", "debian"], 2),
        ];
        assert_eq!(
            select_mapping(
                &base,
                &mappings,
                &labels(&["self-hosted", "linux", "debian"])
            )
            .map(|m| m.value),
            Some(2)
        );
    }

    #[test]
    fn select_base_only_job_matches_no_mapping() {
        // A job carrying only base labels (e.g. a fixed-capacity runner created
        // from agent.labels) must fall back to the default resource, not grab the
        // first mapping just because every label happens to be a base label.
        let base = labels(&["self-hosted", "arm64"]);
        let mappings = [mapping(&["macos", "sequoia"], 1), mapping(&["linux"], 2)];
        assert!(select_mapping(&base, &mappings, &labels(&["self-hosted", "arm64"])).is_none());
        // Same when the job is a strict subset of the base labels.
        assert!(select_mapping(&base, &mappings, &labels(&["self-hosted"])).is_none());
    }

    #[test]
    fn select_returns_first_requested_match_in_order() {
        // When a job requests labels covered by more than one mapping, the first
        // in order wins.
        let base = labels(&["self-hosted"]);
        let mappings = [mapping(&["gpu"], 1), mapping(&["gpu", "cuda"], 2)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted", "gpu"])).map(|m| m.value),
            Some(1)
        );
    }

    #[test]
    fn select_returns_none_when_a_job_label_is_outside_every_set() {
        let base = labels(&["self-hosted"]);
        let mappings = [mapping(&["linux"], 1)];
        // "windows" is in neither base nor the mapping → not covered.
        assert!(select_mapping(&base, &mappings, &labels(&["self-hosted", "windows"])).is_none());
    }

    #[test]
    fn select_is_case_insensitive() {
        let base = labels(&["Self-Hosted"]);
        let mappings = [mapping(&["Linux"], 9000)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted", "LINUX"])).map(|m| m.value),
            Some(9000)
        );
    }

    fn sets(caps: Vec<Capability>) -> Vec<(Vec<String>, String, bool)> {
        caps.into_iter()
            .map(|c| (c.labels, c.id, c.is_default))
            .collect()
    }

    #[test]
    fn capabilities_without_mappings_is_single_base_set() {
        assert_eq!(
            sets(capabilities::<Mapping>(
                &labels(&["Self-Hosted", "X64"]),
                "b",
                &[]
            )),
            vec![(labels(&["self-hosted", "x64"]), "b".to_string(), true)]
        );
    }

    #[test]
    fn capabilities_is_one_set_per_mapping_then_base() {
        let mappings = [mapping(&["Windows"], 104), mapping(&["linux"], 9000)];
        assert_eq!(
            sets(capabilities(&labels(&["self-hosted"]), "b", &mappings)),
            vec![
                (
                    labels(&["self-hosted", "windows"]),
                    "104".to_string(),
                    false
                ),
                (labels(&["self-hosted", "linux"]), "9000".to_string(), false),
                (labels(&["self-hosted"]), "b".to_string(), true),
            ]
        );
    }

    #[test]
    fn capabilities_dedupes_when_mapping_repeats_a_base_label() {
        let mappings = [mapping(&["self-hosted", "windows"], 104)];
        let caps = capabilities(&labels(&["self-hosted"]), "b", &mappings);
        assert_eq!(caps[0].labels, labels(&["self-hosted", "windows"]));
    }
}
