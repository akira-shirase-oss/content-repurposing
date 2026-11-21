//! The spend port: every model call is admitted against the tenant's and the global
//! cap *before* it starts, recorded as one usage event at its estimate in that same
//! admission, and settled with its real usage after. A call that fails, times out or is
//! killed keeps its estimate: providers bill those too.

use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::model::{BoxFuture, Usage};

/// Prices in micro-dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub input_per_mtok: u64,
    pub output_per_mtok: u64,
}

impl Price {
    /// Claude Haiku 5.5 list price (docs/KNOWLEDGE.md): $0.10 in, $0.50 out.
    pub const HAIKU_5_5: Self = Self {
        input_per_mtok: 100_000,
        output_per_mtok: 500_000,
    };

    /// Cost of `usage` in micro-dollars, rounded up.
    pub fn cost(self, usage: Usage) -> u64 {
        let micros = u128::from(usage.input_tokens) * u128::from(self.input_per_mtok)
            + u128::from(usage.output_tokens) * u128::from(self.output_per_mtok);
        u64::try_from(micros.div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpendError {
    #[error("the spend cap is reached")]
    Exhausted,
    #[error("the spend ledger is unavailable")]
    Unavailable,
}

/// The most a call can use: its input estimate and its output cap, and their price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub usage: Usage,
    pub cost: u64,
}

/// A reservation: `reserved` micro-dollars held until settled, and the usage event that
/// already records the call at its estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub id: u64,
    pub reserved: u64,
    pub event: u128,
}

pub trait Spend: Send + Sync {
    /// Reserves `estimate.cost` micro-dollars and records one usage event for `model` at
    /// the estimate, or refuses (recording nothing) when a cap would be crossed.
    fn admit(
        &self,
        model: &str,
        estimate: Estimate,
    ) -> BoxFuture<'_, Result<Admission, SpendError>>;
    /// Replaces the reservation with the actual cost and settles the call's usage event.
    fn settle(
        &self,
        admission: Admission,
        usage: Usage,
        cost: u64,
    ) -> BoxFuture<'_, Result<(), SpendError>>;
}

/// An in-memory ledger with one cap: tests and the development profile.
#[derive(Debug, Clone)]
pub struct MemorySpend {
    inner: Arc<Mutex<Ledger>>,
}

#[derive(Debug, Default)]
struct Ledger {
    cap: u64,
    spent: u64,
    next: u64,
    events: Vec<(String, Usage, u64)>,
}

impl MemorySpend {
    pub fn with_cap(cap: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Ledger {
                cap,
                ..Ledger::default()
            })),
        }
    }

    pub fn spent(&self) -> u64 {
        self.inner.lock().map_or(0, |l| l.spent)
    }

    /// The usage events recorded so far: one per admitted call, at its settled usage
    /// (or its estimate until settled).
    pub fn events(&self) -> Vec<(String, Usage, u64)> {
        self.inner
            .lock()
            .map(|l| l.events.clone())
            .unwrap_or_default()
    }
}

impl Spend for MemorySpend {
    fn admit(
        &self,
        model: &str,
        estimate: Estimate,
    ) -> BoxFuture<'_, Result<Admission, SpendError>> {
        let result = match self.inner.lock() {
            Ok(mut ledger) => {
                if ledger.spent.saturating_add(estimate.cost) > ledger.cap {
                    Err(SpendError::Exhausted)
                } else {
                    ledger.spent += estimate.cost;
                    ledger
                        .events
                        .push((model.to_owned(), estimate.usage, estimate.cost));
                    ledger.next += 1;
                    Ok(Admission {
                        id: ledger.next,
                        reserved: estimate.cost,
                        event: u128::from(ledger.next),
                    })
                }
            }
            Err(_) => Err(SpendError::Unavailable),
        };
        Box::pin(async move { result })
    }

    fn settle(
        &self,
        admission: Admission,
        usage: Usage,
        cost: u64,
    ) -> BoxFuture<'_, Result<(), SpendError>> {
        let result = match self.inner.lock() {
            Ok(mut ledger) => {
                ledger.spent = ledger
                    .spent
                    .saturating_sub(admission.reserved)
                    .saturating_add(cost);
                let at = usize::try_from(admission.event)
                    .ok()
                    .and_then(|n| n.checked_sub(1));
                match at.and_then(|at| ledger.events.get_mut(at)) {
                    Some(event) => {
                        event.1 = usage;
                        event.2 = cost;
                        Ok(())
                    }
                    None => Err(SpendError::Unavailable),
                }
            }
            Err(_) => Err(SpendError::Unavailable),
        };
        Box::pin(async move { result })
    }
}
