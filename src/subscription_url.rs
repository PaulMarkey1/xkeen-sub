use std::fmt;
use url::{ParseError, Url};

#[derive(Debug)]
pub enum SubscriptionUrlError {
    Parse(ParseError),
    UnsupportedScheme,
    MissingHost,
}

impl SubscriptionUrlError {
    pub fn add_prompt_message(&self) -> &'static str {
        match self {
            Self::Parse(_) => "не удалось разобрать адрес",
            Self::UnsupportedScheme => "разрешены только схемы HTTP и HTTPS",
            Self::MissingHost => "в адресе отсутствует host",
        }
    }
}

impl fmt::Display for SubscriptionUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "{error}"),
            Self::UnsupportedScheme => write!(formatter, "разрешены только схемы HTTP и HTTPS"),
            Self::MissingHost => write!(formatter, "в URL отсутствует host"),
        }
    }
}

impl std::error::Error for SubscriptionUrlError {}

/// Parses and validates an HTTP(S) subscription URL.
pub fn parse_subscription_url(value: &str) -> Result<Url, SubscriptionUrlError> {
    let parsed = Url::parse(value).map_err(SubscriptionUrlError::Parse)?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SubscriptionUrlError::UnsupportedScheme);
    }

    if parsed.host_str().is_none() {
        return Err(SubscriptionUrlError::MissingHost);
    }

    Ok(parsed)
}

/// Returns the canonical URL representation used when a subscription is added.
pub fn normalize_subscription_url(value: &str) -> Result<String, SubscriptionUrlError> {
    parse_subscription_url(value).map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{normalize_subscription_url, parse_subscription_url, SubscriptionUrlError};

    #[test]
    fn accepts_http_and_https_urls() {
        assert!(parse_subscription_url("https://example.com/sub").is_ok());
        assert!(parse_subscription_url("http://example.com/sub").is_ok());
    }

    #[test]
    fn rejects_unsupported_schemes() {
        assert!(matches!(
            parse_subscription_url("ftp://example.com/sub"),
            Err(SubscriptionUrlError::UnsupportedScheme)
        ));
    }

    #[test]
    fn rejects_malformed_urls() {
        assert!(matches!(
            parse_subscription_url("not a url"),
            Err(SubscriptionUrlError::Parse(_))
        ));
    }

    #[test]
    fn normalizes_valid_urls() {
        assert_eq!(
            normalize_subscription_url("HTTPS://EXAMPLE.COM/sub").unwrap(),
            "https://example.com/sub"
        );
    }
}
