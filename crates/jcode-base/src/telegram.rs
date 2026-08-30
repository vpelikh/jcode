use crate::logging;
use reqwest::StatusCode;
use serde::Deserialize;

const API_BASE: &str = "https://api.telegram.org/bot";

/// Telegram hard limit on a single message text length in characters.
/// Messages longer than this are rejected with `400 text is too long`.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// Maximum times a transient (HTTP 429) send is retried with backoff.
const SEND_RETRIES: u32 = 4;

/// Upper bound on a single backoff sleep so a pathological stream of rate
/// limits can never stall a reply loop for long.
const MAX_RETRY_DELAY_SECS: u64 = 30;

/// Pause between consecutive chunks of a multi-message send. Firing several
/// `sendMessage` calls back-to-back can crowd Telegram's per-second flood
/// window; a short inter-chunk delay keeps long sends under the limit without
/// noticeably slowing them. Only applies between chunks, not to single sends.
const INTER_CHUNK_DELAY_MS: u64 = 200;

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

/// Curated list of known Telegram data-center IPs, tried in order when the
/// DNS-resolved default is blocked. These come from Telegram's published DC
/// ranges (149.154.167.x / 149.154.175.x and their IPv6 counterparts). The
/// hostname is always kept for TLS/SNI, so this only redirects the TCP
/// connection; it does not bypass certificate verification. TLS verification
/// still uses the hostname, so the server certificate is validated as usual.
/// We deliberately do not scan arbitrary ranges: discovery is bounded to this list.
pub const TELEGRAM_DC_CANDIDATES: &[&str] = &[
    "149.154.167.220",
    "149.154.167.40",
    "149.154.167.50",
    "149.154.175.50",
    "149.154.175.100",
    "149.154.175.53",
    "2001:67c:4e8::1",
    "2001:67c:4e8::2",
    "2001:67c:4e8::3",
    "2001:67c:4e8::4",
    "2001:67c:4e8::5",
    "2001:67c:4e8::6",
    "2001:67c:4e8::7",
    "2001:67c:4e8::8",
    "2001:67c:4e8::9",
    "2001:67c:4e8::a",
];

/// Seconds to wait before retrying discovery after a full failure, so a blocked
/// network does not trigger a fresh (slow) candidate sweep on every poll.
pub const DISCOVERY_BACKOFF_SECS: u64 = 60;

/// Per-candidate connect timeout during discovery. Kept short so an offline
/// network does not make the full sweep block for minutes (18 candidates × the
/// normal 15s is far too long for a single poll). A working candidate is well
/// within this; the long-poll path reuses the same resolved client afterward.
const DISCOVERY_PROBE_TIMEOUT_SECS: u64 = 8;

/// True if the error is a network-level failure (DNS poisoned, IP blocked,
/// connection refused/unreachable, TLS mismatch for a pinned IP) rather than an
/// application-level auth/API error. Used to decide whether to keep trying
/// alternate DC IPs.
pub fn is_connectivity_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("dns")
        || s.contains("resolve")
        || s.contains("lookup")
        || s.contains("name resolution")
        || s.contains("no address")
        || s.contains("timed out")
        || s.contains("timeout")
        || s.contains("deadline")
        || s.contains("connection refused")
        || s.contains("unreachable")
        || s.contains("no route")
        || s.contains("connect error")
        || s.contains("operation timed out")
        || s.contains("reset")
        || s.contains("tcp")
        || s.contains("tls")
        || s.contains("ssl")
        || s.contains("certificate")
        || s.contains("handshake")
        || s.contains("network")
        || s.contains("host")
}

/// Build a short-timeout client used only for the discovery probe (`getMe`).
/// A fast *connect* timeout keeps an unreachable candidate from stalling the
/// sweep. Crucially this client has NO overall request timeout: `discover_client`
/// returns it and it is reused for the real long-poll path (`getUpdates` holds
/// the connection open for ~30s), so capping total request time here would sever
/// long polling. The probe call itself is bounded separately via `tokio::time::timeout`.
fn build_probe_client(proxy: Option<&str>, api_ip: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(DISCOVERY_PROBE_TIMEOUT_SECS));
    if let Some(proxy) = proxy
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
        .map_err(|e| anyhow::anyhow!("failed to build telegram probe client: {e}"))
}

