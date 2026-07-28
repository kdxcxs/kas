use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use kas_core::{DriverObservation, DriverSpec, DriverWatch, ManifestSelector, Resource};

/// Returns whether a running Driver currently owns or watches a Resource.
///
/// Invalid Driver documents are treated as non-matching. Store validation is
/// responsible for rejecting them before they become persistent.
pub(crate) fn driver_matches_resource(driver: &Resource, resource: &Resource) -> bool {
    if driver.metadata.state != "running" {
        return false;
    }
    serde_json::from_value::<DriverSpec>(driver.spec.clone())
        .is_ok_and(|spec| driver_spec_matches_resource(&spec, resource))
}

pub(crate) fn driver_spec_matches_resource(spec: &DriverSpec, resource: &Resource) -> bool {
    spec.manages
        .iter()
        .any(|manifest| manifest == &resource.manifest)
        || spec.watches.iter().any(|watch| watch.matches(resource))
}

/// Computes the Manifest partitions whose observations may have changed after
/// replacing one Driver document with another.
///
/// Exact Manifest selectors are resolved directly and therefore don't scan the
/// registry. Glob selectors are expanded only over the supplied Manifest
/// registry. Callers can then use the `manifest_path` database index to update
/// Resources in these partitions instead of scanning every Resource.
pub(crate) fn affected_manifest_paths(
    old: Option<&Resource>,
    new: Option<&Resource>,
    manifest_registry: &[String],
) -> BTreeSet<String> {
    let old = DriverSnapshot::from_resource(old);
    let new = DriverSnapshot::from_resource(new);

    match (&old, &new) {
        (None, None) => BTreeSet::new(),
        (Some(snapshot), None) | (None, Some(snapshot)) => {
            snapshot.selected_manifests(manifest_registry)
        }
        (Some(old), Some(new)) if old.revision != new.revision => old
            .selected_manifests(manifest_registry)
            .into_iter()
            .chain(new.selected_manifests(manifest_registry))
            .collect(),
        (Some(old), Some(new)) => {
            let candidates = old
                .candidate_manifests(manifest_registry)
                .into_iter()
                .chain(new.candidate_manifests(manifest_registry))
                .collect::<BTreeSet<_>>();
            candidates
                .into_iter()
                .filter(|manifest| old.effect_on(manifest) != new.effect_on(manifest))
                .collect()
        }
    }
}

#[derive(Debug)]
struct DriverSnapshot {
    revision: u64,
    spec: DriverSpec,
}

impl DriverSnapshot {
    fn from_resource(resource: Option<&Resource>) -> Option<Self> {
        let resource = resource?;
        if resource.metadata.state != "running" {
            return None;
        }
        Some(Self {
            revision: resource.revision,
            spec: serde_json::from_value(resource.spec.clone()).ok()?,
        })
    }

    fn selected_manifests(&self, registry: &[String]) -> BTreeSet<String> {
        self.candidate_manifests(registry)
            .into_iter()
            .filter(|manifest| self.effect_on(manifest).is_some())
            .collect()
    }

    fn candidate_manifests(&self, registry: &[String]) -> BTreeSet<String> {
        let mut manifests = self.spec.manages.iter().cloned().collect::<BTreeSet<_>>();
        for watch in &self.spec.watches {
            expand_manifest_selector(&watch.manifest, registry, &mut manifests);
        }
        manifests
    }

