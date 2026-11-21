//! The span checker: every idea must point at exact text in the extracted source.
//!
//! The model is asked for verbatim quotes, never offsets; offsets are computed here by
//! exact byte matching against the source, and [`check_span`] re-validates a stored
//! span on every read. A quote that is not in the source is refused, never fuzzily
//! matched.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Shortest quote accepted, in bytes: shorter quotes match by coincidence.
pub const MIN_QUOTE_BYTES: usize = 8;
/// Longest quote accepted, in bytes.
pub const MAX_QUOTE_BYTES: usize = 1000;

/// A half-open byte range `[start, end)` of the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpanError {
    #[error("the quote is shorter than the minimum")]
    TooShort,
    #[error("the quote is longer than the maximum")]
    TooLong,
    #[error("the span lies outside the source")]
    OutOfBounds,
    #[error("the span splits a character")]
    NotCharBoundary,
    #[error("the span's text does not match the source")]
    Mismatch,
}

fn check_len(quote: &str) -> Result<(), SpanError> {
    if quote.len() < MIN_QUOTE_BYTES || quote.trim().len() < MIN_QUOTE_BYTES {
        return Err(SpanError::TooShort);
    }
    if quote.len() > MAX_QUOTE_BYTES {
        return Err(SpanError::TooLong);
    }
    Ok(())
}

/// Locates `quote` verbatim in `source`, preferring the first occurrence at or after
/// `from` (so repeated phrases map to the passage the idea is about, in order).
pub fn locate(source: &str, quote: &str, from: usize) -> Result<Span, SpanError> {
    check_len(quote)?;
    let from = from.min(source.len());
    let found = source
        .get(from..)
        .and_then(|tail| tail.find(quote).map(|at| at + from))
        .or_else(|| source.find(quote))
        .ok_or(SpanError::Mismatch)?;
    Ok(Span {
        start: found,
        end: found + quote.len(),
    })
}

/// Re-validates a stored span against the source and the text it claims to cover.
pub fn check_span(source: &str, span: Span, quote: &str) -> Result<(), SpanError> {
    check_len(quote)?;
    if span.start >= span.end || span.end > source.len() {
        return Err(SpanError::OutOfBounds);
    }
    if !source.is_char_boundary(span.start) || !source.is_char_boundary(span.end) {
        return Err(SpanError::NotCharBoundary);
    }
    if source.get(span.start..span.end) == Some(quote) {
        Ok(())
    } else {
        Err(SpanError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "Bounded queues win. In 2024, 37 percent fewer pages. Bounded queues win again. Ünïcödé text here.";

    #[test]
    fn locates_exact_quotes_in_order() {
        let first = locate(SOURCE, "Bounded queues win", 0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(first, Span { start: 0, end: 18 });
        let second =
            locate(SOURCE, "Bounded queues win", first.end).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&SOURCE[second.start..second.end], "Bounded queues win");
        assert!(second.start > first.start);
        assert!(check_span(SOURCE, second, "Bounded queues win").is_ok());
    }

    #[test]
    fn refuses_quotes_that_are_not_in_the_source() {
        assert_eq!(
            locate(SOURCE, "47 percent fewer pages", 0),
            Err(SpanError::Mismatch)
        );
        assert_eq!(
            locate(SOURCE, "bounded queues win", 0),
            Err(SpanError::Mismatch)
        );
        assert_eq!(
            locate(SOURCE, "Bounded  queues win", 0),
            Err(SpanError::Mismatch)
        );
        assert_eq!(locate(SOURCE, "win.", 0), Err(SpanError::TooShort));
        assert_eq!(locate(SOURCE, "        ", 0), Err(SpanError::TooShort));
        assert_eq!(
            locate(SOURCE, &"x".repeat(1001), 0),
            Err(SpanError::TooLong)
        );
    }

    #[test]
    fn check_span_refuses_every_bad_span() {
        let q = "37 percent fewer pages";
        let at = SOURCE.find(q).unwrap_or(0);
        let good = Span {
            start: at,
            end: at + q.len(),
        };
        assert!(check_span(SOURCE, good, q).is_ok());
        assert_eq!(
            check_span(
                SOURCE,
                Span {
                    start: at + 1,
                    end: at + 1 + q.len()
                },
                q
            ),
            Err(SpanError::Mismatch)
        );
        assert_eq!(
            check_span(
                SOURCE,
                Span {
                    start: 0,
                    end: SOURCE.len() + 1
                },
                q
            ),
            Err(SpanError::OutOfBounds)
        );
        assert_eq!(
            check_span(SOURCE, Span { start: 9, end: 9 }, q),
            Err(SpanError::OutOfBounds)
        );
        let u = SOURCE.find('Ü').unwrap_or(0);
        assert_eq!(
            check_span(
                SOURCE,
                Span {
                    start: u + 1,
                    end: u + 10
                },
                "abcdefghi"
            ),
            Err(SpanError::NotCharBoundary)
        );
    }
}