/// Build a working client for the Telegram Bot API, auto-discovering a
/// reachable data-center IP when the DNS-resolved default is blocked.
///
/// Precedence: if `override_ip` (from `[safety] telegram_api_ip`) is set, it is
/// tried first as an explicit escape hatch. Then the default DNS resolution is
/// tried, followed by the curated list of known DC IPs (`TELEGRAM_DC_CANDIDATES`).
/// Each candidate is probed with a short-timeout client; the first whose
/// `verify_bot_auth` probe succeeds is returned (and reused for the real path).
/// A non-connectivity error (e.g. a bad bot token) stops discovery immediately,
/// since no IP will help.
pub async fn discover_client(
    bot_token: &str,
    proxy: Option<&str>,
    override_ip: Option<&str>,
) -> anyhow::Result<reqwest::Client> {
    let mut candidates: Vec<Option<String>> = Vec::new();
    if let Some(ip) = override_ip.filter(|ip| !ip.trim().is_empty()) {
        candidates.push(Some(ip.trim().to_string()));
    }
    candidates.push(None); // default DNS resolution
    for ip in TELEGRAM_DC_CANDIDATES {
        candidates.push(Some((*ip).to_string()));
    }

    let mut last_err: Option<anyhow::Error> = None;
    for (i, maybe_ip) in candidates.iter().enumerate() {
        let client = match build_probe_client(proxy, maybe_ip.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(DISCOVERY_PROBE_TIMEOUT_SECS + 5),
            verify_bot_auth(&client, bot_token, None),
        )
        .await
        {
            Ok(Ok(_)) => {
                if let Some(ip) = maybe_ip {
                    logging::info(&format!("telegram reachable via pinned IP {ip}"));
                } else {
                    logging::info(&format!(
                        "telegram reachable via default DNS (candidate {i})"
                    ));
                }
                return Ok(client);
            }
            Ok(Err(e)) => {
                // Probe returned an application-level error (e.g. 401 Unauthorized).
                if !is_connectivity_error(&e) {
                    anyhow::bail!(
                        "Telegram auth failed (bad token?): {e}. Stopping IP discovery."
                    );
                }
                logging::debug(&format!(
                    "telegram candidate {i} unreachable ({maybe_ip:?}): {e}"
                ));
                last_err = Some(e);
            }
            Err(elapsed) => {
                // Overall probe timed out; classify as a connectivity failure and
                // keep trying other candidates.
                logging::debug(&format!(
                    "telegram candidate {i} timed out after {DISCOVERY_PROBE_TIMEOUT_SECS}s"
                ));
                last_err = Some(anyhow::anyhow!("probe timed out: {elapsed}"));
            }
        }
    }

    anyhow::bail!(
        "Telegram unreachable: tried default DNS and {} DC IPs. Last error: {}",
        TELEGRAM_DC_CANDIDATES.len(),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// Escape text for Telegram's legacy `Markdown` parse mode so it renders
/// literally and cannot be misinterpreted as formatting.
///
/// Legacy Markdown treats `_`, `*`, `` ` ``, `[`, `]`, and `\` as control
/// characters. When embedding user-provided or otherwise untrusted content into
/// a message that is sent with `parse_mode=Markdown`, escape these so the text
/// cannot (a) break parse_mode and trigger a "can't parse entities" resend, or
/// (b) leak unintended bold/italic/code formatting into the visible output.
///
/// A literal `\` is only special when it precedes one of the escapable
/// characters (it introduces an escape). So a backslash is doubled *only* when
/// followed by `_`, `*`, `` ` ``, `[`, `]`, or another `\`; a backslash before
/// any other character is left as-is so content like a Windows path
/// (`C:\Users\foo`) or a regex (`\d+`) renders without doubling.
///
/// The surrounding static markup is *not* escaped here; callers compose the
/// trusted delimiters (`*bold*`, `` `code` ``) and escape only the interpolated
/// value: `format!("💬 _{}_", escape_markdown(user_input))`.
pub fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '_' | '*' | '`' | '[' | ']' => {
                out.push('\\');
                out.push(ch);
            }
            '\\' => {
                // A backslash needs escaping only when it precedes an escapable
                // char (i.e. when Telegram would otherwise treat it as an escape
                // marker). Otherwise it is a literal backslash already.
                let next_escapable = chars
                    .get(i + 1)
                    .map(|n| matches!(n, '_' | '*' | '`' | '[' | ']' | '\\'))
                    .unwrap_or(false);
                if next_escapable {
                    out.push('\\');
                    out.push('\\');
                } else {
                    out.push('\\');
                }
            }
            _ => out.push(ch),
        }
        i += 1;
    }
    out
}

