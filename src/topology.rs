//! Fleet topology file: the upstream set described as nodes and their replicas.
//!
//! A single-box deployment names its two or three engines in `RJ_UPSTREAM` and
//! a handful of parallel comma lists. At ten or twenty nodes those lists are
//! forty entries long, must stay index-aligned by hand, and say nothing about
//! which replicas share a machine. `RJ_TOPOLOGY_FILE` replaces them with one
//! JSON document:
//!
//! ```json
//! {"nodes": [
//!   {"name": "h200-01", "replicas": [
//!     {"url": "http://10.0.0.11:8070", "model": "glm-5.3"},
//!     {"url": "http://10.0.0.11:8071", "model": "glm-5.3"}
//!   ]},
//!   {"name": "h200-02", "replicas": [
//!     {"url": "http://10.0.0.12:8070", "model": "glm-5.3"}
//!   ]}
//! ]}
//! ```
//!
//! The file expands into exactly the settings it replaces, so every existing
//! per-upstream validation still applies and upstream ordinals follow file
//! order. Setting both the file and one of the lists it owns is an error rather
//! than a merge: two sources for one list is how a fleet silently renders a
//! different topology than the one reviewed.

use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;

/// Settings the topology file owns; the environment may not also set them.
pub const OWNED_KEYS: [&str; 6] = [
    "RJ_UPSTREAM",
    "RJ_UPSTREAM_MODELS",
    "RJ_UPSTREAM_APIS",
    "RJ_UPSTREAM_DP_RANKS",
    "RJ_ROUTE_SPECULATION_PROFILES",
    "RJ_ROUTE_KV_CAPACITY_TOKENS",
];

