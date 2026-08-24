use crate::logging;
use serde::Deserialize;

const API_BASE: &str = "https://api.telegram.org/bot";

/// Resolve the effective Telegram Bot API base (with a trailing `/bot`).
///
/// Honors an override (e.g. a reverse proxy mirror or an alternate data-center
/// IP that bypasses a blocked regional Telegram endpoint). If the override has
/// no `/bot` path segment it is appended for you.
pub fn api_base(override_base: Option<&str>) -> String {
    match override_base {
        Some(raw) if !raw.trim().is_empty() => {
            let trimmed = raw.trim().trim_end_matches('/');
            if trimmed.ends_with("/bot") {
                format!("{}/", trimmed)
            } else {
                format!("{}/bot/", trimmed)
            }
        }
        _ => API_BASE.to_string(),
    }
}

/// Build a HTTP client for Telegram Bot API calls.
///
/// When `proxy_url` is set, a dedicated client is built that routes through the
/// proxy (SOCKS5/HTTP). When `api_ip` is set, the `api.telegram.org` hostname is
/// pinned to that IP via reqwest's resolver (keeping the real hostname for
/// TLS/SNI verification), so a blocked default DC IP can be bypassed without a
/// proxy. Either override (or neither) may be provided.
pub fn build_client(proxy_url: Option<&str>, api_ip: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)));
    if let Some(proxy) = proxy_url
        && let Some(proxy) = non_empty(proxy)
    {
        let reqwest_proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| anyhow::anyhow!("invalid telegram proxy `{proxy}`: {e}"))?;
        builder = builder.proxy(reqwest_proxy);
    }
    if let Some(ip) = api_ip
        && let Some(ip) = non_empty(ip)
    {
        let addr = parse_telegram_ip(ip)?;
        builder = builder.resolve("api.telegram.org", addr);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build telegram client: {e}"))
}

/// Parse an IPv4/IPv6 alternate Telegram IP into a `SocketAddr` on port 443.
fn parse_telegram_ip(ip: &str) -> anyhow::Result<std::net::SocketAddr> {
    let ip: std::net::IpAddr = ip
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid telegram api_ip `{ip}`: expected an IP address"))?;
    Ok(std::net::SocketAddr::new(ip, 443))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub text: Option<String>,
    pub chat: Chat,
    pub from: Option<TelegramFrom>,
    #[serde(rename = "date")]
    pub _date: i64,
}

#[derive(Debug, Deserialize)]
pub struct TelegramFrom {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

pub async fn send_message(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    send_message_with_base(client, bot_token, chat_id, text, None).await
}

/// [`send_message`] with an explicit Bot API base override (e.g. reverse-proxy
/// mirror or alternate data-center IP). Pass `None` to use the default.
pub async fn send_message_with_base(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    base_override: Option<&str>,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendMessage", api_base(base_override), bot_token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        }))
        .send()
        .await?;

    let status = resp.status();
    let body: TelegramResponse<serde_json::Value> = resp.json().await?;

    if !body.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            body.description.unwrap_or_default()
        );
    }

    logging::info("Telegram notification sent");
    Ok(())
}

pub async fn get_updates(
    client: &reqwest::Client,
    bot_token: &str,
    offset: Option<i64>,
    timeout_secs: u64,
) -> anyhow::Result<Vec<Update>> {
    get_updates_with_base(client, bot_token, offset, timeout_secs, None).await
}

/// [`get_updates`] with an explicit Bot API base override.
pub async fn get_updates_with_base(
    client: &reqwest::Client,
    bot_token: &str,
    offset: Option<i64>,
    timeout_secs: u64,
    base_override: Option<&str>,
) -> anyhow::Result<Vec<Update>> {
    let url = format!("{}{}/getUpdates", api_base(base_override), bot_token);
    let mut params = serde_json::json!({
        "timeout": timeout_secs,
        "allowed_updates": ["message"],
    });

    if let Some(off) = offset {
        params["offset"] = serde_json::json!(off);
    }

    let resp = client
        .post(&url)
        .json(&params)
        .timeout(std::time::Duration::from_secs(timeout_secs + 5))
        .send()
        .await?;

    let body: TelegramResponse<Vec<Update>> = resp.json().await?;

    if !body.ok {
        anyhow::bail!(
            "Telegram getUpdates error: {}",
            body.description.unwrap_or_default()
        );
    }

    Ok(body.result.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_update() {
        let json = r#"{
            "update_id": 123,
            "message": {
                "text": "hello",
                "chat": {"id": 456},
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 123);
        assert_eq!(update.message.unwrap().text.unwrap(), "hello");
    }

    #[test]
    fn test_parse_response() {
        let json = r#"{"ok": true, "result": []}"#;
        let resp: TelegramResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.result.unwrap().is_empty());
    }

    #[test]
    fn test_api_base_default() {
        assert_eq!(api_base(None), "https://api.telegram.org/bot");
        assert_eq!(api_base(Some("")), "https://api.telegram.org/bot");
        assert_eq!(api_base(Some("   ")), "https://api.telegram.org/bot");
    }

    #[test]
    fn test_api_base_override_normalization() {
        // Alternate data-center IP (the user's working DC).
        assert_eq!(
            api_base(Some("https://api.telegram.org/")),
            "https://api.telegram.org/bot/"
        );
        // Raw host without scheme is treated as-is by pushing the wrapper.
        assert_eq!(api_base(Some("149.154.167.220")), "149.154.167.220/bot/");
        // Already has /bot path.
        assert_eq!(
            api_base(Some("https://mirror.example.com/bot")),
            "https://mirror.example.com/bot/"
        );
        // Trailing slash handled.
        assert_eq!(
            api_base(Some("https://mirror.example.com/bot/")),
            "https://mirror.example.com/bot/"
        );
    }

    #[test]
    fn test_build_client_rejects_bad_proxy() {
        // reqwest rejects an all-symbols "url" (url::ParseError at build time).
        assert!(build_client(Some("::::"), None).is_err());
    }

    #[test]
    fn test_build_client_ok_without_overrides() {
        assert!(build_client(None, None).is_ok());
        assert!(build_client(Some(""), None).is_ok());
        assert!(build_client(None, Some("")).is_ok());
    }

    #[test]
    fn test_api_ip_rejects_non_ip() {
        assert!(build_client(None, Some("not-an-ip")).is_err());
        assert!(build_client(None, Some("api.telegram.org")).is_err());
        assert!(build_client(None, Some("::::")).is_err());
    }

    #[test]
    fn test_api_ip_accepts_ipv4_and_ipv6() {
        assert!(build_client(None, Some("149.154.167.220")).is_ok());
        assert!(build_client(None, Some("2001:67c:4e8::1")).is_ok());
    }
}