/// Split `text` into chunks no longer than Telegram's per-message limit.
///
/// Used by senders to deliver arbitrarily-long content (e.g. session history)
/// as a sequence of messages instead of having Telegram reject it. Chunks are
/// split on newlines when possible; a single over-long "word" is hard-split.
pub fn chunk_message(text: &str, max: usize) -> Vec<String> {
    let max = max.clamp(1, MAX_MESSAGE_CHARS);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        let mut line = line.to_string();
        // A single logical line can still exceed the cap (e.g. a long code
        // line). Hard-split it so no chunk overflows.
        while line.chars().count() > max {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let take: String = line.chars().take(max).collect();
            line = line.chars().skip(max).collect();
            chunks.push(take);
        }
        if current.chars().count() + line.chars().count() > max && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(&line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: Option<TelegramFrom>,
    pub data: Option<String>,
    pub message: Option<CallbackMessage>,
}

/// The message a callback query is attached to (chat id + message id are used
/// here to reply-to or edit the tapped message).
#[derive(Debug, Deserialize)]
pub struct CallbackMessage {
    pub chat: Option<Chat>,
    #[serde(default)]
    pub message_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub text: Option<String>,
    /// Caption of a media message (photo/document/video). Used as a textual
    /// fallback when `text` is absent, so attached files with a caption are not
    /// silently dropped.
    #[serde(default)]
    pub caption: Option<String>,
    pub chat: Chat,
    pub from: Option<TelegramFrom>,
    pub message_id: i64,
    #[serde(rename = "date")]
    pub _date: i64,
}

impl TelegramMessage {
    /// The effective inbound text: `text` if present, else a media `caption`.
    pub fn inbound_text(&self) -> Option<&str> {
        self.text
            .as_deref()
            .or(self.caption.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
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
    send_message_with_base(client, bot_token, chat_id, text, None, None).await
}

/// [`send_message`] with an explicit Bot API base override (e.g. reverse-proxy
/// mirror or alternate data-center IP). Pass `None` to use the default.
///
/// `reply_to_message_id`, when `Some`, makes Telegram render the message as a
/// reply to the given message in the chat (used to thread a bot answer under
/// the user message that triggered it).
///
/// Long `text` is split into multiple messages at Telegram's 4096-character
/// limit so content is never rejected; transient rate limits are retried and,
/// if Telegram cannot parse the Markdown, the message is resent in plain text
/// rather than being dropped.
pub async fn send_message_with_base(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    base_override: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendMessage", api_base(base_override), bot_token);
    if text.trim().is_empty() {
        return Ok(());
    }
    let chunks = chunk_message(text, MAX_MESSAGE_CHARS);
    let mut reply_to = reply_to_message_id;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": chunk,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        });
        if let Some(rt) = reply_to {
            body["reply_to_message_id"] = serde_json::json!(rt);
        }
        send_message_once(client, &url, body, true).await?;
        // Only the first message in a multi-chunk sequence threads under the
        // triggering message; the rest simply follow in the chat.
        reply_to = None;
        // Pace consecutive chunks so a long send stays under Telegram's
        // per-second flood-control window instead of triggering a 429.
        if i + 1 < chunks.len() {
            tokio::time::sleep(std::time::Duration::from_millis(INTER_CHUNK_DELAY_MS)).await;
        }
    }
    logging::info(&format!(
        "Telegram notification sent ({} message{})",
        chunks.len(),
        if chunks.len() == 1 { "" } else { "s" }
    ));
    Ok(())
}

/// Send one Telegram message, handling transient HTTP 429 rate limits (with
/// backoff) and, when `allow_plain_fallback` is set, resending without
/// `parse_mode` if Telegram rejects the Markdown (so dynamic agent output is
/// still delivered rather than silently lost).
async fn send_message_once(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
    allow_plain_fallback: bool,
) -> anyhow::Result<()> {
    let mut body = body;
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let resp = match client.post(url).json(&body).send().await {
            Ok(resp) => resp,
            Err(e) => return Err(anyhow::anyhow!("Telegram request failed: {e}")),
        };
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let status = resp.status();

        if status == StatusCode::TOO_MANY_REQUESTS {
            if attempts > SEND_RETRIES {
                return Err(anyhow::anyhow!("Telegram rate limited too many times"));
            }
            let backoff = retry_after
                .unwrap_or_else(|| 2u64.pow(attempts.saturating_sub(1)))
                .max(1)
                .min(MAX_RETRY_DELAY_SECS);
            logging::warn(&format!(
                "Telegram rate limited, waiting {backoff}s (attempt {attempts})"
            ));
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            continue;
        }

        let parsed: TelegramResponse<serde_json::Value> = resp.json().await?;
        if parsed.ok {
            return Ok(());
        }
        let description = parsed.description.clone().unwrap_or_default();
        if allow_plain_fallback
            && is_markdown_parse_error(&description)
            && !body.get("parse_mode").map(|v| v.is_null()).unwrap_or(true)
        {
            logging::warn(&format!(
                "Telegram rejected Markdown, resending as plain text: {description}"
            ));
            if let Some(obj) = body.as_object_mut() {
                obj.remove("parse_mode");
            }
            continue;
        }
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            description
        );
    }
}

