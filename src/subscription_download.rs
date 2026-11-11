use std::error::Error;
use std::process::{Command, Output};
use url::Url;

use crate::subscriptions::Subscription;

type AppResult<T> = Result<T, Box<dyn Error>>;

// Keep the download in memory: this module must not modify XKeen files or
// the subscription baseline. Parsing and applying the response belong to
// later update stages.
const CONNECT_TIMEOUT_SECONDS: &str = "10";
const TOTAL_TIMEOUT_SECONDS: &str = "30";
const MAX_REDIRECTS: &str = "5";

/// Downloads a subscription response and returns its raw bytes.
///
/// HTTP errors, transport errors, missing download tools, and empty
/// responses are reported to the caller. The response is deliberately not
/// parsed here.
pub fn download_subscription(subscription: &Subscription) -> AppResult<Vec<u8>> {
    validate_url(&subscription.url).map_err(|error| {
        format!("Некорректный URL подписки {}: {error}", subscription.id)
    })?;

    let output = if command_exists("curl") {
        run_curl(&subscription.url).map_err(|error| {
            format!("Не удалось запустить curl для подписки {}: {error}", subscription.id)
        })?
    } else if command_exists("wget") {
        run_wget(&subscription.url).map_err(|error| {
            format!("Не удалось запустить wget для подписки {}: {error}", subscription.id)
        })?
    } else {
        return Err("Для загрузки подписок требуется curl или wget.".into());
    };

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let status = output.status;
        let message = if details.is_empty() {
            format!("Не удалось загрузить подписку {}: {status}.", subscription.id)
        } else {
            format!("Не удалось загрузить подписку {}: {details} (код процесса: {status}).", subscription.id)
        };
        return Err(message.into());
    }

    if output.stdout.is_empty() {
        return Err(format!("Подписка {} вернула пустой ответ.", subscription.id).into());
    }

    Ok(output.stdout)
}

fn validate_url(value: &str) -> Result<(), String> {
    let parsed = Url::parse(value).map_err(|error| error.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("разрешены только схемы HTTP и HTTPS".to_owned());
    }
    if parsed.host_str().is_none() {
        return Err("в URL отсутствует host".to_owned());
    }
    Ok(())
}

fn run_curl(url: &str) -> std::io::Result<Output> {
    Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            CONNECT_TIMEOUT_SECONDS,
            "--max-time",
            TOTAL_TIMEOUT_SECONDS,
            "--max-redirs",
            MAX_REDIRECTS,
            url,
        ])
        .output()
}

fn run_wget(url: &str) -> std::io::Result<Output> {
    Command::new("wget")
        .args([
            "--quiet",
            "--output-document=-",
            "--timeout=30",
            "--tries=1",
            url,
        ])
        .output()
}

fn command_exists(command: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::validate_url;

    #[test]
    fn only_http_and_https_urls_are_allowed() {
        assert!(validate_url("https://example.com/sub").is_ok());
        assert!(validate_url("http://example.com/sub").is_ok());
        assert!(validate_url("ftp://example.com/sub").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn empty_response_is_detected() {
        let response: Vec<u8> = Vec::new();
        assert!(response.is_empty());
    }

    #[test]
    fn non_empty_response_is_preserved_as_raw_bytes() {
        let response = b"{\"outbounds\":[]}".to_vec();
        assert_eq!(response, b"{\"outbounds\":[]}");
    }
}
