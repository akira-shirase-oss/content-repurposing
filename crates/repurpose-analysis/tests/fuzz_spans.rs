//! Fuzz target for the span checker: whatever the source and the quote, a located span
//! is exactly the quote's bytes, `check_span` agrees with slicing, and nothing panics
//! on offsets that split characters or run past the end.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use proptest::prelude::*;
use repurpose_analysis::spans::{Span, check_span, locate};

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    #[test]
    fn a_quote_cut_from_the_source_is_located_exactly(
        source in "\\PC{0,400}",
        a in 0usize..400,
        len in 0usize..200,
        from in 0usize..500,
    ) {
        let chars: Vec<(usize, char)> = source.char_indices().collect();
        if chars.is_empty() { return Ok(()); }
        let start = chars[a % chars.len()].0;
        let end = chars.get((a % chars.len()) + len).map_or(source.len(), |c| c.0);
        let quote = &source[start..end];
        match locate(&source, quote, from) {
            Ok(span) => {
                prop_assert_eq!(&source[span.start..span.end], quote);
                prop_assert!(check_span(&source, span, quote).is_ok());
            }
            Err(_) => prop_assert!(quote.trim().len() < 8 || quote.len() > 1000),
        }
    }

    #[test]
    fn check_span_is_slicing_or_a_named_refusal(
        source in "\\PC{0,200}",
        start in 0usize..300,
        end in 0usize..300,
        quote in "\\PC{0,60}",
    ) {
        let span = Span { start, end };
        let ok = check_span(&source, span, &quote).is_ok();
        let sliced = source.get(start..end) == Some(quote.as_str()) && start < end;
        prop_assert!(!ok || sliced, "accepted a span that is not the quote");
    }

    #[test]
    fn an_altered_quote_is_never_located(
        source in "[a-z ]{20,300}",
        at in 0usize..300,
    ) {
        let start = at % (source.len() - 10);
        let mut quote = source[start..start + 10].to_owned();
        quote.push('#');
        prop_assert!(locate(&source, &quote, 0).is_err());
    }
}