/// Whether a Bot API error description indicates a Markdown parse failure
/// (which can be retried without `parse_mode`).
fn is_markdown_parse_error(description: &str) -> bool {
    let d = description.to_lowercase();
    d.contains("can't parse entities")
        || d.contains("cannot parse entities")
        || (d.contains("parse:") && d.contains("entity"))
}

/// Broadcast a chat action such as `typing` to show the user a live indicator
/// while the bot is processing a long request. Errors are swallowed so the
/// caller's flow is not disrupted by a transient indicator failure.
///
/// Returns `Err` when the request fails to send *or* Telegram reports the action
/// was not accepted (`ok:false`), so callers can decide whether retrying is
/// worthwhile rather than blindly looping a rejected indicator.
pub async fn send_chat_action(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    action: &str,
    base_override: Option<&str>,
) -> Result<(), ()> {
    let url = format!("{}{}/sendChatAction", api_base(base_override), bot_token);
    let body = serde_json::json!({
        "chat_id": chat_id,
        "action": action,
    });
    let resp = match client.post(&url).json(&body).send().await {
        Ok(resp) => resp,
        Err(_) => return Err(()),
    };
    // Parse the response body. A connection that succeeded does not guarantee
    // the action was accepted; a non-2xx status or an `ok:false` body means the
    // indicator was not sent.
    if !resp.status().is_success() {
        return Err(());
    }
    match resp.json::<TelegramResponse<serde_json::Value>>().await {
        Ok(parsed) if parsed.ok => Ok(()),
        _ => Err(()),
    }
}

/// Register the bot's slash command list in the Telegram client so users can
/// discover commands via the `/` menu. Non-fatal: failures are swallowed.
pub async fn set_my_commands(
    client: &reqwest::Client,
    bot_token: &str,
    base_override: Option<&str>,
) {
    let body = serde_json::json!({
        "commands": [
            { "command": "start", "description": "Show help and available commands" },
            { "command": "list", "description": "List recent sessions" },
            { "command": "new", "description": "Start a new session (optionally with a prompt)" },
            { "command": "use", "description": "Select a session to talk to (id or #)" },
            { "command": "history", "description": "Show recent messages of the active session" },
            { "command": "resume", "description": "Ask a session (id + prompt)" },
            { "command": "clear", "description": "Stop talking to the active session" },
            { "command": "status", "description": "Show control & ambient status" },
            { "command": "help", "description": "Show help" },
        ]
    });
    if let Err(e) = post_telegram(client, bot_token, "setMyCommands", body, base_override).await {
        logging::warn(&format!("failed to register telegram commands: {e}"));
    }
}

/// Clear the inline keyboard from a message. Used to collapse a session picker
/// after a selection so the buttons do not linger. Non-fatal.
pub async fn edit_message_reply_markup(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: i64,
    message_id: i64,
    base_override: Option<&str>,
) {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reply_markup": { "inline_keyboard": [] },
    });
    if let Err(e) = post_telegram(
        client,
        bot_token,
        "editMessageReplyMarkup",
        body,
        base_override,
    )
    .await
    {
        logging::warn(&format!(
            "failed to clear telegram inline keyboard chat={chat_id} msg={message_id}: {e}"
        ));
    }
}

/// A single inline keyboard button with `callback_data`.
#[derive(Debug, Clone)]
pub struct InlineKeyboardButton {
    pub text: String,
    pub callback_data: String,
}

