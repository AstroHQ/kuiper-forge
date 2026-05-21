//! Shared label matching for capability advertisement and resource selection.
//!
//! Both agents map a job's labels to a provider-specific resource (a Tart image
//! or a Proxmox template VMID) through an ordered list of mappings, and
//! advertise their capabilities to the coordinator as "label sets". Keeping the
//! matching and label-set derivation here ensures the agents can't drift apart
//! (e.g. one advertising per-mapping capabilities and the other only a single
//! set).

/// A label-based mapping rule. The rule matches a job when every label in
/// [`labels`](LabelMapping::labels) is present in the job's labels
/// (case-insensitive).
pub trait LabelMapping {
    /// The labels that must all be present in a job's labels for this rule to match.
    fn labels(&self) -> &[String];
}

/// Return the first mapping whose capability set covers `job_labels` — i.e. the
/// first mapping where every job label is present in `base` or in that mapping's
/// labels (case-insensitive).
///
/// This mirrors how the coordinator routes (a job matches an agent when its
/// labels are a subset of an advertised set = `base` + a mapping's labels), so
/// any job routed here resolves to the mapping behind the set it matched —
/// without the job having to list every label of that mapping. `None` means no
/// mapping covers the job; the caller falls back to its default resource.
pub fn select_mapping<'a, M: LabelMapping>(
    base: &[String],
    mappings: &'a [M],
    job_labels: &[String],
) -> Option<&'a M> {
    mappings.iter().find(|mapping| {
        job_labels.iter().all(|jl| {
            base.iter().any(|b| b.eq_ignore_ascii_case(jl))
                || mapping
                    .labels()
                    .iter()
                    .any(|ml| ml.eq_ignore_ascii_case(jl))
        })
    })
}

/// Derive the capability label sets the agent advertises to the coordinator.
///
/// With no mappings, the agent advertises a single set: `base`. With mappings,
/// it advertises one set per mapping — `base` plus that mapping's labels — so
/// each mapping is a distinct capability the coordinator can route to. All
/// labels are lowercased and de-duplicated (case-insensitive). The coordinator
/// matches a job to the agent when the job's labels are a subset of ANY set.
pub fn label_sets<M: LabelMapping>(base: &[String], mappings: &[M]) -> Vec<Vec<String>> {
    let base: Vec<String> = base.iter().map(|l| l.to_lowercase()).collect();

    if mappings.is_empty() {
        return vec![base];
    }

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
            labels
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mapping {
        labels: Vec<String>,
        value: u32,
    }

    impl LabelMapping for Mapping {
        fn labels(&self) -> &[String] {
            &self.labels
        }
    }

    fn mapping(labels: &[&str], value: u32) -> Mapping {
        Mapping {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            value,
        }
    }

    fn labels(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
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
    fn select_returns_first_match_in_order() {
        // A bare base-only job is covered by the first mapping's set.
        let base = labels(&["self-hosted"]);
        let mappings = [mapping(&["macos", "sequoia"], 1), mapping(&["linux"], 2)];
        assert_eq!(
            select_mapping(&base, &mappings, &labels(&["self-hosted"])).map(|m| m.value),
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

    #[test]
    fn label_sets_without_mappings_is_single_base_set() {
        assert_eq!(
            label_sets::<Mapping>(&labels(&["Self-Hosted", "X64"]), &[]),
            vec![vec!["self-hosted".to_string(), "x64".to_string()]]
        );
    }

    #[test]
    fn label_sets_with_mappings_is_one_set_per_mapping() {
        let mappings = [mapping(&["Windows"], 104), mapping(&["linux"], 9000)];
        let sets = label_sets(&labels(&["self-hosted"]), &mappings);
        assert_eq!(
            sets,
            vec![
                vec!["self-hosted".to_string(), "windows".to_string()],
                vec!["self-hosted".to_string(), "linux".to_string()],
            ]
        );
    }

    #[test]
    fn label_sets_dedupes_when_mapping_repeats_a_base_label() {
        let mappings = [mapping(&["self-hosted", "windows"], 104)];
        let sets = label_sets(&labels(&["self-hosted"]), &mappings);
        assert_eq!(
            sets,
            vec![vec!["self-hosted".to_string(), "windows".to_string()]]
        );
    }
}