    fn effect_on(&self, manifest: &str) -> Option<ManifestEffect> {
        let manages = self.spec.manages.iter().any(|managed| managed == manifest);
        let watches = self
            .spec
            .watches
            .iter()
            .filter(|watch| watch.manifest.matches(manifest))
            .map(normalized_watch_paths)
            .collect::<BTreeSet<_>>();
        (manages || !watches.is_empty()).then_some(ManifestEffect { manages, watches })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ManifestEffect {
    manages: bool,
    watches: BTreeSet<Vec<String>>,
}

fn normalized_watch_paths(watch: &DriverWatch) -> Vec<String> {
    let mut paths = watch.paths.clone();
    paths.sort();
    paths.dedup();
    paths
}

fn expand_manifest_selector(
    selector: &ManifestSelector,
    registry: &[String],
    output: &mut BTreeSet<String>,
) {
    let patterns = match selector {
        ManifestSelector::One(pattern) => std::slice::from_ref(pattern),
        ManifestSelector::Many(patterns) => patterns.as_slice(),
    };
    for pattern in patterns {
        if pattern.contains('*') {
            output.extend(
                registry
                    .iter()
                    .filter(|manifest| kas_core::resource_path_matches(pattern, manifest))
                    .cloned(),
            );
        } else {
            output.insert(pattern.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueueKey {
    driver_path: String,
    resource_path: String,
    driver_revision: u64,
    resource_revision: u64,
}

#[derive(Debug, Default)]
pub(crate) struct ReconcileQueue {
    pending: BTreeMap<String, VecDeque<QueueKey>>,
    latest: HashMap<(String, String), QueueKey>,
    resource_drivers: HashMap<String, HashSet<String>>,
    owners: BTreeMap<String, String>,
}

impl ReconcileQueue {
    pub(crate) fn refresh_drivers(&mut self, drivers: &[Resource]) {
        self.owners.clear();
        for driver in drivers {
            if driver.metadata.state != "running" {
                continue;
            }
            let Ok(spec) = serde_json::from_value::<DriverSpec>(driver.spec.clone()) else {
                continue;
            };
            for manifest in spec.manages {
                self.owners.insert(manifest, driver.path.clone());
            }
        }
    }

    pub(crate) fn rebuild(&mut self, drivers: &[Resource], resources: &[Resource]) {
        self.pending.clear();
        self.latest.clear();
        self.resource_drivers.clear();
        self.refresh_drivers(drivers);
        for resource in resources {
            self.schedule(resource);
        }
    }

    pub(crate) fn schedule(&mut self, resource: &Resource) -> Vec<String> {
        let document_drifted = resource.metadata_without_observed()
            != resource.status_metadata_without_observed()
            || resource.spec != resource.status.spec;
        let owner = self.owners.get(&resource.manifest);
        let previous_drivers = self
            .resource_drivers
            .remove(&resource.path)
            .unwrap_or_default();
        let expected_drivers = resource
            .metadata
            .kas
            .observed
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        for driver_path in previous_drivers.difference(&expected_drivers) {
            self.latest
                .remove(&(driver_path.clone(), resource.path.clone()));
        }
        self.resource_drivers
            .insert(resource.path.clone(), expected_drivers);

        let mut notified = Vec::new();
        for (driver_path, expected) in &resource.metadata.kas.observed {
            let actual = resource.status.metadata.kas.observed.get(driver_path);
            let pair = (driver_path.clone(), resource.path.clone());
            if actual == Some(expected) && !(document_drifted && owner == Some(driver_path)) {
                self.latest.remove(&pair);
                continue;
            }
            let key = QueueKey {
                driver_path: driver_path.clone(),
                resource_path: resource.path.clone(),
                driver_revision: expected.driver_revision,
                resource_revision: expected.resource_revision,
            };
            if self.latest.get(&pair) != Some(&key) {
                self.latest.insert(pair, key.clone());
                self.pending
                    .entry(driver_path.clone())
                    .or_default()
                    .push_back(key);
                notified.push(driver_path.clone());
            }
        }
        notified
    }

    pub(crate) fn pop(&mut self, driver_path: &str) -> Option<(String, DriverObservation)> {
        loop {
            let queue = self.pending.get_mut(driver_path)?;
            let key = queue.pop_front()?;
            if queue.is_empty() {
                self.pending.remove(driver_path);
            }
            let pair = (key.driver_path.clone(), key.resource_path.clone());
            if self.latest.get(&pair) != Some(&key) {
                continue;
            }
            self.latest.remove(&pair);
            return Some((
                key.resource_path,
                DriverObservation {
                    driver_revision: key.driver_revision,
                    resource_revision: key.resource_revision,
                },
            ));
        }
    }

    pub(crate) fn is_pending(
        &self,
        resource: &Resource,
        driver_path: &str,
        expected: &DriverObservation,
    ) -> bool {
        if resource.metadata.kas.observed.get(driver_path) != Some(expected) {
            return false;
        }
        let observed = resource.status.metadata.kas.observed.get(driver_path);
        if observed != Some(expected) {
            return true;
        }
        self.owners.get(&resource.manifest).map(String::as_str) == Some(driver_path)
            && (resource.metadata_without_observed() != resource.status_metadata_without_observed()
                || resource.spec != resource.status.spec)
    }

    pub(crate) fn remove_resource(&mut self, resource_path: &str) {
        if let Some(drivers) = self.resource_drivers.remove(resource_path) {
            for driver_path in drivers {
                self.latest
                    .remove(&(driver_path, resource_path.to_string()));
            }
        }
    }
}

trait ResourceDocuments {
    fn metadata_without_observed(&self) -> serde_json::Value;
    fn status_metadata_without_observed(&self) -> serde_json::Value;
}

impl ResourceDocuments for Resource {
    fn metadata_without_observed(&self) -> serde_json::Value {
        let mut metadata = self.metadata.clone();
        metadata.kas.observed.clear();
        serde_json::to_value(metadata).unwrap_or_default()
    }

    fn status_metadata_without_observed(&self) -> serde_json::Value {
        let mut metadata = self.status.metadata.clone();
        metadata.kas.observed.clear();
        serde_json::to_value(metadata).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kas_core::{KasMetadata, ResourceMetadata, ResourceStatus, ResourceStatusMetadata};
    use serde_json::json;

    fn driver(
        revision: u64,
        state: &str,
        manages: &[&str],
        watches: serde_json::Value,
    ) -> Resource {
        Resource {
            path: "/drivers/example".into(),
            metadata: ResourceMetadata {
                manifest: "/builtin/driver".into(),
                name: "example".into(),
                state: state.into(),
                kas: KasMetadata {
                    revision,
                    ..Default::default()
                },
            },
            spec: json!({
                "runtime": "process",
                "entrypoint": "bin/example",
                "service_account": "/service-accounts/example",
                "manages": manages,
                "watches": watches,
                "restart": "always"
            }),
            status: ResourceStatus::default(),
        }
    }

    fn resource() -> Resource {
        let observation = DriverObservation {
            driver_revision: 2,
            resource_revision: 3,
        };
        Resource {
            path: "/resources/example".into(),
            metadata: ResourceMetadata {
                manifest: "/manifests/example".into(),
                name: "example".into(),
                state: "available".into(),
                kas: KasMetadata {
                    revision: 3,
                    observed: BTreeMap::from([("/drivers/watcher".into(), observation.clone())]),
                    ..Default::default()
                },
            },
            spec: json!({"value": 1}),
            status: ResourceStatus {
                metadata: ResourceStatusMetadata {
                    manifest: "/manifests/example".into(),
                    name: "example".into(),
                    state: "available".into(),
                    kas: KasMetadata::default(),
                },
                spec: json!({"value": 1}),
            },
        }
    }

    #[test]
    fn queues_only_drifted_observations_and_deduplicates() {
        let mut queue = ReconcileQueue::default();
        let resource = resource();
        assert_eq!(queue.schedule(&resource), vec!["/drivers/watcher"]);
        assert!(queue.schedule(&resource).is_empty());
        let (path, observation) = queue.pop("/drivers/watcher").unwrap();
        assert_eq!(path, resource.path);
        assert_eq!(observation.resource_revision, 3);
        assert!(queue.pop("/drivers/watcher").is_none());
        assert_eq!(queue.schedule(&resource), vec!["/drivers/watcher"]);
    }

    #[test]
    fn exact_watch_delta_does_not_depend_on_manifest_registry() {
        let old = driver(
            7,
            "running",
            &[],
            json!([{"manifest": "/manifests/agent", "paths": ["/agents/old/**"]}]),
        );
        let new = driver(
            7,
            "running",
            &[],
            json!([{"manifest": "/manifests/agent", "paths": ["/agents/new/**"]}]),
        );

        assert_eq!(
            affected_manifest_paths(Some(&old), Some(&new), &[]),
            BTreeSet::from(["/manifests/agent".to_owned()])
        );
    }

    #[test]
    fn wildcard_watch_delta_expands_only_registered_manifests() {
        let new = driver(
            1,
            "running",
            &[],
            json!([{"manifest": "/manifests/**", "paths": []}]),
        );
        let registry = vec![
            "/manifests/agent".to_owned(),
            "/manifests/message".to_owned(),
            "/builtin/link".to_owned(),
        ];

        assert_eq!(
            affected_manifest_paths(None, Some(&new), &registry),
            BTreeSet::from([
                "/manifests/agent".to_owned(),
                "/manifests/message".to_owned()
            ])
        );
    }

    #[test]
    fn manages_watch_and_running_state_changes_are_incremental() {
        let registry = vec![
            "/manifests/agent".to_owned(),
            "/manifests/message".to_owned(),
            "/manifests/thread".to_owned(),
        ];
        let stopped = driver(
            3,
            "stopped",
            &["/manifests/agent"],
            json!([{"manifest": "/manifests/message"}]),
        );
        let running = driver(
            3,
            "running",
            &["/manifests/agent"],
            json!([{"manifest": "/manifests/message"}]),
        );
        assert_eq!(
            affected_manifest_paths(Some(&stopped), Some(&running), &registry),
            BTreeSet::from([
                "/manifests/agent".to_owned(),
                "/manifests/message".to_owned()
            ])
        );

        let changed = driver(
            3,
            "running",
            &["/manifests/thread"],
            json!([{"manifest": "/manifests/message"}]),
        );
        assert_eq!(
            affected_manifest_paths(Some(&running), Some(&changed), &registry),
            BTreeSet::from([
                "/manifests/agent".to_owned(),
                "/manifests/thread".to_owned()
            ])
        );
    }

    #[test]
    fn revision_change_reconciles_the_union_of_matched_partitions() {
        let old = driver(
            10,
            "running",
            &["/manifests/agent"],
            json!([{"manifest": "/manifests/message", "paths": ["/messages/**"]}]),
        );
        let new = driver(
            11,
            "running",
            &["/manifests/agent"],
            json!([{"manifest": "/manifests/message", "paths": ["/messages/**"]}]),
        );

        assert_eq!(
            affected_manifest_paths(Some(&old), Some(&new), &[]),
            BTreeSet::from([
                "/manifests/agent".to_owned(),
                "/manifests/message".to_owned()
            ])
        );
    }

    #[test]
    fn driver_matching_includes_managed_and_watched_resources() {
        let running = driver(
            4,
            "running",
            &["/manifests/agent"],
            json!([{
                "manifest": "/manifests/message",
                "paths": ["/resources/example"]
            }]),
        );
        let mut managed = resource();
        managed.metadata.manifest = "/manifests/agent".into();
        assert!(driver_matches_resource(&running, &managed));

        let mut watched = resource();
        watched.metadata.manifest = "/manifests/message".into();
        assert!(driver_matches_resource(&running, &watched));

        let stopped = driver(
            4,
            "stopped",
            &["/manifests/agent"],
            json!([{"manifest": "*"}]),
        );
        assert!(!driver_matches_resource(&stopped, &managed));
    }
}
