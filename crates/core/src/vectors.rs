//! Generate the golden vectors (`vectors.json`) from the authoritative Rust implementation.
//!
//! `cargo run -p content-hub-core --example gen-vectors > vectors.json` writes the file; it is then copied to
//! `@meddleware/content-hub-client` (its published home) and released there first. A core test regenerates it and
//! compares it with the committed copy, so a change to the text engine cannot ship without new vectors.
//!
//! Positions are code-point offsets. Every input is in `crates/core/vectors-input.json`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::instance_state::{self, Actor};
use crate::model::{CountingMethod, InstanceStatus, Markup, TaskStatus, stored_enumerations};
use crate::task_state;
use crate::text::count::{CountRules, count};
use crate::text::limits::{Limits, check, for_target};
use crate::text::render::render_source;
use crate::text::segment::{SegmentTarget, segment};

/// The committed inputs.
pub const INPUT_JSON: &str = include_str!("../vectors-input.json");

/// One counting rule set in the input file.
#[derive(Deserialize)]
struct RuleIn {
    /// How characters are counted.
    method: CountingMethod,
    /// What a link counts for, when the method gives links a fixed weight.
    #[serde(default)]
    url_weight: Option<u32>,
    /// Whether a mention counts only its local part.
    #[serde(default)]
    mention_local_part: bool,
}

/// One segmentation case in the input file.
#[derive(Deserialize)]
struct SegIn {
    /// The source text to split.
    source: String,
    /// The destinations: name, markup, counting method and limit.
    targets: Vec<(String, Markup, CountingMethod, u64)>,
}

/// A definition's limits, a target's configuration and a post, to be resolved and checked.
#[derive(Deserialize)]
struct LimitIn {
    /// The markup the destination renders.
    markup: Markup,
    /// How the destination counts, if it limits length.
    #[serde(default)]
    counting_method: Option<CountingMethod>,
    /// The destination's main limit.
    #[serde(default)]
    primary_limit: Option<i64>,
    /// Its further limits by name.
    #[serde(default)]
    secondary_limits: BTreeMap<String, Value>,
    /// The target's own configuration.
    #[serde(default)]
    target_config: Option<Value>,
    /// The post title.
    title: String,
    /// The post body.
    body: String,
}

/// The whole input file.
#[derive(Deserialize)]
struct Input {
    /// Counting cases, as text.
    counting: Vec<String>,
    /// Counting rules.
    count_rules: Vec<RuleIn>,
    /// Rendering cases, as text.
    rendering: Vec<String>,
    /// The markups to render each case in.
    markups: Vec<Markup>,
    /// Segmentation cases.
    segmentation: Vec<SegIn>,
    /// Limit checks.
    limits: Vec<LimitIn>,
}

