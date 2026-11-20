//! M1's product slice: a pasted URL becomes a source; a worker fetches, extracts and
//! analyses it; its key ideas are stored with the exact spans they quote.

pub mod routes;
pub mod spend;
pub mod store;
pub mod worker;

pub use routes::{Sources, router};
pub use worker::{Outcome, Worker, WorkerSettings};
