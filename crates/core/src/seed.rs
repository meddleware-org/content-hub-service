//! The instance roadmap seed (`schema/roadmap-seed.schema.json`): types and validation. The `seed` subcommand parses
//! the operator's YAML or JSON file into these types, validates it here, and upserts it by key in one transaction.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::ident::{is_key, is_platform_code};
use crate::model::{ItemStatus, ItemType, TaskStatus, TaskType};
use crate::task_state::has_dependency_cycle;

/// The roadmap seed file: phases, items and tasks that start a new instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedFile {
    /// The version of the file's format (only 1 is understood).
    pub schema_version: u32,
    /// The phases, in the order the roadmap shows them.
    pub phases: Vec<SeedPhase>,
    /// The roadmap items, each in one phase.
    pub items: Vec<SeedItem>,
    /// The tasks, each optionally attached to an item.
    pub tasks: Vec<SeedTask>,
}

/// One roadmap phase.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedPhase {
    /// The phase's stable key; items name it.
    pub key: String,
    /// Its position in the roadmap (positive, unique).
    pub order: i32,
    /// The name the operator sees.
    pub internal_name: String,
    /// The description the operator sees.
    pub internal_description: String,
    /// The name the public roadmap shows.
    pub public_name: String,
    /// The description the public roadmap shows.
    pub public_description: String,
    /// When the phase starts, if it is dated.
    #[serde(default)]
    pub window_start: Option<DateTime<Utc>>,
    /// When the phase ends, if it is dated.
    #[serde(default)]
    pub window_end: Option<DateTime<Utc>>,
    /// Whether people reading the roadmap in a browser see the phase.
    pub visible_human: bool,
    /// Whether the machine-readable feeds carry the phase.
    pub visible_bot: bool,
}

/// One roadmap item.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedItem {
    /// The item's stable key; tasks name it.
    pub key: String,
    /// The key of the phase it belongs to.
    pub phase: String,
    /// What kind of item it is.
    #[serde(rename = "type")]
    pub item_type: ItemType,
    /// The title the operator sees.
    pub internal_title: String,
    /// The description the operator sees.
    pub internal_description: String,
    /// The title the public roadmap shows, when it differs from the internal one.
    #[serde(default)]
    pub public_title: Option<String>,
    /// The description the public roadmap shows.
    #[serde(default)]
    pub public_description: Option<String>,
    /// Whether people reading the roadmap in a browser see the item.
    pub visible_human: bool,
    /// Whether the machine-readable feeds carry the item.
    pub visible_bot: bool,
    /// Where the item stands.
    pub status: ItemStatus,
    /// When the item is planned to start.
    #[serde(default)]
    pub planned_at: Option<DateTime<Utc>>,
    /// When the item is due.
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// The due date the public roadmap shows (it may be later than the internal one).
    #[serde(default)]
    pub public_due_at: Option<DateTime<Utc>>,
}

/// One operator task.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedTask {
    /// The task's stable key; other tasks name it in `depends_on`.
    pub key: String,
    /// The key of the item it belongs to, if any.
    #[serde(default)]
    pub item: Option<String>,
    /// What kind of task it is.
    pub task_type: TaskType,
    /// The task's title.
    pub title: String,
    /// The task's description.
    #[serde(default)]
    pub description: String,
    /// The codes of the platforms the task is about.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Names of the instance's platform targets (linked when a target of that name exists on one of the platforms).
    #[serde(default)]
    pub targets: Vec<String>,
    /// Where the task stands.
    pub status: TaskStatus,
    /// When the task is scheduled.
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
    /// How many days pass between reviews of a recurring task.
    #[serde(default)]
    pub review_interval_days: Option<i32>,
    /// The keys of the tasks that must be done first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Whether the task waits for the legal entity to exist.
    #[serde(default)]
    pub entity_gated: bool,
}

/// Whether the number of characters in `s` is between `min` and `max`.
fn len_ok(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.chars().count())
}

