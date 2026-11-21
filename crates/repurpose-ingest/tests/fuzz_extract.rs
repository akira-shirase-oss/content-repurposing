//! Fuzz target for the extractor and the decoder: any bytes, any markup soup, any
//! declared charset never panic, never exceed the text bound, and fail only by name.
#![allow(clippy::unwrap_used, reason = "test: a panic is the failure report")]

use proptest::prelude::*;
use repurpose_ingest::decode::decode_text;
use repurpose_ingest::extract::{Limits, extract_html, extract_plain, normalize};
use url::Url;

fn limits() -> Limits {
    Limits {
        max_text_bytes: 4 * 1024,
        ..Limits::default()
    }
}

fn soup() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("<p>".to_owned()),
        Just("</p>".to_owned()),
        Just("<div>".to_owned()),
        Just("</div>".to_owned()),
        Just("<script>".to_owned()),
        Just("</script>".to_owned()),
        Just("<title>".to_owned()),
        Just("<!--".to_owned()),
        Just("-->".to_owned()),
        Just("<![CDATA[".to_owned()),
        Just("<meta charset=\"utf-16\">".to_owned()),
        Just("<article>".to_owned()),
        Just("&amp;&#0;&#x110000;".to_owned()),
        Just("<a href=\"javascript:alert(1)\">".to_owned()),
        "[ -~]{0,40}",
        "\\PC{0,20}",
        Just("Bounded queues keep a service honest under load and in the long tail. ".to_owned()),
    ];
    prop::collection::vec(piece, 0..200).prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn extract_never_panics_and_stays_bounded(html in soup()) {
        let url = Url::parse("https://example.com/post").unwrap();
        if let Ok(article) = extract_html(&html, &url, &limits()) {
            prop_assert!(article.text.len() <= 4 * 1024);
            prop_assert!(!article.text.contains('\0'));
            prop_assert_eq!(normalize(&article.text), article.text.clone());
        }
    }

    #[test]
    fn any_bytes_decode_and_extract_without_panicking(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
        charset in prop_oneof![Just(None), Just(Some("text/html; charset=utf-16le")),
            Just(Some("text/html; charset=windows-1251")), Just(Some("text/html; charset=utf-7")),
            Just(Some("text/plain; charset=x-unknown"))],
    ) {
        let text = decode_text(&bytes, charset);
        let url = Url::parse("https://example.com/post").unwrap();
        let _ = extract_html(&text, &url, &limits());
        if let Ok(article) = extract_plain(&text, &limits()) {
            prop_assert!(article.text.len() <= 4 * 1024);
        }
    }
}