/// The full vectors document.
pub fn generate() -> Value {
    // The inputs are committed in a file; a broken file stops generation at once.
    let input: Input = serde_json::from_str(INPUT_JSON).expect("vectors-input.json is valid");

    let enumerations: serde_json::Map<String, Value> =
        // The enumerations come from the one list the store's parity test also uses.
        stored_enumerations().into_iter().map(|(t, c, v)| (format!("{t}.{c}"), json!(v))).collect();
    let mut task_transitions = Vec::new();
    for &f in TaskStatus::ALL {
        for &t in TaskStatus::ALL {
            // Every allowed task move, found by trying every pair.
            if task_state::edge_exists(f, t) {
                task_transitions.push(json!([f, t]));
            }
        }
    }
    let mut instance_transitions = Vec::new();
    for &f in InstanceStatus::ALL {
        for &t in InstanceStatus::ALL {
            // Every allowed instance move, with the actor that may make it.
            for (a, name) in [(Actor::Operator, "operator"), (Actor::Worker, "worker"), (Actor::Janitor, "janitor")] {
                if instance_state::allowed(a, f, t) {
                    instance_transitions.push(json!([f, t, name]));
                }
            }
        }
    }

    let mut counting = Vec::new();
    // Every rule set against every counting text.
    for r in &input.count_rules {
        let rules = CountRules { method: r.method, url_weight: r.url_weight, mention_local_part: r.mention_local_part };
        for text in &input.counting {
            counting.push(json!({
                "method": r.method,
                "url_weight": r.url_weight,
                "mention_local_part": r.mention_local_part,
                "text": text,
                "count": count(&rules, text),
            }));
        }
    }

    let mut rendering = Vec::new();
    // Every source rendered in every markup.
    for source in &input.rendering {
        for &m in &input.markups {
            let r = render_source(m, source);
            rendering.push(json!({ "markup": m, "source": source, "payload": r.payload, "visible": r.visible }));
        }
    }

    let mut segmentation = Vec::new();
    for case in &input.segmentation {
        let targets: Vec<SegmentTarget> = case
            .targets
            .iter()
            .map(|(id, markup, method, limit)| SegmentTarget {
                id: id.clone(),
                markup: *markup,
                limits: Limits { rules: CountRules::plain(*method), primary: Some(*limit), bytes: None, title: None },
            })
            .collect();
        // Positions in the output are code-point offsets, so the TypeScript side can compare them directly.
        let seg = segment(&case.source, &targets);
        segmentation.push(json!({
            "source": case.source,
            "targets": case.targets.iter().map(|(id, m, c, l)| json!({"id": id, "markup": m, "method": c, "limit": l})).collect::<Vec<_>>(),
            "cuts": seg.cuts.iter().map(|(id, c)| json!([id, c])).collect::<Vec<_>>(),
            "bands": seg.bands.iter().map(|b| json!({"start": b.start, "end": b.end, "fits": b.fits})).collect::<Vec<_>>(),
        }));
    }

    let mut limits = Vec::new();
    for case in &input.limits {
        let config = case.target_config.clone().unwrap_or_else(|| json!({}));
        let base = Limits::from_definition(case.counting_method, case.primary_limit, &case.secondary_limits);
        // The limits as one target resolves them: the definition's, adjusted by the target's own configuration.
        let resolved = for_target(base, &case.secondary_limits, &config);
        let input_json = json!({
            "markup": case.markup,
            "counting_method": case.counting_method,
            "primary_limit": case.primary_limit,
            "secondary_limits": case.secondary_limits,
            "target_config": config,
            "title": case.title,
            "body": case.body,
        });
        let result = match resolved {
            // A platform with no limits still has a rendered payload.
            None => json!({ "limits": null, "payload": render_source(case.markup, &case.body).payload }),
            Some(l) => {
                let c = check(case.markup, &l, Some(&case.title), &case.body);
                json!({
                    "limits": {
                        "method": l.rules.method, "url_weight": l.rules.url_weight,
                        "mention_local_part": l.rules.mention_local_part,
                        "primary": l.primary, "bytes": l.bytes, "title": l.title,
                    },
                    "payload": c.payload, "counted": c.counted, "bytes": c.bytes,
                    "title_counted": c.title_counted, "fits": c.fits(),
                })
            }
        };
        limits.push(json!({ "input": input_json, "result": result }));
    }

    json!({
        "schema_version": 1,
        "description": "Golden vectors shared by @meddleware/content-hub-client (TypeScript) and content-hub-service (Rust). Generated by the Rust implementation (cargo run -p content-hub-core --example gen-vectors); published from the client package; the service checks its copy against the published file. Positions are code-point offsets.",
        "enumerations": enumerations,
        "task_transitions": task_transitions,
        "instance_transitions": instance_transitions,
        "counting": counting,
        "rendering": rendering,
        "segmentation": segmentation,
        "limits": limits,
    })
}

/// The document as written to disk (two-space JSON and a final newline).
pub fn generate_text() -> String {
    let mut s = serde_json::to_string_pretty(&generate()).expect("serialisable");
    s.push('\n');
    s
}
