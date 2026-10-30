//! The seed import: the generic platform definitions and an instance roadmap seed, upserted by code and key in one
//! transaction. Validation happens first (`content_hub_core::seed::validate`); this module only writes.

use std::collections::BTreeMap;

use content_hub_core::platforms::DefinitionsFile;
use content_hub_core::seed::SeedFile;
use serde::Serialize;
use sqlx::PgConnection;

use super::{audit, items, phases, platforms, tasks};
use crate::Result;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SeedReport {
    pub definitions_inserted: usize,
    pub definitions_refreshed: usize,
    pub phases: usize,
    pub items: usize,
    pub tasks: usize,
    /// Target names a task mentions that do not exist (yet) on its platforms.
    pub unmatched_targets: Vec<String>,
}

/// Insert or refresh every generic definition.
pub async fn apply_definitions(tx: &mut PgConnection, file: &DefinitionsFile, report: &mut SeedReport) -> Result<()> {
    for d in &file.definitions {
        let s = |v: &BTreeMap<String, serde_json::Value>| serde_json::to_value(v).unwrap_or_default();
        let inserted = platforms::upsert_generic(
            tx,
            &platforms::SeedDefinition {
                code: &d.code,
                name: &d.name,
                category: d.category.as_str(),
                posting_mode: d.posting_mode.as_str(),
                counting_method: d.counting_method.map(|m| m.as_str()),
                primary_limit: d.primary_limit.and_then(|l| i32::try_from(l).ok()),
                secondary_limits: s(&d.secondary_limits),
                markup: d.markup.as_str(),
                supports_media: d.supports_media,
                base_url: d.base_url.as_deref(),
                url_templates: serde_json::to_value(&d.url_templates).unwrap_or_default(),
                cost_model: s(&d.cost_model),
                enabled: d.enabled,
                notes: &d.notes,
            },
        )
        .await?;
        if inserted {
            report.definitions_inserted += 1;
        } else {
            report.definitions_refreshed += 1;
        }
    }
    Ok(())
}

/// Upsert a validated instance seed.
pub async fn apply_seed(tx: &mut PgConnection, seed: &SeedFile, actor: &str, report: &mut SeedReport) -> Result<()> {
    let mut phase_ids = BTreeMap::new();
    for p in &seed.phases {
        let row = phases::upsert(
            tx,
            &phases::PhaseFields {
                key: p.key.clone(),
                sort_order: p.order,
                internal_name: p.internal_name.clone(),
                internal_description: p.internal_description.clone(),
                public_name: p.public_name.clone(),
                public_description: p.public_description.clone(),
                window_start: p.window_start,
                window_end: p.window_end,
                visible_human: p.visible_human,
                visible_bot: p.visible_bot,
            },
        )
        .await?;
        phase_ids.insert(p.key.as_str(), row.id);
        report.phases += 1;
    }
    let mut item_ids = BTreeMap::new();
    for i in &seed.items {
        let row = items::upsert(
            tx,
            &items::ItemFields {
                phase_id: phase_ids[i.phase.as_str()],
                key: i.key.clone(),
                item_type: i.item_type.as_str().to_owned(),
                internal_title: i.internal_title.clone(),
                internal_description: i.internal_description.clone(),
                public_title: i.public_title.clone(),
                public_description: i.public_description.clone(),
                visible_human: i.visible_human,
                visible_bot: i.visible_bot,
                status: i.status.as_str().to_owned(),
                planned_at: i.planned_at,
                due_at: i.due_at,
                public_due_at: i.public_due_at,
            },
        )
        .await?;
        item_ids.insert(i.key.as_str(), row.id);
        report.items += 1;
    }
    let mut task_ids = BTreeMap::new();
    for t in &seed.tasks {
        let row = tasks::upsert(
            tx,
            &tasks::TaskFields {
                roadmap_item_id: t.item.as_deref().map(|k| item_ids[k]),
                key: t.key.clone(),
                task_type: t.task_type.as_str().to_owned(),
                title: t.title.clone(),
                description: t.description.clone(),
                scheduled_at: t.scheduled_at,
                review_interval_days: t.review_interval_days,
                next_review_at: t
                    .review_interval_days
                    .map(|d| chrono::Utc::now() + chrono::Duration::days(i64::from(d))),
                assignee_subject: None,
                entity_gated: t.entity_gated,
            },
            t.status.as_str(),
        )
        .await?;
        task_ids.insert(t.key.as_str(), row.id);
        report.tasks += 1;
    }
    for t in &seed.tasks {
        let id = task_ids[t.key.as_str()];
        let deps: Vec<i64> = t.depends_on.iter().map(|d| task_ids[d.as_str()]).collect();
        tasks::set_dependencies(tx, id, &deps).await?;
        let mut pairs = Vec::new();
        for code in &t.platforms {
            let Some(def) = platforms::id_by_code(tx, code).await? else { continue };
            pairs.push((def, None));
        }
        for name in &t.targets {
            let found = sqlx::query!(
                "SELECT id, platform_definition_id FROM hub.platform_targets WHERE name = $1 AND platform_definition_id = ANY($2::bigint[])",
                name,
                &pairs.iter().map(|(d, _)| *d).collect::<Vec<_>>()
            )
            .fetch_optional(&mut *tx)
            .await?;
            match found {
                Some(r) => pairs.push((r.platform_definition_id, Some(r.id))),
                None => report.unmatched_targets.push(format!("{}: {name}", t.key)),
            }
        }
        tasks::set_platforms(tx, id, &pairs).await?;
    }
    audit::record(
        tx,
        actor,
        "seed.import",
        "seed",
        None,
        None,
        Some(serde_json::json!({ "phases": report.phases, "items": report.items, "tasks": report.tasks })),
    )
    .await?;
    Ok(())
}
