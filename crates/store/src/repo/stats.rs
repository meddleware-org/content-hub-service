//! Counts for the admin dashboard and Grafana.

use serde::Serialize;
use sqlx::PgConnection;

use crate::Result;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct CountBy {
    pub key: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Stats {
    pub tasks_by_status: Vec<CountBy>,
    pub items_by_status: Vec<CountBy>,
    pub posts_by_status: Vec<CountBy>,
    pub instances_by_status: Vec<CountBy>,
    /// Posts per roadmap phase (the phase's internal name; posts linked to no item are `(no phase)`).
    pub posts_by_phase: Vec<CountBy>,
    pub reviews_due: i64,
}

pub async fn stats(conn: &mut PgConnection) -> Result<Stats> {
    let tasks_by_status = sqlx::query_as!(
        CountBy,
        r#"SELECT status AS "key!", count(*) AS "count!" FROM hub.tasks GROUP BY status ORDER BY status"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let items_by_status = sqlx::query_as!(
        CountBy,
        r#"SELECT status AS "key!", count(*) AS "count!" FROM hub.roadmap_items GROUP BY status ORDER BY status"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let posts_by_status = sqlx::query_as!(
        CountBy,
        r#"SELECT status AS "key!", count(*) AS "count!" FROM hub.posts GROUP BY status ORDER BY status"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let instances_by_status = sqlx::query_as!(
        CountBy,
        r#"SELECT status AS "key!", count(*) AS "count!" FROM hub.platform_instances GROUP BY status ORDER BY status"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let posts_by_phase = sqlx::query_as!(
        CountBy,
        r#"SELECT COALESCE(ph.internal_name, '(no phase)') AS "key!", count(*) AS "count!"
             FROM hub.posts p
             LEFT JOIN hub.roadmap_items i ON i.id = p.roadmap_item_id
             LEFT JOIN hub.roadmap_phases ph ON ph.id = i.phase_id
            GROUP BY ph.internal_name, ph.sort_order ORDER BY ph.sort_order NULLS LAST, 1"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let reviews_due = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM hub.tasks WHERE next_review_at <= now()"#)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Stats { tasks_by_status, items_by_status, posts_by_status, instances_by_status, posts_by_phase, reviews_due })
}