/// A row of inline keyboard buttons.
pub type InlineKeyboardRow = Vec<InlineKeyboardButton>;

/// Send a message with an optional inline keyboard (`callback_data` buttons).
pub async fn send_message_with_keyboard(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    keyboard: &[InlineKeyboardRow],
    base_override: Option<&str>,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendMessage", api_base(base_override), bot_token);
    if text.trim().is_empty() {
        return Ok(());
    }
    let chunks = chunk_message(text, MAX_MESSAGE_CHARS);
    for (i, chunk) in chunks.iter().enumerate() {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": chunk,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        });
        // Keyboard is attached to the first chunk only; the rest are plain
        // follow-up messages so the buttons don't repeat.
        if i == 0 && !keyboard.is_empty() {
            let rows: Vec<serde_json::Value> = keyboard.iter().map(|row| json_row(row)).collect();
            body["reply_markup"] = serde_json::json!({ "inline_keyboard": rows });
        }
        send_message_once(client, &url, body, true).await?;
        if i + 1 < chunks.len() {
            tokio::time::sleep(std::time::Duration::from_millis(INTER_CHUNK_DELAY_MS)).await;
        }
    }
    Ok(())
}

/// Confirm receipt of a callback query so Telegram stops showing the loading
/// spinner on the tapped button.
pub async fn answer_callback_query(
    client: &reqwest::Client,
    bot_token: &str,
    callback_query_id: &str,
    text: &str,
    base_override: Option<&str>,
) -> anyhow::Result<()> {
    let mut body = serde_json::json!({ "callback_query_id": callback_query_id });
    // Telegram's `text` field must be 1-200 characters when present. An empty
    // string is rejected with "Bad Request" and leaves the tapped button stuck
    // in its loading state, so only include `text` when the caller supplied a
    // non-empty notification.
    if !text.is_empty() {
        body["text"] = serde_json::json!(text);
    }
    post_telegram(client, bot_token, "answerCallbackQuery", body, base_override).await
}

/// POST a JSON body to a Bot API method and return whether it succeeded,
/// logging on failure but not erroring (agnostic callers decide).
async fn post_telegram(
    client: &reqwest::Client,
    bot_token: &str,
    method: &str,
    body: serde_json::Value,
    base_override: Option<&str>,
) -> anyhow::Result<()> {
    let url = format!("{}{}/{}", api_base(base_override), bot_token, method);
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let parsed: TelegramResponse<serde_json::Value> = resp.json().await?;
    if !parsed.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            parsed.description.unwrap_or_default()
        );
    }
    Ok(())
}

/// The `getMe` result: the bot's own identity as reported by Telegram.
#[derive(Debug, Deserialize)]
pub struct BotIdentity {
    pub id: i64,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Call `getMe` to verify the bot token is valid (startup auth check). Returns
/// the bot identity on success, or an error describing the failure so setup
/// mistakes surface early instead of as repeated polling errors.
pub async fn verify_bot_auth(
    client: &reqwest::Client,
    bot_token: &str,
    base_override: Option<&str>,
) -> anyhow::Result<BotIdentity> {
    let url = format!("{}{}/getMe", api_base(base_override), bot_token);
    let resp = client.post(&url).send().await?;
    let status = resp.status();
    let parsed: TelegramResponse<BotIdentity> = resp.json().await?;
    if !parsed.ok {
        anyhow::bail!(
            "Telegram auth failed ({}): {}",
            status,
            parsed.description.unwrap_or_default()
        );
    }
    parsed
        .result
        .ok_or_else(|| anyhow::anyhow!("Telegram getMe returned no bot identity"))
}

/// Convert one keyboard row into a JSON array of button objects.
fn json_row(row: &[InlineKeyboardButton]) -> serde_json::Value {
    let buttons: Vec<serde_json::Value> = row
        .iter()
        .map(|b| {
            serde_json::json!({
                "text": b.text,
                "callback_data": b.callback_data,
            })
        })
        .collect();
    serde_json::Value::Array(buttons)
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
        "allowed_updates": ["message", "callback_query"],
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
        // Telegram's Bot API returns HTTP 200 with a body of
        // `{"ok":false,"error_code":409,...}` when *another* bot instance (or a
        // stale poll from an old process) already holds the `getUpdates` long
        // poll. Telegram only allows one concurrent long poll per bot token; a
        // second one is rejected with 409. Surface this distinctly so the poll
        // loop can log a clear diagnosis instead of a generic error.
        let description = body.description.unwrap_or_default();
        if body.error_code == Some(409) {
            anyhow::bail!(
                "Telegram 409 Conflict: another bot instance is already polling \
                 `getUpdates` with this token (a second concurrent long poll is \
                 rejected). Stop the other process or wait for its stale poll to \
                 expire. ({description})"
            );
        }
        anyhow::bail!("Telegram getUpdates error: {}", description);
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
                "message_id": 1,
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
    fn test_parse_callback_query() {
        let json = r#"{
            "update_id": 42,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 6191763506},
                "data": "sess-abc",
                "message": {"chat": {"id": 6191763506}, "message_id": 987}
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let cb = update.callback_query.expect("callback_query present");
        assert_eq!(cb.id, "cb-1");
        assert_eq!(cb.data.as_deref(), Some("sess-abc"));
        assert_eq!(cb.from.map(|f| f.id), Some(6191763506));
        let cb_msg = cb.message.as_ref().expect("message present");
        let chat_id = cb_msg.chat.as_ref().map(|c| c.id);
        assert_eq!(chat_id, Some(6191763506));
        assert_eq!(cb_msg.message_id, Some(987));
    }

