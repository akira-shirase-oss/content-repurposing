//! The spend port: every model call is admitted against the tenant's and the global
//! cap *before* it starts, and settled with its real usage after.

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

/// A reservation: `reserved` micro-dollars held until settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub id: u64,
    pub reserved: u64,
}

pub trait Spend: Send + Sync {
    /// Reserves `estimate` micro-dollars, or refuses when a cap would be crossed.
    fn admit(&self, estimate: u64) -> BoxFuture<'_, Result<Admission, SpendError>>;
    /// Replaces the reservation with the actual cost and records one usage event.
    fn settle(
        &self,
        admission: Admission,
        model: &str,
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

    /// The usage events recorded so far: one per settled call.
    pub fn events(&self) -> Vec<(String, Usage, u64)> {
        self.inner
            .lock()
            .map(|l| l.events.clone())
            .unwrap_or_default()
    }
}

impl Spend for MemorySpend {
    fn admit(&self, estimate: u64) -> BoxFuture<'_, Result<Admission, SpendError>> {
        let result = match self.inner.lock() {
            Ok(mut ledger) => {
                if ledger.spent.saturating_add(estimate) > ledger.cap {
                    Err(SpendError::Exhausted)
                } else {
                    ledger.spent += estimate;
                    ledger.next += 1;
                    Ok(Admission {
                        id: ledger.next,
                        reserved: estimate,
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
        model: &str,
        usage: Usage,
        cost: u64,
    ) -> BoxFuture<'_, Result<(), SpendError>> {
        let result = match self.inner.lock() {
            Ok(mut ledger) => {
                ledger.spent = ledger
                    .spent
                    .saturating_sub(admission.reserved)
                    .saturating_add(cost);
                ledger.events.push((model.to_owned(), usage, cost));
                Ok(())
            }
            Err(_) => Err(SpendError::Unavailable),
        };
        Box::pin(async move { result })
    }
}
