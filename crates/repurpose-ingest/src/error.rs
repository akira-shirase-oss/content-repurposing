use thiserror::Error;

/// Why a URL or an address was refused before anything was sent to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Refusal {
    #[error("the URL could not be parsed")]
    BadUrl,
    #[error("the URL is too long")]
    UrlTooLong,
    #[error("only http and https URLs can be fetched")]
    Scheme,
    #[error("URLs with credentials are refused")]
    Credentials,
    #[error("only the standard web ports can be fetched")]
    Port,
    #[error("the host name is not a public name")]
    LocalName,
    #[error("the host resolves to an address that is not public")]
    Address,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::BadUrl => "url_invalid",
            Self::UrlTooLong => "url_too_long",
            Self::Scheme => "url_scheme",
            Self::Credentials => "url_credentials",
            Self::Port => "url_port",
            Self::LocalName => "url_local_name",
            Self::Address => "url_address",
        }
    }
}

/// A named fetch failure. Messages never carry the URL, a header or body bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("refused: {0}")]
    Refused(Refusal),
    #[error("the host name did not resolve")]
    Dns,
    #[error("could not connect")]
    Connect,
    #[error("TLS failed")]
    Tls,
    #[error("the server broke the HTTP exchange")]
    Protocol,
    #[error("the fetch exceeded its deadline")]
    Timeout,
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("a redirect had no usable location")]
    BadRedirect,
    #[error("the server answered with status {0}")]
    Status(u16),
    #[error("the page is not HTML or text")]
    UnsupportedType,
    #[error("the page uses an unsupported content encoding")]
    UnsupportedEncoding,
    #[error("the page is larger than the fetch limit")]
    TooLarge,
    #[error("the compressed page expands past the limit")]
    DecompressionBomb,
    #[error("the compressed page is corrupt")]
    CorruptEncoding,
}

impl From<Refusal> for FetchError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl FetchError {
    /// A stable snake-case name for job records and metrics.
    pub fn code(self) -> &'static str {
        match self {
            Self::Refused(_) => "fetch_refused",
            Self::Dns => "fetch_dns",
            Self::Connect => "fetch_connect",
            Self::Tls => "fetch_tls",
            Self::Protocol => "fetch_protocol",
            Self::Timeout => "fetch_timeout",
            Self::TooManyRedirects => "fetch_too_many_redirects",
            Self::BadRedirect => "fetch_bad_redirect",
            Self::Status(_) => "fetch_status",
            Self::UnsupportedType => "fetch_unsupported_type",
            Self::UnsupportedEncoding => "fetch_unsupported_encoding",
            Self::TooLarge => "fetch_too_large",
            Self::DecompressionBomb => "fetch_decompression_bomb",
            Self::CorruptEncoding => "fetch_corrupt_encoding",
        }
    }

    /// Whether a later attempt could succeed (network weather, not a property of the page).
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Dns | Self::Connect | Self::Timeout)
            || matches!(self, Self::Status(code) if code == 429 || code >= 500)
    }
}

/// Why a fetched page yielded no article.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExtractError {
    #[error("the page has more elements than the extractor parses")]
    TooManyElements,
    #[error("the page nests elements deeper than the extractor walks")]
    TooDeep,
    #[error("the page has no readable article")]
    NoArticle,
    #[error("the extraction was abandoned at its deadline")]
    Cancelled,
}

impl ExtractError {
    pub fn code(self) -> &'static str {
        match self {
            Self::TooManyElements => "extract_too_many_elements",
            Self::TooDeep => "extract_too_deep",
            Self::NoArticle => "extract_no_article",
            Self::Cancelled => "extract_cancelled",
        }
    }
}