    #[test]
    fn test_parse_message_message_id() {
        let json = r#"{
            "update_id": 123,
            "message": {
                "text": "hello",
                "chat": {"id": 456},
                "message_id": 555,
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert_eq!(msg.text.as_deref(), Some("hello"));
        assert_eq!(msg.message_id, 555);
    }

    #[test]
    fn test_parse_message_caption_fallback() {
        // A media message (photo/document) has a caption but no `text`.
        let json = r#"{
            "update_id": 200,
            "message": {
                "caption": "  analyze this image  ",
                "chat": {"id": 456},
                "message_id": 7,
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert!(msg.text.is_none());
        assert_eq!(msg.inbound_text(), Some("analyze this image"));
    }

    #[test]
    fn test_inbound_text_prefers_text_and_trims() {
        let json = r#"{
            "update_id": 201,
            "message": {
                "text": "  hi  ",
                "caption": "ignored caption",
                "chat": {"id": 456},
                "message_id": 8,
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert_eq!(msg.inbound_text(), Some("hi"));
    }

    #[test]
    fn test_inbound_text_none_when_no_text_or_caption() {
        let json = r#"{
            "update_id": 202,
            "message": {
                "chat": {"id": 456},
                "message_id": 9,
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        assert!(msg.inbound_text().is_none());
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

    #[test]
    fn test_chunk_message_splits_long_text() {
        let short = "hello world";
        let chunks = chunk_message(short, MAX_MESSAGE_CHARS);
        assert_eq!(chunks, vec![short.to_string()]);

        // Build a text that must span multiple chunks.
        let long = "line one\n".repeat(3000);
        let chunks = chunk_message(&long, MAX_MESSAGE_CHARS);
        assert!(chunks.len() > 1, "expected multiple chunks, got {}", chunks.len());
        for c in &chunks {
            assert!(c.chars().count() <= MAX_MESSAGE_CHARS);
        }
        let joined: String = chunks.concat();
        assert_eq!(joined, long);
    }

    #[test]
    fn test_chunk_message_hard_splits_oversized_single_line() {
        let line = "x".repeat(10_000);
        let chunks = chunk_message(&line, 100);
        assert_eq!(chunks.len(), 100);
        for c in &chunks {
            assert_eq!(c.chars().count(), 100);
        }
        assert_eq!(chunks.concat(), line);
    }

    #[test]
    fn test_is_markdown_parse_error_detects_common_messages() {
        assert!(is_markdown_parse_error("can't parse entities"));
        assert!(is_markdown_parse_error("Bad Request: can't parse entities"));
        assert!(is_markdown_parse_error("cannot parse entities"));
        assert!(is_markdown_parse_error("parse: unexpected ... entity"));
        assert!(!is_markdown_parse_error("message text is empty"));
    }

    #[test]
    fn test_escape_markdown_escapes_control_chars() {
        assert_eq!(escape_markdown("plain text"), "plain text");
        assert_eq!(escape_markdown("a_b"), "a\\_b");
        assert_eq!(escape_markdown("a*b"), "a\\*b");
        assert_eq!(escape_markdown("`code`"), "\\`code\\`");
        assert_eq!(escape_markdown("[x]"), "\\[x\\]");
        // A backslash before an escapable char is doubled so it renders literally.
        assert_eq!(escape_markdown("_a_"), "\\_a\\_");
        // Two consecutive backslashes: the first precedes a `\` (doubled), the
        // second precedes `b` (kept single), so the value ends with three `\`.
        assert_eq!(escape_markdown(r"a\\b"), r"a\\\b");
    }

    #[test]
    fn test_escape_markdown_does_not_double_unrelated_backslashes() {
        // A backslash before a non-escapable char is already literal, so it is
        // NOT doubled: Windows paths and regexes render without `\\`.
        assert_eq!(escape_markdown(r"C:\Users\foo"), r"C:\Users\foo");
        assert_eq!(escape_markdown(r"\d+\.\d+"), r"\d+\.\d+");
        // Trailing backslash has no following char; stays single.
        assert_eq!(escape_markdown(r"path\"), r"path\");
    }

    #[test]
    fn test_escape_markdown_leaves_normal_plain_text_untouched() {
        // Ordinary characters and spaces pass through unescaped; only the
        // legacy-Markdown reserved set gets a backslash.
        assert_eq!(escape_markdown("hello world 123"), "hello world 123");
    }

    #[test]
    fn test_dc_candidate_list_is_non_empty_and_parses() {
        assert!(!TELEGRAM_DC_CANDIDATES.is_empty());
        for ip in TELEGRAM_DC_CANDIDATES {
            assert!(ip.parse::<std::net::IpAddr>().is_ok(), "bad candidate IP: {ip}");
        }
    }

    #[test]
    fn test_discovery_backoff_is_reasonable() {
        assert!(DISCOVERY_BACKOFF_SECS >= 30);
    }

    #[test]
    fn test_is_connectivity_error_classifies() {
        // Network-level failures should keep discovery going.
        for msg in [
            "error trying to connect: tcp connect error: Connection refused",
            "failed to lookup host: dns resolution failed",
            "tls handshake error: certificate mismatch",
            "operation timed out after 8000ms",
            "error sending request: deadline has elapsed",
            "network is unreachable",
            "no address found for host",
        ] {
            assert!(is_connectivity_error(&anyhow::anyhow!(msg)), "expected connectivity: {msg}");
        }
        // Auth errors should stop discovery.
        assert!(!is_connectivity_error(&anyhow::anyhow!(
            "Telegram auth failed: Unauthorized"
        )));
        // A successful result is never an error; guard the negative path too.
        assert!(!is_connectivity_error(&anyhow::anyhow!(
            "Telegram API error (400): Bad Request: can't parse entities"
        )));
    }

    #[test]
    fn test_discovery_candidate_ordering() {
        // Simulate the ordering used in discover_client: override first, then
        // default DNS, then the curated list.
        let override_ip = Some("10.0.0.1");
        let mut candidates: Vec<Option<String>> = Vec::new();
        if let Some(ip) = override_ip.filter(|ip| !ip.trim().is_empty()) {
            candidates.push(Some(ip.trim().to_string()));
        }
        candidates.push(None);
        for ip in TELEGRAM_DC_CANDIDATES {
            candidates.push(Some((*ip).to_string()));
        }
        // candidates.first() returns Option<&Option<String>>, need Some(&Option...)
        assert_eq!(candidates.first(), Some(&Some("10.0.0.1".to_string())));
        assert_eq!(candidates.get(1), Some(&None));
        assert_eq!(candidates.get(2), Some(&Some("149.154.167.220".to_string())));
        // The curated list must be tried before any arbitrary scan: total is
        // override + default-DNS + every candidate, and never a wider sweep.
        assert_eq!(candidates.len(), 2 + TELEGRAM_DC_CANDIDATES.len());
    }

    #[test]
    fn test_build_probe_client_rejects_non_ip() {
        // A non-IP override must fail to build so discovery skips it, not crash.
        assert!(build_probe_client(None, Some("not-an-ip")).is_err());
        // An empty override builds a default-DNS probe client.
        assert!(build_probe_client(None, Some("")).is_ok());
        assert!(build_probe_client(None, None).is_ok());
        // A valid pinned IP builds a probe client.
        assert!(build_probe_client(None, Some("149.154.167.220")).is_ok());
    }
}