/// Every problem with a seed (empty when valid). `known_codes` are the platform codes the database holds.
pub fn validate(seed: &SeedFile, known_codes: &BTreeSet<String>) -> Vec<String> {
    // Every problem is collected, so the operator sees them all at once.
    let mut p = Vec::new();
    if seed.schema_version != 1 {
        p.push(format!("schema_version {} is not supported (1)", seed.schema_version));
    }
    // Phase keys seen so far, to catch a duplicate.
    let mut phase_keys = BTreeSet::new();
    // Positions seen so far, to catch a duplicate.
    let mut orders = BTreeSet::new();
    for ph in &seed.phases {
        let k = &ph.key;
        if !is_key(k) {
            p.push(format!("phase {k}: not a key"));
        }
        if !phase_keys.insert(k.as_str()) {
            p.push(format!("phase {k}: duplicate key"));
        }
        if ph.order < 1 || !orders.insert(ph.order) {
            p.push(format!("phase {k}: order must be positive and unique"));
        }
        if !len_ok(&ph.internal_name, 1, 200) || !len_ok(&ph.public_name, 1, 200) {
            p.push(format!("phase {k}: names must be 1 to 200 characters"));
        }
        if !len_ok(&ph.internal_description, 0, 4000) || !len_ok(&ph.public_description, 0, 4000) {
            p.push(format!("phase {k}: descriptions must be at most 4000 characters"));
        }
        if let (Some(a), Some(b)) = (ph.window_start, ph.window_end)
            && b < a
        {
            p.push(format!("phase {k}: window_end is before window_start"));
        }
    }
    let mut item_keys = BTreeSet::new();
    for it in &seed.items {
        let k = &it.key;
        if !is_key(k) {
            p.push(format!("item {k}: not a key"));
        }
        if !item_keys.insert(k.as_str()) {
            p.push(format!("item {k}: duplicate key"));
        }
        if !phase_keys.contains(it.phase.as_str()) {
            p.push(format!("item {k}: unknown phase {}", it.phase));
        }
        if !len_ok(&it.internal_title, 1, 200) || !len_ok(&it.internal_description, 0, 4000) {
            p.push(format!("item {k}: internal title 1-200, description at most 4000 characters"));
        }
        if (it.visible_human || it.visible_bot) && (it.public_title.is_none() || it.public_description.is_none()) {
            p.push(format!("item {k}: a visible item needs public_title and public_description"));
        }
        if it.public_title.as_deref().is_some_and(|t| !len_ok(t, 1, 200))
            || it.public_description.as_deref().is_some_and(|d| !len_ok(d, 0, 4000))
        {
            p.push(format!("item {k}: public title 1-200, description at most 4000 characters"));
        }
    }
    let mut task_keys = BTreeSet::new();
    let mut edges = Vec::new();
    for t in &seed.tasks {
        let k = &t.key;
        if !is_key(k) {
            p.push(format!("task {k}: not a key"));
        }
        if !task_keys.insert(k.as_str()) {
            p.push(format!("task {k}: duplicate key"));
        }
        if let Some(i) = &t.item
            && !item_keys.contains(i.as_str())
        {
            p.push(format!("task {k}: unknown item {i}"));
        }
        if !len_ok(&t.title, 1, 200) || !len_ok(&t.description, 0, 4000) {
            p.push(format!("task {k}: title 1-200, description at most 4000 characters"));
        }
        for c in &t.platforms {
            if !is_platform_code(c) || !known_codes.contains(c) {
                p.push(format!("task {k}: unknown platform {c}"));
            }
        }
        if t.review_interval_days.is_some_and(|d| !(1..=3650).contains(&d)) {
            p.push(format!("task {k}: review_interval_days must be 1 to 3650"));
        }
        for d in &t.depends_on {
            edges.push((k.clone(), d.clone()));
        }
    }
    for (k, d) in &edges {
        if !task_keys.contains(d.as_str()) {
            p.push(format!("task {k}: depends on unknown task {d}"));
        }
    }
    if has_dependency_cycle(&edges) {
        p.push("tasks: the dependencies contain a cycle".to_owned());
    }
    p
}

/// Platform code -> keys of the tasks that use it (for the import report).
pub fn platform_usage(seed: &SeedFile) -> BTreeMap<&str, usize> {
    let mut m = BTreeMap::new();
    for t in &seed.tasks {
        for c in &t.platforms {
            *m.entry(c.as_str()).or_insert(0) += 1;
        }
    }
    m
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;

    /// A small valid seed: one phase, one item and two tasks, the second depending on the first.
    const OK: &str = r#"{"schema_version": 1,
      "phases": [{"key": "p1", "order": 1, "internal_name": "Internal", "internal_description": "", "public_name": "Public",
                  "public_description": "", "visible_human": true, "visible_bot": false}],
      "items": [{"key": "i1", "phase": "p1", "type": "engineering", "internal_title": "t", "internal_description": "",
                 "public_title": "T", "public_description": "d", "visible_human": true, "visible_bot": false, "status": "planned",
                 "public_due_at": "2027-01-01T12:00:00Z"}],
      "tasks": [{"key": "a", "item": "i1", "task_type": "ENGINEERING", "title": "A", "status": "planned", "platforms": ["OWNED_FEED"]},
                {"key": "b", "task_type": "OTHER", "title": "B", "status": "done", "depends_on": ["a"]}]}"#;

    /// The platform codes the database is pretended to hold.
    fn codes() -> BTreeSet<String> {
        ["OWNED_FEED".to_owned()].into()
    }

    /// A valid seed has no problems.
    #[test]
    fn a_valid_seed_has_no_problems() {
        let s: SeedFile = serde_json::from_str(OK).expect("parses");
        assert_eq!(validate(&s, &codes()), Vec::<String>::new());
        assert_eq!(platform_usage(&s)["OWNED_FEED"], 1);
    }

    /// Every problem is reported.
    #[test]
    fn every_problem_is_reported() {
        let mut s: SeedFile = serde_json::from_str(OK).expect("parses");
        s.schema_version = 2;
        s.items[0].public_title = None;
        s.items[0].phase = "nope".into();
        s.tasks[0].platforms.push("TIKTOK".into());
        s.tasks[0].depends_on.push("b".into());
        s.tasks[1].depends_on.push("ghost".into());
        let all = validate(&s, &codes()).join("\n");
        for needle in [
            "schema_version 2",
            "unknown phase nope",
            "needs public_title",
            "unknown platform TIKTOK",
            "unknown task ghost",
            "cycle",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
    }

    /// Unknown fields are refused.
    #[test]
    fn unknown_fields_are_refused() {
        let bad = OK.replacen(r#""order": 1,"#, r#""order": 1, "secret": true,"#, 1);
        assert!(serde_json::from_str::<SeedFile>(&bad).is_err());
    }
}
