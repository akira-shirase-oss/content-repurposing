//! The spend ledger in PostgreSQL: per-workspace and global daily caps, checked and
//! reserved in one transaction before a model call, which also records the call as a
//! usage event at its estimate; settled after with a usage settlement. A call killed
//! mid-flight keeps its event at the estimate and its reservation held: never under-billed.

use repurpose_analysis::model::{BoxFuture, Usage};
use repurpose_analysis::spend::{Admission, Estimate, Spend, SpendError};
use sqlx::PgPool;
use uuid::Uuid;

/// Daily caps in micro-dollars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub workspace_daily: i64,
    pub global_daily: i64,
}

impl Default for Caps {
    /// $1 a day per workspace, $50 a day in total: generous for M1's Haiku-priced
    /// analyses (well under a cent each), tight enough that a runaway loop stops early.
    fn default() -> Self {
        Self {
            workspace_daily: 1_000_000, // LIMIT(spend-workspace-daily)
            global_daily: 50_000_000,
        }
    }
}

/// The ledger for one job: its workspace and source.
#[derive(Debug, Clone)]
pub struct PgSpend {
    pub pool: PgPool,
    pub caps: Caps,
    pub workspace: Uuid,
    pub source: Option<Uuid>,
}

fn unavailable(_: sqlx::Error) -> SpendError {
    SpendError::Unavailable
}

impl PgSpend {
    async fn admit_tx(&self, model: &str, est: Estimate) -> Result<Admission, SpendError> {
        let estimate = i64::try_from(est.cost).map_err(|_| SpendError::Exhausted)?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        // The UTC day as days since 2000-01-01 is the admission id: settling hits the same
        // day's rows even across midnight.
        let day: i32 =
            sqlx::query_scalar("SELECT (now() AT TIME ZONE 'utc')::date - DATE '2000-01-01'")
                .fetch_one(&mut *tx)
                .await
                .map_err(unavailable)?;
        // Global row first, then the workspace's: one lock order for every admission.
        for (scope, cap) in [
            (Uuid::nil(), self.caps.global_daily),
            (self.workspace, self.caps.workspace_daily),
        ] {
            sqlx::query(
                "INSERT INTO spend_days (workspace_id, day) VALUES ($1, DATE '2000-01-01' + $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(scope)
            .bind(day)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
            let held: i64 = sqlx::query_scalar(
                "SELECT reserved_micros + spent_micros FROM spend_days \
                 WHERE workspace_id = $1 AND day = DATE '2000-01-01' + $2 FOR UPDATE",
            )
            .bind(scope)
            .bind(day)
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
            if held.saturating_add(estimate) > cap {
                return Err(SpendError::Exhausted);
            }
            sqlx::query(
                "UPDATE spend_days SET reserved_micros = reserved_micros + $3 \
                 WHERE workspace_id = $1 AND day = DATE '2000-01-01' + $2",
            )
            .bind(scope)
            .bind(day)
            .bind(estimate)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }
        let event = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO usage_events (id, workspace_id, source_id, model, input_tokens, output_tokens, cost_micros) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(event)
        .bind(self.workspace)
        .bind(self.source)
        .bind(model)
        .bind(i64::try_from(est.usage.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(est.usage.output_tokens).unwrap_or(i64::MAX))
        .bind(estimate)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Admission {
            id: u64::try_from(day).unwrap_or(0),
            reserved: u64::try_from(estimate).unwrap_or(0),
            event: event.as_u128(),
        })
    }

    async fn settle_tx(
        &self,
        admission: Admission,
        usage: Usage,
        cost: u64,
    ) -> Result<(), SpendError> {
        let reserved = i64::try_from(admission.reserved).unwrap_or(i64::MAX);
        let cost = i64::try_from(cost).unwrap_or(i64::MAX);
        let day = i32::try_from(admission.id).unwrap_or(0);
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        for scope in [Uuid::nil(), self.workspace] {
            sqlx::query(
                "UPDATE spend_days SET reserved_micros = GREATEST(reserved_micros - $3, 0), \
                     spent_micros = spent_micros + $4 \
                 WHERE workspace_id = $1 AND day = DATE '2000-01-01' + $2",
            )
            .bind(scope)
            .bind(day)
            .bind(reserved)
            .bind(cost)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }
        sqlx::query(
            "INSERT INTO usage_settlements (event_id, input_tokens, output_tokens, cost_micros) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::from_u128(admission.event))
        .bind(i64::try_from(usage.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(usage.output_tokens).unwrap_or(i64::MAX))
        .bind(cost)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)
    }
}

impl Spend for PgSpend {
    fn admit(
        &self,
        model: &str,
        estimate: Estimate,
    ) -> BoxFuture<'_, Result<Admission, SpendError>> {
        let model = model.to_owned();
        Box::pin(async move { self.admit_tx(&model, estimate).await })
    }

    fn settle(
        &self,
        admission: Admission,
        usage: Usage,
        cost: u64,
    ) -> BoxFuture<'_, Result<(), SpendError>> {
        Box::pin(self.settle_tx(admission, usage, cost))
    }
}

/// Whether `workspace` (or the service) has spent today's cap: new sources are refused
/// as `budget_exhausted` instead of queued to fail.
pub async fn exhausted(
    pool: &PgPool,
    caps: Caps,
    workspace: Uuid,
) -> Result<bool, repurpose_db::DbError> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT workspace_id, reserved_micros + spent_micros FROM spend_days \
         WHERE workspace_id IN ($1, $2) AND day = (now() AT TIME ZONE 'utc')::date",
    )
    .bind(Uuid::nil())
    .bind(workspace)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().any(|(scope, held)| {
        let cap = if scope.is_nil() {
            caps.global_daily
        } else {
            caps.workspace_daily
        };
        *held >= cap
    }))
}
