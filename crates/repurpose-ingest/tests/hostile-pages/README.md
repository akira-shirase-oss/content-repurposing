# hostile-pages fixtures

Pages served by the fetcher tests (`tests/hostile_pages.rs`). Generated hostile bodies
(the 1 GiB stream, the gzip bomb, endless redirects, slow-loris drips) are built in the
test itself so no large file is committed.

- `article.html`: a benign article, the control.
- `script-only.html`: an app shell with no server-rendered text; must be `extract_no_article`.
- `malformed.html`: unclosed tags, stray closers, an unterminated comment; extracts or fails by name.
- `unclosed-title.html`: a `<title>` never closed swallows the page as RCDATA; `extract_no_article`.
- `svg-title-nest.html`, `svg-style-nest.html`, `comment-bang-nest.html`: 5,000 nested
  `<div>`s behind markup a raw-text prescan skips (`<title>`/`<style>` inside `<svg>`
  are markup to the parser; `--!>` ends a comment): `extract_too_deep` within a fixed
  operation count, the same count at eight times the nesting.
- `charset-meta-windows-1251.html`: Cyrillic bytes declared only by `<meta http-equiv>`.
- `charset-utf7-trick.html`: declares UTF-7 to smuggle markup; UTF-7 is not honoured.
- `charset-bom-vs-header.html`: a UTF-8 BOM that must beat a lying header and meta.
