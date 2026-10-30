//! The shared golden vectors (this repository's copy of `@meddleware/content-hub-client/vectors.json`; the
//! contract job checks the copy against the published file). Every Rust table must reproduce them exactly.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::instance_state::{self, Actor};
use crate::model::{InstanceStatus, TaskStatus, stored_enumerations};
use crate::task_state;

const VECTORS: &str = include_str!("../../../vectors.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    schema_version: u32,
    #[allow(dead_code)]
    description: String,
    enumerations: BTreeMap<String, Vec<String>>,
    task_transitions: Vec<(String, String)>,
    instance_transitions: Vec<(String, String, String)>,
    counting: Vec<serde_json::Value>,
    rendering: Vec<serde_json::Value>,
    segmentation: Vec<serde_json::Value>,
    #[allow(dead_code)]
    limits: Vec<serde_json::Value>,
}

fn vectors() -> Vectors {
    serde_json::from_str(VECTORS).expect("vectors.json parses")
}

fn actor(s: &str) -> Actor {
    match s {
        "operator" => Actor::Operator,
        "worker" => Actor::Worker,
        "janitor" => Actor::Janitor,
        other => panic!("unknown actor {other}"),
    }
}

#[test]
fn enumerations_match_value_for_value_and_in_order() {
    let v = vectors();
    assert_eq!(v.schema_version, 1);
    let ours: BTreeMap<String, Vec<String>> = stored_enumerations()
        .into_iter()
        .map(|(t, c, vals)| (format!("{t}.{c}"), vals.into_iter().map(str::to_owned).collect()))
        .collect();
    assert_eq!(ours, v.enumerations);
}

#[test]
fn the_task_state_machine_is_exactly_the_vector_table() {
    let v = vectors();
    let mut ours = Vec::new();
    for &from in TaskStatus::ALL {
        for &to in TaskStatus::ALL {
            if task_state::edge_exists(from, to) {
                ours.push((from.as_str().to_owned(), to.as_str().to_owned()));
            }
        }
    }
    let mut theirs = v.task_transitions.clone();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

#[test]
fn the_instance_state_machine_is_exactly_the_vector_table() {
    let v = vectors();
    for &a in Actor::ALL {
        for &from in InstanceStatus::ALL {
            for &to in InstanceStatus::ALL {
                let listed = v
                    .instance_transitions
                    .iter()
                    .any(|(f, t, x)| f == from.as_str() && t == to.as_str() && actor(x) == a);
                assert_eq!(instance_state::allowed(a, from, to), listed, "{a:?}: {from} -> {to}");
            }
        }
    }
}

#[test]
fn the_committed_vectors_are_what_the_implementation_generates() {
    let v = vectors();
    assert!(!v.counting.is_empty() && !v.rendering.is_empty() && !v.segmentation.is_empty());
    assert!(
        VECTORS == crate::vectors::generate_text(),
        "vectors.json is stale: cargo run -p content-hub-core --example gen-vectors > vectors.json, then copy it to content-hub-client and publish that first"
    );
}