/// Bounds a hand-written file; also caps the per-upstream metric series.
const MAX_REPLICAS: usize = 256;
const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_NODE_NAME: usize = 63;
const MAX_DP_RANKS: u32 = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Topology {
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Stable machine name; becomes the `node` label of `ramjet_upstream_info`.
    pub name: String,
    pub replicas: Vec<Replica>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Replica {
    pub url: String,
    /// Served model ID; set on every replica or none (`RJ_UPSTREAM_MODELS`).
    pub model: Option<String>,
    /// API profile, `openai` when unset (`RJ_UPSTREAM_APIS`).
    pub api: Option<String>,
    /// Set on every replica or none (`RJ_ROUTE_SPECULATION_PROFILES`).
    pub speculation_profile: Option<String>,
    /// KV pool size; unknown when unset (`RJ_ROUTE_KV_CAPACITY_TOKENS`). With
    /// `dp_ranks`, the pool of one rank.
    pub kv_capacity_tokens: Option<u64>,
    /// `SGLang` data-parallel attention size. Each rank keeps its own KV
    /// cache, so the replica expands into this many upstreams that share the
    /// URL and pin requests with `routed_dp_rank` (`RJ_UPSTREAM_DP_RANKS`).
    pub dp_ranks: Option<u32>,
}

impl Replica {
    /// Upstreams this replica expands into, with the rank each one pins.
    fn ranks(&self) -> Vec<Option<u32>> {
        self.dp_ranks
            .map_or_else(|| vec![None], |ranks| (0..ranks).map(Some).collect())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TopologyError {
    #[error("cannot read topology file {path}: {reason}")]
    Read { path: String, reason: String },
    #[error("invalid topology file {path}: {reason}")]
    Invalid { path: String, reason: String },
}

impl Topology {
    /// Reads and validates a topology file.
    ///
    /// # Errors
    ///
    /// Returns [`TopologyError`] when the file is unreadable, oversized, not the
    /// documented JSON shape, or structurally inconsistent.
    pub fn load(path: &str) -> Result<Self, TopologyError> {
        let read = |reason: String| TopologyError::Read {
            path: path.to_owned(),
            reason,
        };
        let size = fs::metadata(Path::new(path))
            .map_err(|error| read(error.to_string()))?
            .len();
        if size > MAX_FILE_BYTES {
            return Err(read(format!("larger than {MAX_FILE_BYTES} bytes")));
        }
        let raw = fs::read_to_string(path).map_err(|error| read(error.to_string()))?;
        Self::parse(&raw).map_err(|reason| TopologyError::Invalid {
            path: path.to_owned(),
            reason,
        })
    }

    /// Parses and validates topology JSON.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when the document is invalid.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let topology: Self = serde_json::from_str(raw).map_err(|error| error.to_string())?;
        topology.validate()?;
        Ok(topology)
    }

    fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("no nodes".to_owned());
        }
        let mut names = HashSet::new();
        let mut urls = HashSet::new();
        for node in &self.nodes {
            let valid_name = !node.name.is_empty()
                && node.name.len() <= MAX_NODE_NAME
                && node
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
            if !valid_name {
                return Err(format!(
                    "node name {:?} must be 1-{MAX_NODE_NAME} of [A-Za-z0-9._-]",
                    node.name
                ));
            }
            if !names.insert(node.name.as_str()) {
                return Err(format!("duplicate node {:?}", node.name));
            }
            if node.replicas.is_empty() {
                return Err(format!("node {:?} has no replicas", node.name));
            }
            for replica in &node.replicas {
                let url = replica.url.trim().trim_end_matches('/');
                if !urls.insert(url) {
                    return Err(format!("duplicate replica url {url:?}"));
                }
                let fields = [
                    Some(replica.url.as_str()),
                    replica.model.as_deref(),
                    replica.api.as_deref(),
                    replica.speculation_profile.as_deref(),
                ];
                if fields
                    .into_iter()
                    .flatten()
                    .any(|field| field.contains(','))
                {
                    return Err(format!("replica {url:?} has a field containing a comma"));
                }
                if replica
                    .dp_ranks
                    .is_some_and(|ranks| !(1..=MAX_DP_RANKS).contains(&ranks))
                {
                    return Err(format!("replica {url:?} dp_ranks must be 1-{MAX_DP_RANKS}"));
                }
            }
        }
        let upstreams = self.upstreams().count();
        if upstreams > MAX_REPLICAS {
            return Err(format!("{upstreams} upstreams exceeds {MAX_REPLICAS}"));
        }
        let replicas = self.replicas().count();
        for (field, set) in [
            (
                "model",
                self.replicas().filter(|r| r.model.is_some()).count(),
            ),
            (
                "speculation_profile",
                self.replicas()
                    .filter(|r| r.speculation_profile.is_some())
                    .count(),
            ),
        ] {
            if set != 0 && set != replicas {
                return Err(format!("set {field} on every replica or on none"));
            }
        }
        Ok(())
    }

    fn replicas(&self) -> impl Iterator<Item = &Replica> {
        self.nodes.iter().flat_map(|node| &node.replicas)
    }

    /// Every upstream in ordinal order: its node, replica, and pinned rank.
    fn upstreams(&self) -> impl Iterator<Item = (&Node, &Replica, Option<u32>)> {
        self.nodes.iter().flat_map(|node| {
            node.replicas.iter().flat_map(move |replica| {
                replica
                    .ranks()
                    .into_iter()
                    .map(move |rank| (node, replica, rank))
            })
        })
    }

    /// Node name of every upstream, in upstream-ordinal order.
    #[must_use]
    pub fn upstream_nodes(&self) -> Vec<String> {
        self.upstreams()
            .map(|(node, _, _)| node.name.clone())
            .collect()
    }

    /// The value this topology supplies for one of [`OWNED_KEYS`], or `None`
    /// where the setting keeps its unset default.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<String> {
        let each = |value: fn(&Replica, Option<u32>) -> String| {
            Some(
                self.upstreams()
                    .map(|(_, replica, rank)| value(replica, rank))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };
        let any = |field: fn(&Replica) -> bool| self.replicas().any(field);
        match key {
            "RJ_UPSTREAM" => each(|r, _| r.url.clone()),
            "RJ_UPSTREAM_MODELS" if any(|r| r.model.is_some()) => {
                each(|r, _| r.model.clone().unwrap_or_default())
            }
            "RJ_UPSTREAM_APIS" if any(|r| r.api.is_some()) => {
                each(|r, _| r.api.clone().unwrap_or_else(|| "openai".to_owned()))
            }
            "RJ_UPSTREAM_DP_RANKS" if any(|r| r.dp_ranks.is_some()) => {
                each(|_, rank| rank.map_or_else(|| "-".to_owned(), |rank| rank.to_string()))
            }
            "RJ_ROUTE_SPECULATION_PROFILES" if any(|r| r.speculation_profile.is_some()) => {
                each(|r, _| r.speculation_profile.clone().unwrap_or_default())
            }
            "RJ_ROUTE_KV_CAPACITY_TOKENS" if any(|r| r.kv_capacity_tokens.is_some()) => {
                each(|r, _| {
                    r.kv_capacity_tokens
                        .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string())
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_NODES: &str = r#"{"nodes": [
        {"name": "h200-01", "replicas": [
            {"url": "http://10.0.0.11:8070", "model": "glm-5.3", "kv_capacity_tokens": 2050000},
            {"url": "http://10.0.0.11:8071/", "model": "glm-5.3"}
        ]},
        {"name": "h200-02", "replicas": [
            {"url": "http://10.0.0.12:8070", "model": "qwen", "api": "openai"}
        ]}
    ]}"#;

    #[test]
    fn expands_into_the_lists_it_replaces_in_file_order() {
        let topology = Topology::parse(TWO_NODES).unwrap();
        assert_eq!(
            topology.lookup("RJ_UPSTREAM").unwrap(),
            "http://10.0.0.11:8070,http://10.0.0.11:8071/,http://10.0.0.12:8070"
        );
        assert_eq!(
            topology.lookup("RJ_UPSTREAM_MODELS").unwrap(),
            "glm-5.3,glm-5.3,qwen"
        );
        assert_eq!(
            topology.lookup("RJ_UPSTREAM_APIS").unwrap(),
            "openai,openai,openai"
        );
        assert_eq!(
            topology.lookup("RJ_ROUTE_KV_CAPACITY_TOKENS").unwrap(),
            "2050000,-,-"
        );
        assert_eq!(topology.lookup("RJ_ROUTE_SPECULATION_PROFILES"), None);
        assert_eq!(topology.lookup("RJ_UPSTREAM_DP_RANKS"), None);
        assert_eq!(topology.lookup("RJ_ROUTE_ALPHA"), None);
        assert_eq!(topology.upstream_nodes(), ["h200-01", "h200-01", "h200-02"]);
    }

    #[test]
    fn a_data_parallel_replica_expands_into_one_upstream_per_rank() {
        let topology = Topology::parse(
            r#"{"nodes": [
                {"name": "h200-01", "replicas": [
                    {"url": "http://10.0.0.11:8073", "model": "glm-5.3", "dp_ranks": 3,
                     "kv_capacity_tokens": 350000}]},
                {"name": "h200-02", "replicas": [
                    {"url": "http://10.0.0.12:8073", "model": "glm-5.3"}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            topology.lookup("RJ_UPSTREAM").unwrap(),
            "http://10.0.0.11:8073,http://10.0.0.11:8073,http://10.0.0.11:8073,http://10.0.0.12:8073"
        );
        assert_eq!(topology.lookup("RJ_UPSTREAM_DP_RANKS").unwrap(), "0,1,2,-");
        assert_eq!(
            topology.lookup("RJ_ROUTE_KV_CAPACITY_TOKENS").unwrap(),
            "350000,350000,350000,-"
        );
        assert_eq!(
            topology.upstream_nodes(),
            ["h200-01", "h200-01", "h200-01", "h200-02"]
        );
        for ranks in [0, 65] {
            let raw = format!(
                r#"{{"nodes": [{{"name": "a", "replicas": [{{"url": "http://x", "dp_ranks": {ranks}}}]}}]}}"#
            );
            assert!(Topology::parse(&raw).unwrap_err().contains("dp_ranks"));
        }
    }

    #[test]
    fn rejects_inconsistent_or_ambiguous_documents() {
        for (raw, reason) in [
            (r#"{"nodes": []}"#, "no nodes"),
            (
                r#"{"nodes": [{"name": "a", "replicas": []}]}"#,
                "no replicas",
            ),
            (
                r#"{"nodes": [{"name": "a b", "replicas": [{"url": "http://x"}]}]}"#,
                "node name",
            ),
            (
                r#"{"nodes": [{"name": "a", "replicas": [{"url": "http://x"}]},
                              {"name": "a", "replicas": [{"url": "http://y"}]}]}"#,
                "duplicate node",
            ),
            (
                r#"{"nodes": [{"name": "a", "replicas": [{"url": "http://x"}, {"url": "http://x/"}]}]}"#,
                "duplicate replica",
            ),
            (
                r#"{"nodes": [{"name": "a", "replicas": [{"url": "http://x", "model": "m"}, {"url": "http://y"}]}]}"#,
                "every replica or on none",
            ),
            (
                r#"{"nodes": [{"name": "a", "replicas": [{"url": "http://x", "model": "a,b"}]}]}"#,
                "comma",
            ),
            (
                r#"{"nodes": [{"name": "a", "replicas": [{"url": "http://x", "weight": 2}]}]}"#,
                "unknown field",
            ),
        ] {
            let error = Topology::parse(raw).unwrap_err();
            assert!(error.contains(reason), "{raw}: {error}");
        }
    }

    #[test]
    fn load_reports_missing_and_oversized_files() {
        assert!(matches!(
            Topology::load("/nonexistent/topology.json"),
            Err(TopologyError::Read { .. })
        ));
    }
}
