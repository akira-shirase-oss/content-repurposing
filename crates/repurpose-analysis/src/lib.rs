//! Analysis: an extracted article in, key ideas out, each tied to exact source spans.
//!
//! The model sits behind [`model::Model`]; tests and development use
//! [`model::Recorded`]. [`spend::Spend`] admits every call before it is made.

pub mod analyse;
pub mod model;
pub mod spans;
pub mod spend;

pub use analyse::{Analysis, AnalysisError, Idea, Kind, Settings, analyse};
