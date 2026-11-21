//! Turning fetched bytes into text: bounded gzip and the charset decision.

use std::io::Read;

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};

use crate::error::FetchError;

/// How far into a page the `<meta charset>` prescan looks (the HTML standard's 1024).
const PRESCAN_BYTES: usize = 1024;

/// Media types the fetcher accepts.
pub fn is_textual(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "text/html" | "application/xhtml+xml" | "text/plain"
    )
}

/// Whether the declared type is plain text (no Readability pass).
pub fn is_plain_text(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|e| e.trim().eq_ignore_ascii_case("text/plain"))
    })
}

/// Inflates a gzip body, refusing to produce more than `max` bytes. The decoder stops at
/// `max + 1`, so a bomb costs at most `max` bytes of memory and that much inflation.
pub fn gunzip(wire: &[u8], max: usize) -> Result<Vec<u8>, FetchError> {
    gunzip_counted(wire, max).0
}

/// [`gunzip`], also returning how many bytes the decoder inflated.
pub fn gunzip_counted(wire: &[u8], max: usize) -> (Result<Vec<u8>, FetchError>, u64) {
    let mut out = Vec::new();
    let mut decoder = Counted {
        inner: flate2::read::MultiGzDecoder::new(wire),
        read: 0,
    };
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let read = (&mut decoder).take(limit).read_to_end(&mut out);
    let inflated = decoder.read;
    if read.is_err() {
        return (Err(FetchError::CorruptEncoding), inflated);
    }
    if out.len() > max {
        return (Err(FetchError::DecompressionBomb), inflated);
    }
    (Ok(out), inflated)
}

struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read = self.read.saturating_add(n as u64);
        Ok(n)
    }
}

/// Decodes a page: a byte-order mark wins, then the `Content-Type` charset, then a
/// `<meta>` charset in the first 1024 bytes, then UTF-8. A `<meta>` claiming UTF-16
/// means UTF-8 (the page was read as ASCII to find it). Unknown labels such as
/// `utf-7` are ignored, never guessed. NULs and other C0 controls are dropped.
pub fn decode_text(bytes: &[u8], content_type: Option<&str>) -> String {
    let (encoding, body) = if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        (encoding, bytes.get(bom..).unwrap_or_default())
    } else {
        {
            let declared = content_type
                .and_then(charset_param)
                .and_then(|label| Encoding::for_label(label.as_bytes()));
            let encoding = declared
                .or_else(|| {
                    meta_charset(bytes.get(..PRESCAN_BYTES).unwrap_or(bytes)).map(|e| {
                        if e == UTF_16LE || e == UTF_16BE {
                            UTF_8
                        } else {
                            e
                        }
                    })
                })
                .unwrap_or(UTF_8);
            (encoding, bytes)
        }
    };
    let (text, _) = encoding.decode_without_bom_handling(body);
    text.chars()
        .filter(|&c| !c.is_control() || c == '\n' || c == '\t' || c == '\r')
        .collect()
}

fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).to_owned())
    })
}

/// A simplified prescan: the first `charset=` inside a `<meta` tag.
fn meta_charset(head: &[u8]) -> Option<&'static Encoding> {
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = find(&lower[from..], b"<meta") {
        let tag_start = from + at;
        let tag_end = find(&lower[tag_start..], b">").map_or(lower.len(), |e| tag_start + e);
        let tag = &lower[tag_start..tag_end];
        if let Some(c) = find(tag, b"charset=") {
            let value = &tag[c + b"charset=".len()..];
            let value = value
                .strip_prefix(b"\"")
                .or_else(|| value.strip_prefix(b"'"))
                .unwrap_or(value);
            let end = value
                .iter()
                .position(|b| matches!(b, b'"' | b'\'' | b';' | b' ' | b'/' | b'>'))
                .unwrap_or(value.len());
            if let Some(encoding) = Encoding::for_label(&value[..end]) {
                return Some(encoding);
            }
        }
        from = tag_end;
        if from >= lower.len() {
            break;
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_precedence_and_tricks() {
        // BOM beats a lying header.
        let bom = [&[0xEF, 0xBB, 0xBF][..], "café".as_bytes()].concat();
        assert_eq!(
            decode_text(&bom, Some("text/html; charset=windows-1252")),
            "café"
        );
        // Header beats meta.
        let latin = b"<meta charset=\"utf-8\"><p>caf\xe9</p>";
        assert!(decode_text(latin, Some("text/html; charset=ISO-8859-1")).contains("café"));
        // Meta is used when the header is silent.
        let meta = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\"></head><p>\xcf\xf0\xe8</p>";
        assert!(decode_text(meta, Some("text/html")).contains("При"));
        // UTF-7 is not a WHATWG encoding: ignored, page read as UTF-8, payload stays inert.
        let utf7 = b"<meta charset=utf-7><p>+ADw-script+AD4-</p>";
        assert!(decode_text(utf7, None).contains("+ADw-script+AD4-"));
        // A meta claiming UTF-16 on an ASCII-readable page means UTF-8.
        let u16 = "<meta charset=utf-16><p>naïve</p>".as_bytes();
        assert!(decode_text(u16, None).contains("naïve"));
        // Invalid UTF-8 becomes replacement characters, never a panic; NULs are dropped.
        assert_eq!(decode_text(b"a\xff\x00b", None), "a\u{fffd}b");
    }

    #[test]
    fn gzip_is_bounded() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..16 {
            enc.write_all(&zeros).unwrap_or_default();
        }
        let bomb = enc.finish().unwrap_or_default();
        assert!(bomb.len() < 64 * 1024);
        assert_eq!(
            gunzip(&bomb, 1024 * 1024),
            Err(FetchError::DecompressionBomb)
        );
        assert_eq!(
            gunzip(&bomb, 16 * 1024 * 1024).map(|v| v.len()),
            Ok(16 * 1024 * 1024)
        );
        assert_eq!(gunzip(b"not gzip", 1024), Err(FetchError::CorruptEncoding));
    }

    /// The bound is the `take`, not the size check after it: a bomb far over the limit
    /// is inflated only to the limit plus one read buffer.
    #[test]
    fn gzip_inflates_no_more_than_the_limit() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..64 {
            enc.write_all(&zeros).unwrap_or_default();
        }
        let bomb = enc.finish().unwrap_or_default();
        assert!(bomb.len() < 128 * 1024);
        let max = 1024 * 1024;
        let (out, inflated) = gunzip_counted(&bomb, max);
        assert_eq!(out, Err(FetchError::DecompressionBomb));
        assert!(inflated <= (max + 64 * 1024) as u64, "inflated {inflated}");
    }
}
