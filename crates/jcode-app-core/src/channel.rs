use crate::ambient_runner::AmbientRunnerHandle;
use crate::config::SafetyConfig;
use crate::logging;
use crate::telegram::escape_markdown_v2;
use crate::telegram::InlineKeyboardButton;
use crate::telegram::InlineKeyboardRow;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait MessageChannel: Send + Sync {
    fn name(&self) -> &str;

    fn is_send_enabled(&self) -> bool;

    fn is_reply_enabled(&self) -> bool;

    async fn send(&self, text: &str) -> anyhow::Result<()>;

    /// Poll the channel for inbound messages and react. `runner` is the ambient
    /// runner when ambient mode is available; pass `None` when the loop runs
    /// standalone (e.g. Telegram remote control with ambient disabled).
    ///
    /// Takes `Arc<Self>` so during the loop it can detach long-running message
    /// handling into its own task, keeping the poll loop responsive.
    async fn reply_loop(self: Arc<Self>, runner: Option<AmbientRunnerHandle>);
}

#[derive(Clone)]
pub struct ChannelRegistry {
    channels: Vec<Arc<dyn MessageChannel>>,
}

impl ChannelRegistry {
    pub fn from_config(config: &SafetyConfig) -> Self {
        let mut channels: Vec<Arc<dyn MessageChannel>> = Vec::new();

        if config.telegram_enabled
            && let (Some(token), Some(chat_id)) = (
                config.telegram_bot_token.clone(),
                config.telegram_chat_id.clone(),
            )
        {
            logging::info(&format!(
                "registering telegram notification channel reply_enabled={}",
                config.telegram_reply_enabled
            ));
            channels.push(Arc::new(TelegramChannel::with_connectivity(
                token,
                chat_id,
                config.telegram_reply_enabled,
                config.telegram_api_base.clone(),
                config.telegram_proxy.clone(),
                config.telegram_api_ip.clone(),
                config.telegram_allowed_user_id.clone(),
            )));
        }

        if config.discord_enabled
            && let (Some(token), Some(channel_id)) = (
                config.discord_bot_token.clone(),
                config.discord_channel_id.clone(),
            )
        {
            logging::info(&format!(
                "registering discord notification channel reply_enabled={}",
                config.discord_reply_enabled
            ));
            channels.push(Arc::new(DiscordChannel::new(
                token,
                channel_id,
                config.discord_reply_enabled,
                config.discord_bot_user_id.clone(),
            )));
        }

        if config.jade_relay_enabled {
            match (
                config.jade_relay_api_base.clone(),
                config.jade_relay_token.clone(),
                config.jade_relay_session_id.clone(),
            ) {
                (Some(api_base), Some(token), Some(session_id)) => {
                    // user_id defaults to the token id when not explicitly set.
                    let user_id = config
                        .jade_relay_user_id
                        .clone()
                        .or_else(|| config.jade_relay_token_id.clone())
                        .unwrap_or_else(|| "default".to_string());
                    logging::info(&format!(
                        "registering jade relay channel user={} session={} reply_enabled={}",
                        user_id, session_id, config.jade_relay_reply_enabled
                    ));
                    channels.push(Arc::new(JadeRelayChannel::new(
                        api_base,
                        token,
                        config.jade_relay_token_id.clone(),
                        user_id,
                        session_id,
                        config.jade_relay_reply_enabled,
                    )));
                }
                _ => {
                    logging::warn(
                        "jade_relay_enabled but api_base/token/session_id incomplete; skipping",
                    );
                }
            }
        }

        logging::debug(&format!(
            "channel registry initialized channel_count={}",
            channels.len()
        ));
        Self { channels }
    }

    pub fn send_all(&self, text: &str) {
        if tokio::runtime::Handle::try_current().is_err() {
            logging::warn("skipping channel send_all because no Tokio runtime is active");
            return;
        }
        for ch in self.channels.iter().filter(|c| c.is_send_enabled()) {
            let ch = Arc::clone(ch);
            let text = text.to_string();
            tokio::spawn(async move {
                logging::debug(&format!("sending notification via {}", ch.name()));
                if let Err(e) = ch.send(&text).await {
                    logging::error(&format!("{} notification failed: {}", ch.name(), e));
                }
            });
        }
    }

    pub fn spawn_reply_loops(&self, runner: Option<&AmbientRunnerHandle>) {
        for ch in self.channels.iter().filter(|c| c.is_reply_enabled()) {
            let ch = Arc::clone(ch);
            let runner = runner.cloned();
            tokio::spawn(async move {
                logging::info(&format!("{} reply loop spawned", ch.name()));
                let ch = Arc::clone(&ch);
                ch.reply_loop(runner).await;
            });
        }
    }

    pub fn channel_names(&self) -> Vec<String> {
        self.channels.iter().map(|c| c.name().to_string()).collect()
    }

    pub fn find_by_name(&self, name: &str) -> Option<Arc<dyn MessageChannel>> {
        let channel = self.channels.iter().find(|c| c.name() == name).cloned();
        if channel.is_none() {
            logging::debug(&format!("channel lookup missed name={name}"));
        }
        channel
    }

    pub fn send_enabled(&self) -> Vec<Arc<dyn MessageChannel>> {
        self.channels
            .iter()
            .filter(|c| c.is_send_enabled())
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Telegram channel
// ---------------------------------------------------------------------------

pub struct TelegramChannel {
    token: String,
    chat_id: String,
    reply_enabled: bool,
    api_base: Option<String>,
    allowed_user_id: Option<String>,
    /// HTTP client used for Bot API calls. Lazily (re)discovered: on first use a
    /// reachable data-center IP is auto-selected (see `client_or_default`), so a
    /// blocked default DC does not require manual `telegram_api_ip` configuration.
    client: tokio::sync::Mutex<reqwest::Client>,
    /// Proxy override retained for re-discovery when the cached client goes
    /// stale (connectivity failure).
    proxy: Option<String>,
    /// Explicit IP override (`[safety] telegram_api_ip`) tried first during
    /// discovery as a last-resort escape hatch. Usually `None`.
    api_ip: Option<String>,
    /// When discovery was last attempted (success or failure), used to throttle
    /// re-discovery after a full failure (see `DISCOVERY_BACKOFF_SECS`).
    last_discovery: tokio::sync::Mutex<std::time::Instant>,
    /// Whether discovery has been run at least once. Until then, `client_or_default`
    /// triggers a discovery sweep so a blocked default DC is worked around at
    /// first use rather than only after a failed call.
    discovered_once: tokio::sync::Mutex<bool>,
    /// Serializes inbound message handling for this chat. Because each message
    /// is handled in its own task (so the poll loop stays responsive), this
    /// lock ensures the replies to messages from one chat arrive in arrival
    /// order, like a single in-order processor.
    process_lock: tokio::sync::Mutex<()>,
    /// Tracks consecutive discovery failures for UX reporting and circuit behavior.
    consecutive_discovery_failures: tokio::sync::Mutex<u32>,
    /// Tracks bot authentication warnings to avoid spam. User-friendly warning
    /// messages are shown once per process to help users identify setup issues.
    /// Wrapped in a Mutex because `reply_loop` runs on `Arc<Self>` and needs
    /// `&mut` access to flip the `warned` flag across an `.await`.
    auth_warning_tracker: tokio::sync::Mutex<AuthWarningTracker>,
    /// Tracks pending confirmation state for destructive operations (/free, /abort).
    /// Wrapped in a Mutex for the same reason as `auth_warning_tracker`.
    confirmation_tracker: tokio::sync::Mutex<ConfirmationTracker>,
}

impl TelegramChannel {
    pub fn new(token: String, chat_id: String, reply_enabled: bool) -> Self {
        // Default connectivity: default API base, no proxy, no IP pin, no user whitelist.
        Self::with_connectivity(token, chat_id, reply_enabled, None, None, None, None)
    }

    /// Construct a Telegram channel with optional API-base, proxy, alternate-IP,
    /// and sender-whitelist overrides (from `[safety] telegram_api_base` /
    /// `telegram_proxy` / `telegram_api_ip` / `telegram_allowed_user_id`, or
    /// their env vars).
    #[allow(clippy::too_many_arguments)]
    pub fn with_connectivity(
        token: String,
        chat_id: String,
        reply_enabled: bool,
        api_base: Option<String>,
        proxy: Option<String>,
        api_ip: Option<String>,
        allowed_user_id: Option<String>,
    ) -> Self {
        let client = match crate::telegram::build_client(proxy.as_deref(), api_ip.as_deref()) {
            Ok(client) => client,
            Err(e) => {
                crate::logging::error(&format!(
                    "failed to build telegram client, falling back to shared client: {}",
                    e
                ));
                crate::provider::shared_http_client()
            }
        };
        Self {
            token,
            chat_id,
            reply_enabled,
            api_base,
            allowed_user_id,
            client: tokio::sync::Mutex::new(client),
            proxy,
            api_ip,
            last_discovery: tokio::sync::Mutex::new(std::time::Instant::now()),
            discovered_once: tokio::sync::Mutex::new(false),
            process_lock: tokio::sync::Mutex::new(()),
            consecutive_discovery_failures: tokio::sync::Mutex::new(0),
            auth_warning_tracker: tokio::sync::Mutex::new(AuthWarningTracker::default()),
            confirmation_tracker: tokio::sync::Mutex::new(ConfirmationTracker::new()),
        }
    }

    /// Run (or re-run) the discovery sweep, replacing the cached client with the
    /// first reachable DC (or the default client if all candidates fail). Marks
    /// discovery as done and records the attempt time for backoff.
    async fn run_discovery(&self) {
        let replacement = match crate::telegram::discover_client(
            &self.token,
            self.proxy.as_deref(),
            self.api_ip.as_deref(),
        )
        .await
        {
            Ok(c) => {
                *self.consecutive_discovery_failures.lock().await = 0;
                c
            }
            Err(_) => {
                let mut failures = self.consecutive_discovery_failures.lock().await;
                *failures = failures.saturating_add(1);
                crate::provider::shared_http_client()
            }
        };
        *self.client.lock().await = replacement;
        *self.discovered_once.lock().await = true;
        *self.last_discovery.lock().await = std::time::Instant::now();
    }

    /// Return a usable client, running discovery once at first use (or after the
    /// cache is invalidated) so a blocked default DC is transparently worked
    /// around. On a full discovery failure a default (DNS-resolved) client is
    /// returned so the calling operation still attempts to run.
    async fn client_or_default(&self) -> reqwest::Client {
        if !*self.discovered_once.lock().await {
            self.run_discovery().await;
        }
        self.client.lock().await.clone()
    }

    /// Drop the cached client so the next call re-discovers. Re-sweeps are
    /// throttled by `DISCOVERY_BACKOFF_SECS` (unless `force`) so a persistently
    /// blocked network does not trigger a slow candidate sweep on every poll.
    async fn invalidate_cache(&self, force: bool) {
        if !force {
            let last = *self.last_discovery.lock().await;
            if last.elapsed().as_secs() < crate::telegram::DISCOVERY_BACKOFF_SECS {
                return;
            }
        }
        self.run_discovery().await;
    }

    /// Handle a slash command received over Telegram, returning the reply text.
    /// Read-only commands: `/help`, `/status`, `/list`, `/whoami`. Write commands
    /// (create/resume/abort/free) act on the live session registry.
    async fn handle_command(
        &self,
        trimmed: &str,
        runner: Option<&AmbientRunnerHandle>,
    ) -> String {
        let (cmd, rest) = split_command(trimmed);
        match cmd.as_str() {
            "/help" | "/start" | "help" | "start" => HELP_TEXT.to_string(),
            "/list" | "/sessions" => {
                let args = rest.trim();
                self.send_session_picker(args).await;
                String::new()
            }
            "/status" => self.status_reply(runner).await,
            "/use" => self.use_session_reply(&rest).await,
            "/new" | "/start_new" => {
                let prompt = rest.trim();
                self.new_session_reply(prompt).await
            }
            "/history" => self.history_reply(&rest),
            "/find" | "/search" => {
                let q = rest.trim();
                self.find_session_reply(q).await
            }
            "/whoami" => self.whoami_reply(),
            "/peek" => self.peek_reply(&rest),
            "/live" | "/ls" => {
                self.send_live_sessions_picker().await;
                String::new()
            }
            "/free" => {
                let arg = rest.trim();
                self.free_session_reply(arg).await
            }
            "/clear" | "/stop" => {
                let cleared =
                    crate::server::telegram_control::active_session_for(&self.chat_id).is_some();
                crate::server::telegram_control::clear_active_session(&self.chat_id);
                if cleared {
                    "✓ Cleared the active session. Use `/use <id>` to select another.".to_string()
                } else {
                    format!("No active session to clear{}", help_footer())
                }
            }
            "/abort" => self.abort_reply().await,
            "/cancel" => self.cancel_reply().await,
            "/resume" => {
                let prompt = rest.trim();
                self.resume_reply(prompt).await
            }
            "/confirm" => self.confirm_reply().await,
            _ => format!(
                "Unknown command `{}`. Use `/help` for available commands{}.",
                cmd,
                help_footer()
            ),
        }
    }

    /// Send an inline-keyboard session picker to the chat. Supports filter
    /// flags: `--saved` (saved sessions only), `--today` (active in the last
    /// 24h). Each button's `callback_data` is the session id, so tapping it
    /// selects that session.
    async fn send_session_picker(&self, args: &str) {
        let client = self.client_or_default().await;
        // Determine filter mode from arguments
        let today_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let one_day_ago_ms = today_ms - 24 * 60 * 60 * 1000;
        // Fetch a larger candidate pool for the filtered modes: the SQL `recent`
        // applies `LIMIT` before our Rust-side `--saved`/`--today` filter, so a
        // small pool could hide matching sessions that sit past the cap. We cap
        // the displayed rows afterwards.
        let filtered_mode = args.contains("--saved") || args.contains("--today");
        let limit = if filtered_mode { 50 } else { 12 };

        let entries = match crate::recent_session_index::recent(limit) {
            Ok(list) => list,
            Err(e) => {
                logging::warn(&format!("telegram session picker index error: {e}"));
                return;
            }
        };

        // Filter based on flags
        let filtered: Vec<_> = if args.contains("--saved") {
            entries.into_iter().filter(|s| s.saved).take(24).collect()
        } else if args.contains("--today") {
            entries
                .into_iter()
                .filter(|s| {
                    s.last_active_at_ms
                        .map(|t| t >= one_day_ago_ms)
                        .unwrap_or(false)
                })
                .take(12)
                .collect()
        } else {
            entries
        };

        let sessions = filtered;
        if sessions.is_empty() {
            let empty_msg = if args.contains("--saved") {
                "📚 **No Saved Sessions Found**\n\nNo saved sessions yet.\n▪️ Save a session with `/save` in the TUI\n▪️ Use `/new` to start a new session\n▪️ Use `/help` for all available commands"
            } else if args.contains("--today") {
                "📚 **No Recent Sessions**\n\nNo sessions active in the last 24 hours.\n▪️ Send any message to create a new session\n▪️ Use `/new` for an empty session\n▪️ Use `/list` to see recent sessions\n▪️ Use `/help` for all available commands"
            } else {
                "📚 **No Sessions Found**\n\nYou haven't started any conversations yet.\n▪️ Send any message to create a new session\n▪️ Use `/new` for an empty session\n▪️ Use `/help` for all available commands"
            };
            let _ = self
                .send_reply(empty_msg, None)
                .await;
            return;
        }
        use crate::telegram::{InlineKeyboardButton, InlineKeyboardRow};
        let active = crate::server::telegram_control::active_session_for(&self.chat_id);
        let mut rows: Vec<InlineKeyboardRow> = Vec::new();
        for s in sessions.iter() {
            let title: String = s
                .display_title()
                .unwrap_or(s.session_id.as_str())
                .chars()
                .take(30)
                .collect();
            let short: String = s.session_id.chars().take(8).collect();
            let prefix = if active.as_deref() == Some(s.session_id.as_str()) {
                "✅ "
            } else if s.saved {
                "⭐ "
            } else {
                ""
            };
            rows.push(vec![InlineKeyboardButton {
                text: format!("{}{} ({})", prefix, title, short),
                callback_data: s.session_id.clone(),
            }]);
        }
        // Enhanced UX: informative header with count/instructions
        let filter_label = if args.contains("--saved") {
            "saved"
        } else if args.contains("--today") {
            "recent (24h)"
        } else {
            "recent"
        };
        let header = if sessions.len() == 1 {
            format!("📚 1 {} session available:", filter_label)
        } else {
            format!("📚 {} {} sessions available:", sessions.len(), filter_label)
        };
        let mut picker_header = header;
        if active.is_some() {
            picker_header.push_str("\n✅ = active session");
        }
        if args.contains("--saved") {
            picker_header.push_str("\n⭐ = saved session");
        }
        let _ = crate::telegram::send_message_with_keyboard(
            &client,
            &self.token,
            &self.chat_id,
            &picker_header,
            &rows,
            self.api_base.as_deref(),
        )
        .await;
    }

    /// Handle an inline-keyboard tap (`callback_query`). `callback_data` is a
    /// session id; selecting it sets the active session for this chat.
    async fn handle_callback_query(&self, cb: crate::telegram::CallbackQuery) {
        let client = self.client_or_default().await;
        // A tap on a picker button must always be acknowledged so Telegram
        // clears the button's loading spinner. A callback whose message or
        // chat id is missing is still answered (a no-op toast) so the button
        // never stays stuck in its loading state.
        let Some(msg) = cb.message.as_ref() else {
            let _ = crate::telegram::answer_callback_query(
                &client,
                &self.token,
                &cb.id,
                "",
                self.api_base.as_deref(),
            )
            .await;
            return;
        };
        let Some(chat_id) = msg.chat.as_ref().map(|c| c.id.to_string()) else {
            let _ = crate::telegram::answer_callback_query(
                &client,
                &self.token,
                &cb.id,
                "",
                self.api_base.as_deref(),
            )
            .await;
            return;
        };
        if chat_id != self.chat_id {
            // Not our chat; do not act on or acknowledge a foreign callback.
            return;
        }
        // The message id of the tapped picker message, used to collapse its
        // inline keyboard after a selection.
        let picker_message_id = msg.message_id;

        if !self.is_allowed_sender(cb.from.as_ref()) {
            logging::warn("ignoring callback_query from disallowed sender");
            let _ = crate::telegram::answer_callback_query(
                &client,
                &self.token,
                &cb.id,
                "Not allowed",
                self.api_base.as_deref(),
            )
            .await;
            return;
        }
        let Some(data) = cb.data.as_deref() else {
            let _ = crate::telegram::answer_callback_query(
                &client,
                &self.token,
                &cb.id,
                "",
                self.api_base.as_deref(),
            )
            .await;
            return;
        };

        // A tap on a `/free` picker button carries a `__free__<session_id>`
        // payload: drop that live session instead of selecting it.
        if let Some(id) = data.strip_prefix("__free__") {
            let id = id.trim().to_string();
            let removed = crate::server::telegram_control::free_session_for_control(&id).await;
            let ack = if removed {
                format!("🗑️ Freed `{}`", short_id(&id))
            } else {
                format!("⚠️ `{}` already gone", short_id(&id))
            };
            let _ = crate::telegram::answer_callback_query(
                &client,
                &self.token,
                &cb.id,
                &ack,
                self.api_base.as_deref(),
            )
            .await;
            if let Some(message_id) = picker_message_id
                && let Ok(chat_id_num) = chat_id.parse::<i64>()
            {
                crate::telegram::edit_message_reply_markup(
                    &client,
                    &self.token,
                    chat_id_num,
                    message_id,
                    self.api_base.as_deref(),
                )
                .await;
            }
            return;
        }

        let session_id = data.trim().to_string();
        crate::server::telegram_control::set_active_session(&self.chat_id, &session_id);
        let short_id = short_id(&session_id);
        let ack = format!("✅ Session `{}` selected", short_id);
        crate::logging::info(&format!("telegram callback selected session={session_id}"));
        let _ = crate::telegram::answer_callback_query(
            &client,
            &self.token,
            &cb.id,
            &ack,
            self.api_base.as_deref(),
        )
        .await;
        // Collapse the picker's inline keyboard now that a session was chosen.
        if let Some(message_id) = picker_message_id
            && let Ok(chat_id_num) = chat_id.parse::<i64>()
        {
            crate::telegram::edit_message_reply_markup(
                &client,
                &self.token,
                chat_id_num,
                message_id,
                self.api_base.as_deref(),
            )
            .await;
        }
        let confirmation = format!(
            "🎉 Success! Active session is `{}`.\n\
   💬 Send any message to talk to it\n\
   📜 Use `/history` to view conversation history\n\
   🔄 Use `/clear` to stop talking to this session\n\
   ➕ Use `/new` to start a different session",
            short_id
        );
        let _ = self
            .send_reply(&confirmation, picker_message_id)
            .await;
    }

    /// `/use <n-or-id>`: select the active session for this chat.
    async fn use_session_reply(&self, arg: &str) -> String {
        let arg = arg.trim();
        if arg.is_empty() {
            return "Usage: `/use <n-or-id>`. Run `/list` to see numbered sessions.".to_string();
        }
        let session_id = if let Ok(n) = arg.parse::<usize>() {
            match crate::recent_session_index::recent(200) {
                Ok(list) if n >= 1 && n <= list.len() => list[n - 1].session_id.clone(),
                Ok(_) => return format!("`{n}` is out of range for `/list`.").to_string(),
                Err(e) => return format!("⚠️ Could not read the session index: {e}").to_string(),
            }
        } else {
            match self.resolve_session_id(arg).await {
                Ok(id) => id,
                Err(e) => return e,
            }
        };
        crate::server::telegram_control::set_active_session(&self.chat_id, &session_id);
        format!(
            "✅ Selected session `{}`. Use `/history` to view it.",
            short_id(&session_id)
        )
    }

    /// `/new [prompt]`: create a fresh session, make it the active one for this
    /// chat, and (optionally) run the first turn with `prompt`.
    async fn new_session_reply(&self, arg: &str) -> String {
        // If an opening prompt is supplied, run the first turn headlessly with
        // the typing indicator so the user sees the bot working.
        if arg.is_empty() {
            return match crate::server::telegram_control::create_session_for_control(None).await {
                Ok((id, _)) => {
                    crate::server::telegram_control::set_active_session(&self.chat_id, &id);
                    format!(
                        "✅ Created new session `{}`. Send a message to talk to it.",
                        short_id(&id)
                    )
                }
                Err(e) => format!("⚠️ Could not create a session: {e}"),
            };
        }
        match self
            .with_typing(async {
                crate::server::telegram_control::create_session_for_control(Some(arg)).await
            })
            .await
        {
            Ok((id, reply)) => {
                crate::server::telegram_control::set_active_session(&self.chat_id, &id);
                agent_reply_message(&id, &reply)
            }
            Err(e) => format!("⚠️ Could not create a session: {e}"),
        }
    }

    /// `/history [n]`: show recent messages of the active session.
    fn history_reply(&self, arg: &str) -> String {
        let Some(session_id) = crate::server::telegram_control::active_session_for(&self.chat_id)
        else {
            return format!("No active session. Use `/use <n>` after `/list`.{}", help_footer());
        };
        let limit = arg
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=50).contains(n))
            .unwrap_or(10);
        match crate::server::telegram_control::render_session_history(&session_id, limit) {
            Ok(text) if text != "(no visible messages)" => {
                // Escape only the dynamic message bodies so the history cannot
                // break parse_mode=MarkdownV2, while keeping the role labels bold.
                format!("📜 [{}]\n{}", short_id(&session_id), escape_history_entries(&text))
            }
            Ok(text) => format!("[{}] {}", short_id(&session_id), text),
            Err(e) => format!(
                "⚠️ Could not read history for `{}`: {}",
                short_id(&session_id),
                e
            ),
        }
    }

    /// `/peek [n]`: show a compact preview (first user msg + first assistant
    /// msg) of the active session so the user can remember what it is about
    /// before committing to `/use`. No inline picker is needed — just send
    /// the two-line summary directly.
    fn peek_reply(&self, arg: &str) -> String {
        let Some(session_id) = crate::server::telegram_control::active_session_for(&self.chat_id)
        else {
            return format!("No active session. Use `/use <n>` after `/list`{}", help_footer());
        };
        let limit = arg
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=20).contains(n))
            .unwrap_or(10);
        let entries = match crate::server::telegram_control::render_session_history(&session_id, limit) {
            Ok(text) => text,
            Err(_) => return format!("⚠️ Could not read session `{}` for preview.", short_id(&session_id)),
        };
        // Split the renderer's block into preview lines: strip each role prefix
        // and escape the raw content (see peek_preview_lines) so the preview
        // neither drops to a doubled role label nor breaks parse_mode=MarkdownV2.
        let lines = peek_preview_lines(&entries);
        let preview = if lines.len() >= 2 {
            format!("👤 *you:* {}\n🤖 *jcode:* {}", lines[0], lines[1])
        } else if lines.len() == 1 {
            format!("👤 *you:* {}", lines[0])
        } else {
            "(no messages to preview)".to_string()
        };
        let message_count = lines.len();
        format!("📖 **Preview** `{}`, {} messages\n{}",
            short_id(&session_id),
            message_count,
            preview)
    }

    /// `/find <query>`: search recent sessions by title/working-dir/id and send
    /// a tap-to-select inline picker of the matches.
    async fn find_session_reply(&self, query: &str) -> String {
        let query = query.trim();
        if query.is_empty() {
            return "Usage: `/find <text>`. Searches session titles and ids.".to_string();
        }
        let entries = match crate::recent_session_index::search(query, 24) {
            Ok(list) => list,
            Err(e) => {
                logging::warn(&format!("telegram /find index error: {e}"));
                return format!("⚠️ Could not search sessions: {e}");
            }
        };
        if entries.is_empty() {
            return format!("🔍 No sessions match `{}`.", escape_markdown_v2(query));
        }
        let active = crate::server::telegram_control::active_session_for(&self.chat_id);
        let mut rows: Vec<InlineKeyboardRow> = Vec::new();
        for s in entries.iter().take(24) {
            let title: String = s
                .display_title()
                .unwrap_or(s.session_id.as_str())
                .chars()
                .take(30)
                .collect();
            let short: String = s.session_id.chars().take(8).collect();
            let prefix = if active.as_deref() == Some(s.session_id.as_str()) {
                "✅ "
            } else {
                ""
            };
            rows.push(vec![InlineKeyboardButton {
                text: format!("{}{} ({})", prefix, title, short),
                callback_data: s.session_id.clone(),
            }]);
        }
        let client = self.client_or_default().await;
        let header = format!("🔍 {} match(es) for `{}`:", rows.len(), escape_markdown_v2(query));
        let _ = crate::telegram::send_message_with_keyboard(
            &client,
            &self.token,
            &self.chat_id,
            &header,
            &rows,
            self.api_base.as_deref(),
        )
        .await;
        String::new()
    }

    /// `/whoami`: report this chat's id and the sender's user id so the user can
    /// paste them into `[safety] telegram_chat_id` / `telegram_allowed_user_id`.
    fn whoami_reply(&self) -> String {
        let mut msg = String::from("👤 *You are:*\n");
        msg.push_str(&format!(
            "• chat\\_id: `{}`\n",
            escape_markdown_v2(&self.chat_id)
        ));
        if let Some(uid) = self.allowed_user_id.as_ref() {
            msg.push_str(&format!(
                "• configured allowed\\_user\\_id: `{}`\n",
                escape_markdown_v2(uid)
            ));
        } else {
            msg.push_str("• allowed\\_user\\_id: _(not set — any sender in this chat is accepted)_\n");
        }
        msg.push_str("\n📋 Copy these into your config:\n");
        msg.push_str(&format!(
            "```\n[safety]\ntelegram_chat_id = \"{}\"\n```",
            escape_markdown_v2(&self.chat_id)
        ));
        msg
    }

    /// `/free <id-or-prefix>`: drop a live (headless) session from the in-memory
    /// registry so it no longer consumes resources. Use `/live` to list ids.
    /// Shows a confirmation prompt; use `/confirm` to execute.
    async fn free_session_reply(&self, arg: &str) -> String {
        let arg = arg.trim();
        if arg.is_empty() {
            return "Usage: `/free <session-id-or-prefix>`. List live sessions with `/live`.".to_string();
        }
        let Some(sessions) = crate::server::telegram_control::live_sessions_snapshot().await else {
            return "Telegram control is not wired to a server runtime.".to_string();
        };
        let matches: Vec<String> = if let Some(exact) = sessions.iter().find(|id| id.as_str() == arg)
        {
            vec![exact.clone()]
        } else {
            sessions
                .iter()
                .filter(|id| id.starts_with(arg))
                .cloned()
                .collect()
        };
        match matches.len() {
            0 => format!("No live session matches `{}`.", escape_markdown_v2(arg)),
            1 => {
                let id = &matches[0];
                // Show confirmation prompt instead of executing directly
                let mut tracker = self.confirmation_tracker.lock().await;
                let prompt = tracker.request("free", id.clone());
                drop(tracker);
                let _ = self.send_reply(&prompt, None).await;
                String::new()
            }
            _ => format!(
                "`{}` matches {} live sessions; use a longer prefix.",
                escape_markdown_v2(arg),
                matches.len()
            ),
        }
    }

    /// `/abort` (alias `/cancel`): request a graceful stop of the active
    /// session's in-flight turn. Shows a confirmation prompt first so the
    /// user must tap `/confirm` to actually trigger the abort.
    async fn abort_reply(&self) -> String {
        let Some(session_id) =
            crate::server::telegram_control::active_session_for(&self.chat_id)
        else {
            return format!("No active session to abort. Use `/use <n>` first{}", help_footer());
        };
        let mut tracker = self.confirmation_tracker.lock().await;
        let prompt = tracker.request("abort", session_id.clone());
        drop(tracker);
        // Send the confirmation prompt as a reply so the user can tap /confirm.
        let _ = self.send_reply(&prompt, None).await;
        String::new()
    }

    /// `/confirm` (alias for tapping the confirm button): execute the pending
    /// destructive action stored in `confirmation_tracker`.
    async fn confirm_reply(&self) -> String {
        let mut tracker = self.confirmation_tracker.lock().await;
        let Some((action, session_id)) = tracker.verify("__confirm__") else {
            return "⚠️ No pending confirmation found or it has expired.".to_string();
        };
        drop(tracker);
        match action {
            "abort" => {
                let signaled =
                    crate::server::telegram_control::request_graceful_shutdown_for_control(&session_id).await;
                if signaled {
                    format!(
                        "🛑 Abort confirmed for `{}`. The agent will stop at the next safe point.",
                        short_id(&session_id)
                    )
                } else {
                    format!(
                        "⚠️ `{}` is not a live session (it was resumed headlessly or has ended).",
                        short_id(&session_id)
                    )
                }
            }
            "free" => {
                let removed = crate::server::telegram_control::free_session_for_control(&session_id).await;
                if removed {
                    // Also clear it if it was the active session for this chat.
                    if crate::server::telegram_control::active_session_for(&self.chat_id).as_deref()
                        == Some(session_id.as_str())
                    {
                        crate::server::telegram_control::clear_active_session(&self.chat_id);
                    }
                    format!("🗑️ Session `{}` freed.", short_id(&session_id))
                } else {
                    format!("⚠️ Could not free `{}` (already gone?).", short_id(&session_id))
                }
            }
            _ => unreachable!(),
        }
    }

    /// `/cancel`: cancel a pending `/free` or `/abort` confirmation.
    async fn cancel_reply(&self) -> String {
        let mut tracker = self.confirmation_tracker.lock().await;
        if tracker.clear() {
            "✅ Cancelled pending confirmation.".to_string()
        } else {
            "⚠️ No pending confirmation to cancel.".to_string()
        }
    }

    /// `/live` (alias `/ls`): list the sessions currently held in the in-memory
    /// live registry (including Telegram-spawned headless ones) as a picker so
    /// the user can `/free` or `/use` them.
    async fn send_live_sessions_picker(&self) {
        let client = self.client_or_default().await;
        let Some(sessions) = crate::server::telegram_control::live_sessions_snapshot().await else {
            let _ = self
                .send_reply("⚠️ Telegram control is not wired to a server runtime.", None)
                .await;
            return;
        };
        if sessions.is_empty() {
            let _ = self
                .send_reply(
                    "🟢 No live sessions. Start one with `/new` or `/use <n>`.",
                    None,
                )
                .await;
            return;
        }
        let active = crate::server::telegram_control::active_session_for(&self.chat_id);
        let mut rows: Vec<InlineKeyboardRow> = Vec::new();
        for id in sessions.iter() {
            let short: String = id.chars().take(8).collect();
            let prefix = if active.as_deref() == Some(id.as_str()) {
                "✅ "
            } else {
                ""
            };
            rows.push(vec![InlineKeyboardButton {
                text: format!("{}🗑️ {} (free)", prefix, short),
                callback_data: format!("__free__{}", id),
            }]);
        }
        let header = format!("🟢 {} live session(s):", sessions.len());
        let _ = crate::telegram::send_message_with_keyboard(
            &client,
            &self.token,
            &self.chat_id,
            &header,
            &rows,
            self.api_base.as_deref(),
        )
        .await;
    }

    /// `/resume <session-id> <prompt>`: send a prompt to a live session
    /// headlessly and return the assistant's reply (prefix id allowed).
    async fn resume_reply(&self, args: &str) -> String {
        let args = args.trim();
        if args.is_empty() {
            return "Usage: `/resume <session-id> <prompt>`. Run `/list` first.".to_string();
        }
        let (ref_token, prompt) = match args.find(char::is_whitespace) {
            Some(idx) => (args[..idx].trim(), args[idx..].trim()),
            None => (args, "Continue"),
        };
        let prompt = if prompt.is_empty() { "Continue" } else { prompt };
        let session_id = match self.resolve_session_id(ref_token).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        crate::server::telegram_control::set_active_session(&self.chat_id, &session_id);
        crate::logging::info(&format!(
            "telegram /resume session={} chars={}",
            session_id,
            prompt.chars().count()
        ));
        // Stream progress into a single placeholder message the user can watch.
        self.stream_reply_to_session(None, &session_id, prompt).await;
        String::new()
    }

    /// Resolve a user-supplied session reference (full id or unique prefix)
    /// against the live session registry.
    async fn resolve_session_id(&self, reference: &str) -> Result<String, String> {
        let Some(sessions) = crate::server::telegram_control::live_session_ids().await else {
            return Err("Telegram control is not wired to a server runtime.".to_string());
        };
        let reference = reference.trim();
        if let Some(id) = sessions.iter().find(|id| id.as_str() == reference) {
            return Ok(id.clone());
        }
        let matches: Vec<&String> = sessions
            .iter()
            .filter(|id| id.starts_with(reference))
            .collect();
        match matches.len() {
            0 => Err(format!(
                "No live session matches `{}`. Use `/list`, then `/use <n>`, or pick a live session id.",
                crate::telegram::escape_markdown_v2(reference)
            )),
            1 => Ok(matches[0].clone()),
            _ => Err(format!(
                "`{}` matches {} live sessions; use a longer prefix.",
                crate::telegram::escape_markdown_v2(reference),
                matches.len()
            )),
        }
    }

    /// `/status`: report ambient mode availability, session counts, and remote-control readiness.
    async fn status_reply(&self, runner: Option<&AmbientRunnerHandle>) -> String {
        let active = crate::server::telegram_control::active_session_for(&self.chat_id);
        let ambient = if let Some(r) = runner {
            let running = r.is_running().await;
            if running { "running" } else { "initialized" }
        } else {
            "disabled"
        };
        let active_line = match active {
            Some(id) => format!("*Active session:* `{}` (use `/history`)", short_id(&id)),
            None => "*Active session:* none (use `/use`)".to_string(),
        };
        // Enhanced UX: rich, scannable status with health indicators and tips.
        let discovery = if *self.discovered_once.lock().await {
            if self.api_ip.as_deref().is_some_and(|ip| !ip.trim().is_empty()) {
                format!("pinned to `{}`", self.api_ip.as_deref().unwrap())
            } else {
                "auto-discovered".to_string()
            }
        } else {
            "discovering…".to_string()
        };
        let since = self.last_discovery.lock().await.elapsed();
        let failures = *self.consecutive_discovery_failures.lock().await;
        let discovery_line = if failures > 0 {
            format!(
                "*Connection:* {} (re-checked {}s ago, {} recent failure(s))",
                discovery,
                since.as_secs(),
                failures
            )
        } else {
            format!(
                "*Connection:* {} (re-checked {}s ago)",
                discovery,
                since.as_secs()
            )
        };
        // Honest health: actually probe auth (getMe) and the session store
        // rather than printing a static "all green". Cheap and worth it because
        // a silently-dead bot is the #1 Telegram support complaint.
        let (auth_ok, auth_detail) = self.live_auth_status().await;
        let store_ok = crate::recent_session_index::recent(1).map(|r| !r.is_empty()).unwrap_or(false);
        let live_count = crate::server::telegram_control::live_session_count()
            .await
            .unwrap_or(0);
        let health_line = format!(
            "🔍 *Health:* auth {} · session store {} · {} live session(s)",
            if auth_ok { "ok" } else { "FAIL" },
            if store_ok { "ready" } else { "unavailable" },
            live_count
        );
        let auth_hint = if !auth_ok {
            format!("\n⚠️ *Auth:* {}", escape_markdown_v2(&auth_detail))
        } else {
            String::new()
        };
        // Count saved vs recent sessions for richer status
        let recent_entries = crate::recent_session_index::recent(100).unwrap_or_default();
        let total_recent = recent_entries.len();
        let total_saved = recent_entries.iter().filter(|s| s.saved).count();
        let has_pending_confirm = self.confirmation_tracker.lock().await.pending.is_some();
        let confirm_line = if has_pending_confirm {
            "⚠️ Pending confirmation (use /confirm or /cancel)"
        } else {
            "No pending confirmations"
        };
        format!(
            "🤖 *jcode Telegram control*\n\
             *Ambient mode:* {}\n\
             {}\n\
             {}\n\n\
             {}{}\n\n\
             📊 *Sessions:* {} total, {} saved, {} live\n\
             🛡️ *Safety:* {}\n\n\
             📋 *Commands:* /list [--saved|--today] /find /use /new /peek /history /resume /live /free /abort /clear /whoami /status /help\n\
             💡 Tip: send any message to talk to a session, or `/list` to browse.",
            ambient, active_line, discovery_line, health_line, auth_hint,
            total_recent, total_saved, live_count, confirm_line
        )
    }

    /// Probe bot auth live (calls `getMe`) so `/status` reports reality rather
    /// than a cached assumption. Returns (ok, detail).
    async fn live_auth_status(&self) -> (bool, String) {
        let client = self.client_or_default().await;
        match crate::telegram::verify_bot_auth(&client, &self.token, self.api_base.as_deref()).await {
            Ok(id) => (true, id.username.unwrap_or_else(|| "ok".to_string())),
            Err(e) => (false, e.to_string()),
        }
    }

    /// Enforce the sender whitelist. When no whitelist is configured, any
    /// sender in the configured chat is accepted for backwards compatibility.
    fn is_allowed_sender(&self, from: Option<&crate::telegram::TelegramFrom>) -> bool {
        match &self.allowed_user_id {
            Some(allow) => {
                let allow = allow.trim();
                if allow.is_empty() {
                    return true;
                }
                from.map(|f| f.id.to_string() == allow).unwrap_or(false)
            }
            None => true,
        }
    }

    /// Fire a one-shot `typing` chat action so the user sees a live indicator
    /// while the bot is processing. Non-fatal (errors are swallowed).
    async fn show_typing(&self) {
        let client = self.client_or_default().await;
        let _ = crate::telegram::send_chat_action(
            &client,
            &self.token,
            &self.chat_id,
            "typing",
            self.api_base.as_deref(),
        )
        .await;
    }

    /// Send a bot message to the chat, optionally threading it as a reply to the
    /// message `reply_to_message_id` that triggered it.
    async fn send_reply(&self, text: &str, reply_to_message_id: Option<i64>) -> anyhow::Result<()> {
        let client = self.client_or_default().await;
        crate::telegram::send_message_with_base(
            &client,
            &self.token,
            &self.chat_id,
            text,
            self.api_base.as_deref(),
            reply_to_message_id,
        )
        .await
    }

    /// Run `fut`, showing a periodic `typing` indicator the whole time and
    /// cancelling it when the future completes. Returns the future's output.
    async fn with_typing<Fut, T>(&self, fut: Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        let client = self.client_or_default().await;
        let token = self.token.clone();
        let chat_id = self.chat_id.clone();
        let api_base = self.api_base.clone();
        let typing = tokio::spawn(async move {
            loop {
                let _ = crate::telegram::send_chat_action(
                    &client,
                    &token,
                    &chat_id,
                    "typing",
                    api_base.as_deref(),
                )
                .await;
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
        });
        let result = fut.await;
        typing.abort();
        result
    }

    /// Process one inbound Telegram message. Runs under the per-channel
    /// processing lock so replies arrive in order. Returns `Ok` when handling
    /// completed (including when a message was legitimately ignored), or `Err`
    /// on an unexpected internal failure.
    async fn handle_inbound_message(
        &self,
        msg: crate::telegram::TelegramMessage,
        reply_to: Option<i64>,
        runner: Option<&AmbientRunnerHandle>,
    ) -> anyhow::Result<()> {
        let _guard = self.process_lock.lock().await;

        // Only react to the configured chat, and only to allowed senders.
        if msg.chat.id.to_string() != self.chat_id {
            return Ok(());
        }
        if !self.is_allowed_sender(msg.from.as_ref()) {
            logging::warn(&format!(
                "ignoring telegram message from disallowed sender from={:?}",
                msg.from.as_ref().map(|f| f.id)
            ));
            return Ok(());
        }

        // Fall back to a media caption when there is no text body, so attached
        // files with a caption still drive the session instead of being dropped.
        let Some(trimmed) = msg.inbound_text() else {
            logging::debug("ignoring telegram message with no text or caption");
            return Ok(());
        };

        if let Some(req_id) = crate::notifications::extract_permission_id(trimmed) {
            let (approved, message) =
                crate::notifications::parse_permission_reply(trimmed);
            // Record the approving sender's id in the decision audit trail so
            // it is clear *who* decided (not just via which channel). The
            // sender is already gated by `is_allowed_sender` above; embedding
            // the id makes the decision attributable and auditable.
            let via = match msg.from.as_ref() {
                Some(from) => format!("telegram_reply:{}", from.id),
                None => "telegram_reply".to_string(),
            };
            if let Err(e) =
                crate::safety::record_permission_via_file(&req_id, approved, &via, message)
            {
                logging::error(&format!(
                    "Failed to record permission from Telegram for {}: {}",
                    req_id, e
                ));
            } else {
                logging::info(&format!(
                    "Permission {} via Telegram: {}",
                    if approved { "approved" } else { "denied" },
                    req_id
                ));
                let _ = self
                    .send_reply(
                        &format!(
                            "✅ Permission {} for `{}`",
                            if approved { "approved" } else { "denied" },
                            req_id
                        ),
                        reply_to,
                    )
                    .await;
            }
            return Ok(());
        }

        if trimmed.starts_with('/') {
            let reply = self.handle_command(trimmed, runner).await;
            if !reply.is_empty() {
                let _ = self.send_reply(&reply, reply_to).await;
            }
            return Ok(());
        }

        if let Some(active_id) =
            crate::server::telegram_control::active_session_for(&self.chat_id)
        {
            self.stream_reply_to_session(reply_to, &active_id, trimmed).await;
            return Ok(());
        }

        if let Some(runner) = runner {
            self.show_typing().await;
            let injected = runner.inject_message(trimmed, "telegram").await;
            logging::info(&format!(
                "telegram reply injected into session injected={}",
                injected
            ));
            let ack = if injected {
                format!(
                    "💬 Message sent to active session: _{}_",
                    crate::telegram::escape_markdown_v2(trimmed)
                )
            } else {
                format!(
                    "📋 Message queued, waking agent: _{}_",
                    crate::telegram::escape_markdown_v2(trimmed)
                )
            };
            let _ = self.send_reply(&ack, reply_to).await;
        } else {
            let _ = self
                .send_reply(
                    "ℹ️ Select a session first: use `/list` then `/use <n>`, or run `/help`.",
                    reply_to,
                )
                .await;
        }
        Ok(())
    }
}

/// Split a Telegram command line into (command, rest-of-args), matching bot
/// commands like `/status`, `/list 5`, or a bare `/start`. A trailing
/// `@botname` on the command word is stripped.
fn split_command(line: &str) -> (String, String) {
    let line = line.trim();
    let (word, rest) = match line.find(char::is_whitespace) {
        Some(idx) => (&line[..idx], line[idx..].trim()),
        None => (line, ""),
    };
    let cmd = word.split('@').next().unwrap_or(word).to_string();
    (cmd, rest.to_string())
}

/// First 8 characters of a session id, for compact display.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Split a `render_session_history` block into preview lines: one per
/// message paragraph, with the renderer's role prefix (`🧑 *you:* ` /
/// `🤖 *jcode:* `) stripped and each body's MarkdownV2-reserved characters
/// escaped. Used by `/peek` so the preview neither doubles the role label nor
/// breaks `parse_mode=MarkdownV2` with raw session content.
fn peek_preview_lines(entries: &str) -> Vec<String> {
    entries
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .map(|para| {
            let text = para
                .strip_prefix("🧑 *you:* ")
                .or_else(|| para.strip_prefix("🤖 *jcode:* "))
                .unwrap_or(para);
            crate::telegram::escape_markdown_v2(text)
        })
        .collect()
}

/// Escape the body text of a `render_session_history` block while preserving
/// each line's role prefix (`🧑 *you:* <text>` / `🤖 *jcode:* <text>`). The
/// unchanged role markers stay bold; only the dynamic body is escaped so the
/// history cannot break `parse_mode=MarkdownV2`.
fn escape_history_entries(entries: &str) -> String {
    entries
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .map(|para| {
            if let Some(body) = para.strip_prefix("🧑 *you:* ") {
                format!("🧑 *you:* {}", crate::telegram::escape_markdown_v2(body))
            } else if let Some(body) = para.strip_prefix("🤖 *jcode:* ") {
                format!("🤖 *jcode:* {}", crate::telegram::escape_markdown_v2(body))
            } else {
                crate::telegram::escape_markdown_v2(para)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Returns a small guidance footer for use after error or help messages,
/// so users know where to find available commands when they hit a dead end.
fn help_footer() -> &'static str {
    "\n💡 Tip: use `/help` for all commands or `/list` to browse sessions."
}

/// Render a user-facing, friendly version of an internal error so the Telegram
/// user gets actionable guidance instead of a raw stack trace. Used for logging
/// and (optionally) for surfacing recovery hints back to the chat.
pub fn format_user_friendly_error(error: &anyhow::Error, context: Option<&str>) -> String {
    let error_str = error.to_string().to_lowercase();
    let mut msg = String::new();
    msg.push('\n');
    match error_str.as_str() {
        s if s.contains("unauthorized") || s.contains("authentication failed") => {
            msg.push_str("❌ *Bot Authentication Failed*\n\n");
            msg.push_str("   Your bot token is incorrect or has been revoked.\n");
            msg.push_str("   • Check [safety] telegram_bot_token in your config\n");
            msg.push_str("   • Verify the token with @BotFather on Telegram\n");
            msg.push_str("   • Ensure the bot is still in your chat\n\n");
            msg.push_str("   Use `/help` for setup instructions.");
        }
        s if s.contains("network unreachable")
            || s.contains("connection refused")
            || s.contains("timeout") =>
        {
            msg.push_str("📡 *Connection Issue*\n\n");
            msg.push_str("   Can't reach Telegram servers.\n");
            msg.push_str("   • Check your internet connection\n");
            msg.push_str("   • Try again in a few moments\n");
            msg.push_str("   • Bot may be temporarily blocked\n\n");
            msg.push_str("   Let me try alternate servers…");
        }
        s if s.contains("rate limit") || s.contains("too many requests") => {
            msg.push_str("⏱️ *Rate Limited*\n\n");
            msg.push_str("   Too many requests. Please slow down.\n");
            msg.push_str("   • Wait a moment before trying again\n");
            msg.push_str("   • Use `/status` for timing information");
        }
        s if s.contains("dns") || s.contains("resolve") => {
            msg.push_str("🌐 *Server Resolution Problem*\n\n");
            msg.push_str("   Can't find Telegram servers.\n");
            msg.push_str("   • Trying alternate server addresses\n");
            msg.push_str("   • Network may be blocking api.telegram.org");
        }
        _ => {
            msg.push_str("❌ *Something Went Wrong*\n\n");
            msg.push_str(&format!("   Error: {}\n", error));
            if let Some(ctx) = context {
                msg.push_str(&format!("   Context: {}\n", ctx));
            }
            msg.push_str("\n   • Use `/help` for assistance\n");
            msg.push_str("   • Try again later");
        }
    }
    msg
}

/// Enhanced warning system for bot auth failures with persistent warning flag.
/// Tracks whether we've already warned about auth failure to avoid spam.
#[derive(Default)]
pub struct AuthWarningTracker {
    warned: bool,
}

impl AuthWarningTracker {
    pub fn new() -> Self {
        Self { warned: false }
    }

    /// Check if we should warn about auth failure and return appropriate message.
    /// Only warns once per channel to avoid spam.
    pub fn warn_if_needed(&mut self) -> Option<String> {
        if self.warned {
            return None;
        }
        self.warned = true;
        Some(
            "🔐 *Bot Authentication Required*\n\n\
             • Your bot token has failed authentication.\n\
             • Please check [safety] telegram_bot_token in your config\n\
             • Verify the bot is properly set up with @BotFather\n\
             • Ensure the bot is still in your chat\n\n\
             🔧 Use `/help` for setup instructions and troubleshooting."
                .to_string(),
        )
    }
}


/// Tracks confirmation state for destructive operations (/free, /abort).
/// Holds at most one pending confirmation per channel at a time, with a TTL
/// so a stale prompt cannot be confirmed long after it was issued.
#[derive(Default)]
struct ConfirmationTracker {
    pending: Option<PendingConfirmation>,
}

struct PendingConfirmation {
    action: &'static str,
    session_id: String,
    expires_at: std::time::Instant,
}

const CONFIRM_TIMEOUT_SECS: u64 = 120;

impl ConfirmationTracker {
    fn new() -> Self {
        Self { pending: None }
    }

    /// Create a new confirmation and return the prompt text + callback data
    /// to be used as the inline-keyboard button. The button's callback data
    /// is `__confirm__`; the action is encoded in the tracker.
    fn request(&mut self, action: &'static str, session_id: String) -> String {
        let sid = short_id(&session_id);
        self.pending = Some(PendingConfirmation {
            action,
            session_id,
            expires_at: std::time::Instant::now()
                + std::time::Duration::from_secs(CONFIRM_TIMEOUT_SECS),
        });
        format!(
            "⚠️ *Confirm `{action}` session `{sid}`*\n\n\
             This cannot be undone.\n\
             /confirm to proceed, /cancel to abort."
        )
    }

    /// Verify a confirmation token and consume it. Returns the action and
    /// session id if the token matches a non-expired pending confirmation.
    fn verify(&mut self, token: &str) -> Option<(&'static str, String)> {
        let pending = self.pending.take()?;
        if token != "__confirm__" {
            return None;
        }
        if std::time::Instant::now() > pending.expires_at {
            return None;
        }
        Some((pending.action, pending.session_id))
    }

    /// Drop any pending confirmation (e.g. after /cancel).
    fn clear(&mut self) -> bool {
        self.pending.take().is_some()
    }
}


/// Format an agent reply for a session-reply message, escaping the reply text
/// so it cannot break Telegram's MarkdownV2 `parse_mode`. The reply
/// follows the short session id on the same line; the id itself is a short
/// hash and needs no escaping.
fn agent_reply_message(session_id: &str, reply: &str) -> String {
    format!(
        "💬 \\[{}] {}",
        short_id(session_id),
        escape_markdown_v2(reply)
    )
}

impl TelegramChannel {
    /// Run a prompt against `session_id` and stream partial assistant text into a
/// single Telegram message (so the user sees live progress), then leave the
/// final reply in place. Sends a placeholder first, then edits it as tokens
/// arrive. If the turn fails, reports the error.
async fn stream_reply_to_session(
    &self,
    reply_to: Option<i64>,
    session_id: &str,
    text: &str,
) {
    // Post one placeholder message that we will edit with streamed progress.
    // `send_message_raw` returns the created message id (or None if the text
    // needed chunking), which we then target with `edit_message_text`. The
    // placeholder is sent with `parse_mode=MarkdownV2`, so the `[` `]` around
    // the session id must be escaped (otherwise Telegram rejects the message
    // and the whole turn is skipped).
    let placeholder = format!("💭 \\[{}] _thinking…_", short_id(session_id));
    let client = self.client_or_default().await;
    let sent_id = match crate::telegram::send_message_raw(
        &client,
        &self.token,
        &self.chat_id,
        &placeholder,
        self.api_base.as_deref(),
        reply_to,
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // Placeholder needed chunking; fall back to non-streaming.
            self.fallback_reply(session_id, reply_to, text).await;
            return;
        }
        Err(e) => {
            let _ = self
                .send_reply(
                    &format!("⚠️ Could not start reply for `{}`: {}", short_id(session_id), e),
                    reply_to,
                )
                .await;
            return;
        }
    };
    use crate::server::telegram_control::resume_session_for_control_or_spawn_streaming as stream_turn;
    let token = self.token.clone();
    let api_base = self.api_base.clone();
    let chat_id = self.chat_id.clone();
    let sid = short_id(session_id).to_string();
    let on_progress = |partial: String| {
        let client = client.clone();
        let token = token.clone();
        let api_base = api_base.clone();
        let chat_id = chat_id.clone();
        let sid = sid.clone();
        async move {
            let body = format!("💬 \\[{}] {}", sid, escape_markdown_v2(&partial));
            crate::telegram::edit_message_text(
                &client,
                &token,
                chat_id.parse::<i64>().unwrap_or(0),
                sent_id,
                &body,
                api_base.as_deref(),
            )
            .await;
        }
    };
    if let Err(e) = stream_turn(session_id, text, on_progress).await {
        let _ = crate::telegram::edit_message_text(
            &client,
            &self.token,
            self.chat_id.parse::<i64>().unwrap_or(0),
            sent_id,
            &format!(
                "⚠️ Could not reach session `{}`: {}",
                short_id(session_id),
                escape_markdown_v2(&e.to_string())
            ),
            self.api_base.as_deref(),
        )
        .await;
    }
}

/// Non-streaming fallback used when we cannot obtain a message id to edit.
async fn fallback_reply(&self, session_id: &str, reply_to: Option<i64>, text: &str) {
    match crate::server::telegram_control::resume_session_for_control_or_spawn(session_id, text).await {
        Ok(reply) => {
            let _ = self.send_reply(&agent_reply_message(session_id, &reply), reply_to).await;
        }
        Err(e) => {
            let _ = self
                .send_reply(
                    &format!("⚠️ Could not reach session `{}`: {}", short_id(session_id), e),
                    reply_to,
                )
                .await;
        }
    }
}
}

const HELP_TEXT: &str = "\
🤖 *jcode Telegram session control*

*Commands:*
/list [--saved|--today] — list sessions (tap to select)
/find (text) — search sessions by title or id
/new (prompt) — start a new session (optional opening prompt)
/use (n or id) — select a session to talk to
/history (n) — show recent messages of the selected session
/peek (n) — quick 2-line preview of the active session
/resume (id) (prompt) — ask a session directly
/live — list live sessions (tap 🗑️ to free one)
/free (id) — drop a live headless session (requires /confirm)
/abort — stop the active session's running turn (requires /confirm)
/confirm — execute a pending destructive action
/cancel — cancel a pending destructive action
/clear — stop talking to the selected session
/status — show ambient & control status
/whoami — show this chat's id for config
/help — this help

*Tips:*
• Send any plain message after /use or /new to talk to a session.
• /list shows an inline picker: tap a row to select that session.
• /list --saved shows only saved sessions; /list --today shows today's activity.
• /use 2 selects the 2nd session; /use abc123… matches by id prefix.
• A ✅ marks the active session in the picker.
• /free and /abort now require /confirm for safety.";

// ---------------------------------------------------------------------------
// Discord channel

#[async_trait]
impl MessageChannel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn is_send_enabled(&self) -> bool {
        true
    }

    fn is_reply_enabled(&self) -> bool {
        self.reply_enabled
    }

    async fn send(&self, text: &str) -> anyhow::Result<()> {
        logging::debug(&format!(
            "sending telegram notification bytes={}",
            text.len()
        ));
        let client = self.client_or_default().await;
        match crate::telegram::send_message_with_base(
            &client,
            &self.token,
            &self.chat_id,
            text,
            self.api_base.as_deref(),
            None,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                if crate::telegram::is_connectivity_error(&e) {
                    logging::warn(&format!(
                        "telegram send failed (connectivity); re-discovering: {e}"
                    ));
                    self.invalidate_cache(true).await;
                    let client = self.client_or_default().await;
                    crate::telegram::send_message_with_base(
                        &client,
                        &self.token,
                        &self.chat_id,
                        text,
                        self.api_base.as_deref(),
                        None,
                    )
                    .await
                } else {
                    // Enhanced UX: surface a friendly hint for the user (e.g. via a
                    // background retry), but keep the error for the caller.
                    let friendly = format_user_friendly_error(&e, Some("sending notification"));
                    logging::warn(&format!("telegram send error (non-connectivity): {friendly}"));
                    Err(e)
                }
            }
        }
    }

    async fn reply_loop(self: Arc<Self>, runner: Option<AmbientRunnerHandle>) {
        let mut offset: Option<i64> = None;

        // Fail fast on a bad bot token with a clear message instead of the loop
        // silently re-polling an auth error forever. Discovery resolves a
        // reachable DC first, so a blocked default endpoint does not block auth.
        let client = self.client_or_default().await;
        match crate::telegram::verify_bot_auth(
            &client,
            &self.token,
            self.api_base.as_deref(),
        )
        .await
        {
            Ok(id) => logging::info(&format!(
                "telegram auth ok bot_id={} username={:?}",
                id.id, id.username
            )),
            Err(e) => {
                logging::error(&format!(
                    "telegram bot token invalid/unreachable (config issue): {e}"
                ));
                // Enhanced UX: warn the chat owner once so the misconfiguration is
                // visible where they will see it, not only in logs. The warning
                // tracker ensures the (potentially un-actionable) auth message is
                // shown at most once per process instead of on every loop start.
                if let Some(message) = self.auth_warning_tracker.lock().await.warn_if_needed() {
                    let _ = crate::telegram::send_message_with_base(
                        &client,
                        &self.token,
                        &self.chat_id,
                        &format!("⚠️ *Telegram bot failed to authenticate.*\n{}", message),
                        self.api_base.as_deref(),
                        None,
                    )
                    .await;
                }
            }
        }

        // Register the bot's slash commands with Telegram so they show up in the
        // user's `/` menu. Non-fatal on failure.
        crate::telegram::set_my_commands(&client, &self.token, self.api_base.as_deref()).await;

        loop {
            let client = self.client_or_default().await;
            match crate::telegram::get_updates_with_base(
                &client,
                &self.token,
                offset,
                30,
                self.api_base.as_deref(),
            )
            .await
            {
                Ok(updates) => {
                    if !updates.is_empty() {
                        logging::debug(&format!(
                            "telegram reply loop received update_count={}",
                            updates.len()
                        ));
                    }
                    for update in updates {
                        offset = Some(update.update_id + 1);

                        if let Some(cb) = update.callback_query {
                            let _ = self.handle_callback_query(cb).await;
                            continue;
                        }

                        let msg = match update.message {
                            Some(m) => m,
                            None => continue,
                        };
                        // `reply_to_message_id` for threading, and a clone of
                        // the message id is passed so the handler can reply to
                        // the user message that triggered it.
                        let reply_to = Some(msg.message_id);

                        // Handle each inbound message in its own task so a slow
                        // agent turn or `/resume` cannot block `getUpdates`
                        // polling, which would delay every other message in the
                        // chat and keep the confirmed offset from advancing.
                        let handler = Arc::clone(&self);
                        let runner = runner.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handler
                                .handle_inbound_message(msg, reply_to, runner.as_ref())
                                .await
                            {
                                logging::error(&format!(
                                    "telegram message handler error: {}",
                                    e
                                ));
                            }
                        });
                    }
                }
                Err(e) => {
                    logging::error(&format!("Telegram poll error: {}", e));
                    if crate::telegram::is_connectivity_error(&e) {
                        // The cached DC IP is no longer reachable; re-discover on
                        // the next iteration (throttled by the backoff window).
                        self.invalidate_cache(false).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discord channel
// ---------------------------------------------------------------------------

pub struct DiscordChannel {
    token: String,
    channel_id: String,
    reply_enabled: bool,
    bot_user_id: Option<String>,
    client: reqwest::Client,
}

impl DiscordChannel {
    pub fn new(
        token: String,
        channel_id: String,
        reply_enabled: bool,
        bot_user_id: Option<String>,
    ) -> Self {
        Self {
            token,
            channel_id,
            reply_enabled,
            bot_user_id,
            client: crate::provider::shared_http_client(),
        }
    }

    async fn poll_messages(&self, after: Option<&str>) -> anyhow::Result<Vec<DiscordMessage>> {
        logging::debug(&format!(
            "polling discord messages after_present={}",
            after.is_some()
        ));
        let mut url = format!(
            "https://discord.com/api/v10/channels/{}/messages?limit=10",
            self.channel_id
        );
        if let Some(after_id) = after {
            url.push_str(&format!("&after={}", after_id));
        }

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            logging::warn(&format!("discord message poll returned status={status}"));
            anyhow::bail!("Discord messages error ({}): {}", status, body);
        }

        let messages: Vec<DiscordMessage> = resp.json().await?;
        logging::debug(&format!(
            "discord message poll returned count={}",
            messages.len()
        ));
        Ok(messages)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DiscordMessage {
    pub id: String,
    pub content: String,
    pub author: DiscordAuthor,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DiscordAuthor {
    pub id: String,
    pub bot: Option<bool>,
}

#[async_trait]
impl MessageChannel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn is_send_enabled(&self) -> bool {
        true
    }

    fn is_reply_enabled(&self) -> bool {
        self.reply_enabled
    }

    async fn send(&self, text: &str) -> anyhow::Result<()> {
        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            self.channel_id
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&serde_json::json!({ "content": text }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord API error ({}): {}", status, body);
        }

        logging::info("Discord notification sent");
        Ok(())
    }

    async fn reply_loop(self: Arc<Self>, runner: Option<AmbientRunnerHandle>) {
        let mut last_seen_id: Option<String> = None;

        // Get the latest message ID on startup so we don't replay old messages
        match self.poll_messages(None).await {
            Ok(msgs) => {
                if let Some(latest) = msgs.first() {
                    last_seen_id = Some(latest.id.clone());
                }
            }
            Err(e) => {
                logging::error(&format!("Discord initial poll error: {}", e));
            }
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            match self.poll_messages(last_seen_id.as_deref()).await {
                Ok(msgs) => {
                    // Discord returns newest first, reverse for chronological order
                    let mut msgs = msgs;
                    msgs.reverse();

                    for msg in msgs {
                        last_seen_id = Some(msg.id.clone());

                        // Skip messages from bots (including ourselves)
                        if msg.author.bot.unwrap_or(false) {
                            continue;
                        }

                        // If we know our bot user ID, also skip our own messages
                        if let Some(ref bot_id) = self.bot_user_id
                            && msg.author.id == *bot_id
                        {
                            continue;
                        }

                        let trimmed = msg.content.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Some(req_id) = crate::notifications::extract_permission_id(trimmed) {
                            let (approved, message) =
                                crate::notifications::parse_permission_reply(trimmed);
                            if let Err(e) = crate::safety::record_permission_via_file(
                                &req_id,
                                approved,
                                "discord_reply",
                                message,
                            ) {
                                logging::error(&format!(
                                    "Failed to record permission from Discord for {}: {}",
                                    req_id, e
                                ));
                            } else {
                                logging::info(&format!(
                                    "Permission {} via Discord: {}",
                                    if approved { "approved" } else { "denied" },
                                    req_id
                                ));
                                let _ = self
                                    .send(&format!(
                                        "✅ Permission {} for `{}`",
                                        if approved { "approved" } else { "denied" },
                                        req_id
                                    ))
                                    .await;
                            }
                        } else if let Some(ref runner) = runner {
                            let injected = runner.inject_message(trimmed, "discord").await;
                            logging::info(&format!(
                                "discord reply injected into session injected={}",
                                injected
                            ));
                            let ack = if injected {
                                format!("💬 Message sent to active session: *{}*", trimmed)
                            } else {
                                format!("📋 Message queued, waking agent: *{}*", trimmed)
                            };
                            let _ = self.send(&ack).await;
                        }
                    }
                }
                Err(e) => {
                    logging::error(&format!("Discord poll error: {}", e));
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Jade cloud relay channel
// ---------------------------------------------------------------------------

/// Remote control via the Jade cloud relay (an append-only per-session event
/// log in AWS). Unlike the WebSocket gateway, nothing listens on this machine:
/// the laptop only makes outbound long-poll requests, so there is no inbound
/// port to attack. A cloud client posts `prompt` events; this channel injects
/// them into the live session and posts the agent's reply back as a `response`
/// event for the cloud client to read.
pub struct JadeRelayChannel {
    /// API base URL, normalized to end with a single '/'.
    api_base: String,
    token: String,
    token_id: Option<String>,
    user_id: String,
    session_id: String,
    reply_enabled: bool,
    client: reqwest::Client,
}

impl JadeRelayChannel {
    pub fn new(
        api_base: String,
        token: String,
        token_id: Option<String>,
        user_id: String,
        session_id: String,
        reply_enabled: bool,
    ) -> Self {
        let api_base = if api_base.ends_with('/') {
            api_base
        } else {
            format!("{}/", api_base)
        };
        Self {
            api_base,
            token,
            token_id,
            user_id,
            session_id,
            reply_enabled,
            client: crate::provider::shared_http_client(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base, path.trim_start_matches('/'))
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req.header("Authorization", format!("Bearer {}", self.token));
        if let Some(id) = &self.token_id {
            req = req.header("x-jade-token-id", id);
        }
        req
    }

    /// Register/heartbeat this device so the cloud can show it as online.
    async fn heartbeat(&self, device_id: &str) {
        let body = serde_json::json!({
            "user_id": self.user_id,
            "device_id": device_id,
            "label": device_id,
            "platform": std::env::consts::OS,
        });
        let req = self.auth(self.client.post(self.url("v1/devices")).json(&body));
        if let Err(e) = req.send().await {
            logging::debug(&format!("jade relay heartbeat failed: {}", e));
        }
    }

    /// Long-poll for new prompt events after `after`. Returns (events, next_after).
    /// `wait` is the server-side long-poll window in seconds (capped at 25 by the relay).
    async fn poll_prompts(&self, after: i64, wait: u32) -> anyhow::Result<(Vec<RelayEvent>, i64)> {
        let session = urlencoding_encode(&self.session_id);
        let url = self.url(&format!(
            "v1/sessions/{}/events?user_id={}&after={}&types=prompt&wait={}",
            session,
            urlencoding_encode(&self.user_id),
            after,
            wait
        ));
        let resp = self.auth(self.client.get(&url)).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("jade relay poll error ({}): {}", status, body);
        }
        let parsed: RelayEventsResponse = resp.json().await?;
        Ok((parsed.events, parsed.next_after))
    }

    /// Post a response event back to the relay for the cloud client to read.
    async fn post_response(&self, text: &str, request_seq: i64) -> anyhow::Result<()> {
        let session = urlencoding_encode(&self.session_id);
        let body = serde_json::json!({
            "user_id": self.user_id,
            "type": "response",
            "text": text,
            "request_seq": request_seq,
            "origin": "jcode",
        });
        let resp = self
            .auth(
                self.client
                    .post(self.url(&format!("v1/sessions/{}/events", session)))
                    .json(&body),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("jade relay post error ({}): {}", status, detail);
        }
        Ok(())
    }
}

#[derive(Debug, serde::Deserialize)]
struct RelayEventsResponse {
    #[serde(default)]
    events: Vec<RelayEvent>,
    #[serde(default)]
    next_after: i64,
}

#[derive(Debug, serde::Deserialize)]
struct RelayEvent {
    #[serde(default)]
    seq: i64,
    #[serde(default)]
    text: Option<String>,
}

/// Minimal percent-encoding for path/query segments (alnum and -_.~ pass through).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[async_trait]
impl MessageChannel for JadeRelayChannel {
    fn name(&self) -> &str {
        "jade_relay"
    }

    fn is_send_enabled(&self) -> bool {
        true
    }

    fn is_reply_enabled(&self) -> bool {
        // Inbound Jade relay prompts are delivered by server::jade_relay so they
        // work even when ambient mode is disabled and target the configured live
        // Jcode session directly. Keep this channel for outbound notifications
        // only; otherwise ambient mode would start a second poller.
        let _configured_for_server_listener = self.reply_enabled;
        false
    }

    async fn send(&self, text: &str) -> anyhow::Result<()> {
        // Cloud notifications (e.g. ambient cycle summaries) are posted as a
        // response event with request_seq=0 (not tied to a specific prompt).
        self.post_response(text, 0).await
    }

    async fn reply_loop(self: Arc<Self>, runner: Option<AmbientRunnerHandle>) {
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "laptop".to_string());
        let device_id = format!("jcode-{}", host);
        logging::info(&format!(
            "jade relay reply loop started channel={}/{}",
            self.user_id, self.session_id
        ));
        // Start after the latest existing prompt so we don't replay history.
        let mut after: i64 = match self.poll_prompts(0, 0).await {
            Ok((_, next)) => next,
            Err(e) => {
                logging::error(&format!("jade relay init poll failed: {}", e));
                0
            }
        };
        let mut last_heartbeat = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now);

        loop {
            if last_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                self.heartbeat(&device_id).await;
                last_heartbeat = std::time::Instant::now();
            }
            match self.poll_prompts(after, 20).await {
                Ok((events, next_after)) => {
                    after = next_after;
                    for ev in events {
                        let text = ev.text.unwrap_or_default();
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Some(req_id) = crate::notifications::extract_permission_id(trimmed) {
                            let (approved, message) =
                                crate::notifications::parse_permission_reply(trimmed);
                            if let Err(e) = crate::safety::record_permission_via_file(
                                &req_id,
                                approved,
                                "jade_relay",
                                message,
                            ) {
                                logging::error(&format!(
                                    "Failed to record permission from jade relay for {}: {}",
                                    req_id, e
                                ));
                            } else {
                                let _ = self
                                    .post_response(
                                        &format!(
                                            "Permission {} for {}",
                                            if approved { "approved" } else { "denied" },
                                            req_id
                                        ),
                                        ev.seq,
                                    )
                                    .await;
                            }
                            continue;
                        }
                        let injected = if let Some(ref runner) = runner {
                            runner.inject_message(trimmed, "jade_relay").await
                        } else {
                            false
                        };
                        logging::info(&format!(
                            "jade relay prompt injected seq={} injected={}",
                            ev.seq, injected
                        ));
                        let ack = if injected {
                            "Message delivered to active session."
                        } else {
                            "Message queued; waking agent."
                        };
                        if let Err(e) = self.post_response(ack, ev.seq).await {
                            logging::error(&format!("jade relay ack post failed: {}", e));
                        }
                    }
                }
                Err(e) => {
                    logging::error(&format!("jade relay poll error: {}", e));
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_user_friendly_error_classifies() {
        // Auth errors get a clear, actionable message.
        let auth = format_user_friendly_error(
            &anyhow::anyhow!("Telegram auth failed: Unauthorized"),
            None,
        );
        assert!(auth.contains("Bot Authentication Failed"));
        assert!(auth.contains("/help"));

        // Network errors point at connectivity + alternate servers.
        let net = format_user_friendly_error(
            &anyhow::anyhow!("error trying to connect: connection refused"),
            Some("sending notification"),
        );
        assert!(net.contains("Connection Issue"));
        assert!(net.contains("alternate servers"));

        // Rate limits tell the user to slow down.
        let rl = format_user_friendly_error(
            &anyhow::anyhow!("Too Many Requests (429)"),
            None,
        );
        assert!(rl.contains("Rate Limited"));
    }

    #[test]
    fn test_format_user_friendly_error_fallback_includes_raw_error() {
        let other = format_user_friendly_error(
            &anyhow::anyhow!("some unexpected internal error"),
            Some("bot startup auth"),
        );
        assert!(other.contains("Something Went Wrong"));
        assert!(other.contains("some unexpected internal error"));
        assert!(other.contains("bot startup auth"));
    }

    #[test]
    fn test_auth_warning_tracker_warns_once() {
        let mut tracker = AuthWarningTracker::new();
        // First call returns the friendly warning message.
        let first = tracker.warn_if_needed();
        assert!(first.is_some());
        assert!(first.unwrap().contains("Bot Authentication Required"));
        // Subsequent calls within the same process return None (no spam).
        assert!(tracker.warn_if_needed().is_none());
        assert!(tracker.warn_if_needed().is_none());
        // A fresh tracker warns again (per-process tracking, not global).
        let mut fresh = AuthWarningTracker::new();
        assert!(fresh.warn_if_needed().is_some());
    }

    /// A tiny in-memory Telegram Bot API endpoint for exercising the picker
    /// menu flow (`send_session_picker` + `handle_callback_query`) without a
    /// live network. It records the requests it receives so the test can assert
    /// on the exact JSON payloads the channel emits (keyboard, answer, and
    /// keyboard-collapse).
    struct MockTelegram {
        addr: std::net::SocketAddr,
        requests: std::sync::Arc<tokio::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    }

    impl MockTelegram {
        async fn start() -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock");
            let addr = listener.local_addr().expect("local addr");
            let requests = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let reqs = requests.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let reqs = reqs.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(&mut sock);
                        let mut line = String::new();
                        let n = reader.read_line(&mut line).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        // Request line: POST /bot<token>/<method> HTTP/1.1
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        let method_name = parts
                            .get(1)
                            .and_then(|p| p.rsplit('/').next())
                            .map(str::to_string)
                            .unwrap_or_default();
                        // Read headers until blank line.
                        let mut content_length = 0usize;
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).await.unwrap_or(0) == 0 {
                                break;
                            }
                            if h.trim().is_empty() {
                                break;
                            }
                            if let Some(rest) = h.to_lowercase().strip_prefix("content-length:") {
                                content_length = rest.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0u8; content_length];
                        reader.read_exact(&mut body).await.unwrap_or(0);
                        let json: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                        reqs.lock().await.push((method_name.clone(), json.clone()));
                        // Answer each Bot API call with ok:true and a minimal
                        // result so the client considers the call successful.
                        let response = match method_name.as_str() {
                            "sendMessage" | "answerCallbackQuery" | "editMessageReplyMarkup" => {
                                serde_json::json!({
                                    "ok": true,
                                    "result": {"message_id": 1}
                                })
                            }
                            _ => serde_json::json!({ "ok": true, "result": true }),
                        };
                        let payload = serde_json::to_string(&response).unwrap();
                        let _ = sock
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    payload.len(),
                                    payload
                                )
                                .as_bytes(),
                            )
                            .await;
                    });
                }
            });
            MockTelegram { addr, requests }
        }

        fn base(&self) -> String {
            format!("http://{}/", self.addr)
        }

        async fn methods(&self) -> Vec<String> {
            self.requests.lock().await.iter().map(|(m, _)| m.clone()).collect()
        }

        async fn bodies(&self) -> Vec<serde_json::Value> {
            self.requests.lock().await.iter().map(|(_, b)| b.clone()).collect()
        }
    }

    // The test-env lock is held across the `.await` points on purpose (it
    // isolates JCODE_HOME and the recent-session index for the whole test), so
    // clippy's await_holding_lock finding is intentional here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn test_session_picker_menu_flow() {
        // Isolate the recent-session index (and the per-chat active-session
        // state) under a scratch JCODE_HOME so we can seed a resumable session
        // and have the `/list` menu actually render a real button.
        let _guard = crate::storage::lock_test_env();
        let home = tempfile::TempDir::new().expect("temp home");
        crate::env::set_var("JCODE_HOME", home.path());

        crate::recent_session_index::upsert(&crate::recent_session_index::RecentSessionMetadata {
            session_id: "session_fox_1_aabbccddeeff0011".into(),
            working_dir: None,
            // No title at all: the menu label must fall back to the memorable
            // session name ("fox") instead of rendering "<untitled>".
            generated_title: None,
            custom_title: None,
            todo_title: None,
            saved: false,
            updated_at_ms: 1,
            last_active_at_ms: Some(2),
        })
        .expect("seed session");
        // Note: the test env lock is held for the whole test (matching the
        // established agent_tests pattern) so the global JCODE_HOME / recent
        // index are isolated from parallel tests.

        let mock = MockTelegram::start().await;

        let ch = TelegramChannel::with_connectivity(
            "tok".into(),
            "77".into(),
            true,
            Some(mock.base()),
            None,
            None,
            None,
        );

        // /list drives the picker menu.
        let reply = ch.handle_command("/list", None).await;
        // The picker sends the keyboard message itself; there is no text reply.
        assert_eq!(reply, "");

        let bodies = mock.bodies().await;
        let picker = bodies
            .iter()
            .find(|b| b.get("text").is_some() && b.get("reply_markup").is_some())
            .expect("a picker message with an inline keyboard");
        let keyboard = picker["reply_markup"]["inline_keyboard"]
            .as_array()
            .expect("inline_keyboard array");
        assert_eq!(keyboard.len(), 1, "one seeded session -> one menu button row");
        let row = keyboard[0].as_array().expect("button row");
        assert_eq!(row.len(), 1, "one button per row");
        assert_eq!(
            row[0]["text"], "fox (session_)",
            "menu button must fall back to the memorable session name, not <untitled>"
        );
        assert_eq!(
            row[0]["callback_data"], "session_fox_1_aabbccddeeff0011",
            "button data must carry the selectable session id"
        );

        // Tap the menu button: a callback_query carrying the session id.
        ch.handle_callback_query(crate::telegram::CallbackQuery {
            id: "cb1".into(),
            from: Some(crate::telegram::TelegramFrom { id: 1 }),
            data: Some("session_fox_1_aabbccddeeff0011".into()),
            message: Some(crate::telegram::CallbackMessage {
                chat: Some(crate::telegram::Chat { id: 77 }),
                message_id: Some(42),
            }),
        })
        .await;

        // The tap must answer the callback (clear the button's loading spinner)
        // and collapse the picker keyboard so the menu does not linger.
        let methods = mock.methods().await;
        assert!(
            methods.iter().any(|m| m == "answerCallbackQuery"),
            "callback must be answered to stop the button spinner, got {methods:?}"
        );
        assert!(
            methods.iter().any(|m| m == "editMessageReplyMarkup"),
            "picker keyboard should be collapsed after selection, got {methods:?}"
        );
        // The acknowledged session becomes the active session for this chat.
        assert_eq!(
            crate::server::telegram_control::active_session_for("77").as_deref(),
            Some("session_fox_1_aabbccddeeff0011")
        );

        // A callback with no `data` (e.g. a stray tap on a disabled button)
        // must still be acknowledged, and the acknowledgement must NOT carry an
        // empty `text` field: Telegram rejects empty text and would leave the
        // button stuck in its loading state.
        let before = mock.methods().await.len();
        ch.handle_callback_query(crate::telegram::CallbackQuery {
            id: "cb_empty".into(),
            from: Some(crate::telegram::TelegramFrom { id: 1 }),
            data: None,
            message: Some(crate::telegram::CallbackMessage {
                chat: Some(crate::telegram::Chat { id: 77 }),
                message_id: Some(43),
            }),
        })
        .await;
        let (new_methods, bodies) = (mock.methods().await, mock.bodies().await);
        assert!(
            new_methods.len() > before,
            "a data-less callback must still be answered"
        );
        let answer_body = bodies
            .iter()
            .zip(new_methods.iter())
            .find(|(_, m)| m.as_str() == "answerCallbackQuery")
            .map(|(b, _)| b)
            .expect("latest answerCallbackQuery body");
        assert!(
            answer_body.get("text").is_none() || !answer_body["text"].as_str().unwrap_or("").is_empty(),
            "answerCallbackQuery must not send an empty `text` (would leave the button stuck)"
        );
    }

    #[test]
    fn test_split_command_basic() {
        let (cmd, rest) = split_command("/list");
        assert_eq!(cmd, "/list");
        assert_eq!(rest, "");
    }

    #[test]
    fn test_split_command_with_args() {
        let (cmd, rest) = split_command("/list  10");
        assert_eq!(cmd, "/list");
        assert_eq!(rest, "10");
    }

    #[test]
    fn test_split_command_strips_bot_mention() {
        let (cmd, _) = split_command("/status@vasily_pelikh_openclaw_bot");
        assert_eq!(cmd, "/status");
    }

    #[test]
    fn test_is_allowed_sender_unrestricted() {
        let ch = TelegramChannel::new("t".into(), "c".into(), true);
        assert!(ch.is_allowed_sender(None));
        assert!(ch.is_allowed_sender(Some(&crate::telegram::TelegramFrom { id: 999 })));
    }

    #[test]
    fn test_is_allowed_sender_whitelist() {
        let ch = TelegramChannel::with_connectivity(
            "t".into(),
            "c".into(),
            true,
            None,
            None,
            None,
            Some("42".into()),
        );
        assert!(!ch.is_allowed_sender(None));
        assert!(!ch.is_allowed_sender(Some(&crate::telegram::TelegramFrom { id: 7 })));
        assert!(ch.is_allowed_sender(Some(&crate::telegram::TelegramFrom { id: 42 })));
    }

    #[test]
    fn test_discord_message_parse() {
        let json = r#"{
            "id": "123456",
            "content": "hello agent",
            "author": {"id": "789", "bot": false}
        }"#;
        let msg: DiscordMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id, "123456");
        assert_eq!(msg.content, "hello agent");
        assert!(!msg.author.bot.unwrap());
    }

    #[test]
    fn test_discord_bot_message_parse() {
        let json = r#"{
            "id": "999",
            "content": "bot response",
            "author": {"id": "111", "bot": true}
        }"#;
        let msg: DiscordMessage = serde_json::from_str(json).unwrap();
        assert!(msg.author.bot.unwrap());
    }

    #[test]
    fn test_relay_events_parse() {
        let json = r#"{
            "events": [
                {"seq": 5, "type": "prompt", "text": "run the tests"},
                {"seq": 6, "type": "prompt", "text": "now lint"}
            ],
            "next_after": 6
        }"#;
        let parsed: RelayEventsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].seq, 5);
        assert_eq!(parsed.events[0].text.as_deref(), Some("run the tests"));
        assert_eq!(parsed.next_after, 6);
    }

    #[test]
    fn test_relay_events_empty() {
        let json = r#"{"events": [], "next_after": 0}"#;
        let parsed: RelayEventsResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.events.is_empty());
        assert_eq!(parsed.next_after, 0);
    }

    #[test]
    fn test_relay_url_encoding() {
        assert_eq!(urlencoding_encode("sess-relay-test"), "sess-relay-test");
        assert_eq!(urlencoding_encode("a/b c"), "a%2Fb%20c");
        assert_eq!(urlencoding_encode("user.name~1_2"), "user.name~1_2");
    }

    #[test]
    fn test_relay_url_join() {
        let ch = JadeRelayChannel::new(
            "https://example.com/api".to_string(),
            "tok".to_string(),
            Some("jeremy".to_string()),
            "jeremy".to_string(),
            "sess-1".to_string(),
            true,
        );
        assert_eq!(ch.url("v1/devices"), "https://example.com/api/v1/devices");
        assert_eq!(ch.url("/v1/devices"), "https://example.com/api/v1/devices");
    }

    #[test]
    fn test_relay_registry_wiring() {
        // Disabled: not registered.
        let cfg = SafetyConfig::default();
        let reg = ChannelRegistry::from_config(&cfg);
        assert!(!reg.channel_names().iter().any(|n| n == "jade_relay"));

        // Enabled but incomplete: skipped with a warning.
        let mut cfg = SafetyConfig {
            jade_relay_enabled: true,
            ..SafetyConfig::default()
        };
        let reg = ChannelRegistry::from_config(&cfg);
        assert!(!reg.channel_names().iter().any(|n| n == "jade_relay"));

        // Enabled and complete: registered.
        cfg.jade_relay_api_base = Some("https://example.com/".to_string());
        cfg.jade_relay_token = Some("tok".to_string());
        cfg.jade_relay_session_id = Some("sess-1".to_string());
        let reg = ChannelRegistry::from_config(&cfg);
        assert!(reg.channel_names().iter().any(|n| n == "jade_relay"));
    }

    /// Live end-to-end test against the real Jade relay. Ignored by default;
    /// run with the relay env vars set:
    ///   JADE_RELAY_API_BASE, JADE_RELAY_TOKEN, JADE_RELAY_TOKEN_ID,
    ///   JADE_RELAY_USER_ID, JADE_RELAY_SESSION_ID
    ///   cargo test -p jcode-app-core relay_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires live Jade relay credentials"]
    async fn test_relay_live_roundtrip() {
        let api_base = match std::env::var("JADE_RELAY_API_BASE") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skipping: JADE_RELAY_API_BASE not set");
                return;
            }
        };
        let token = std::env::var("JADE_RELAY_TOKEN").expect("JADE_RELAY_TOKEN");
        let token_id = std::env::var("JADE_RELAY_TOKEN_ID").ok();
        let user_id = std::env::var("JADE_RELAY_USER_ID").unwrap_or_else(|_| "jeremy".to_string());
        let session_id = std::env::var("JADE_RELAY_SESSION_ID")
            .unwrap_or_else(|_| format!("rust-live-{}", chrono::Utc::now().timestamp()));

        let ch = JadeRelayChannel::new(
            api_base,
            token,
            token_id.clone(),
            user_id.clone(),
            session_id.clone(),
            true,
        );

        // 1) heartbeat (device register)
        ch.heartbeat("jcode-test-device").await;

        // 2) baseline cursor: no prompts yet
        let (events, after) = ch.poll_prompts(0, 0).await.expect("baseline poll");
        eprintln!("baseline: {} events, next_after={}", events.len(), after);

        // 3) simulate a cloud client posting a prompt by POSTing a prompt event
        let prompt_text = format!(
            "hello from rust live test {}",
            chrono::Utc::now().timestamp()
        );
        let prompt_body = serde_json::json!({
            "user_id": user_id,
            "type": "prompt",
            "text": prompt_text,
            "origin": "rust-test-client",
        });
        let resp = ch
            .auth(
                ch.client
                    .post(ch.url(&format!(
                        "v1/sessions/{}/events",
                        urlencoding_encode(&session_id)
                    )))
                    .json(&prompt_body),
            )
            .send()
            .await
            .expect("post prompt");
        assert!(
            resp.status().is_success(),
            "post prompt status {}",
            resp.status()
        );

        // 4) the channel polls and sees the prompt
        let (events, after2) = ch.poll_prompts(after, 5).await.expect("poll after prompt");
        assert!(!events.is_empty(), "expected at least one prompt event");
        let prompt_ev = events
            .iter()
            .find(|e| e.text.as_deref() == Some(prompt_text.as_str()))
            .expect("our prompt event present");
        eprintln!("received prompt seq={} after2={}", prompt_ev.seq, after2);

        // 5) the channel posts a response tied to that prompt's seq
        let reply = format!("rust live reply to seq {}", prompt_ev.seq);
        ch.post_response(&reply, prompt_ev.seq)
            .await
            .expect("post response");

        // 6) verify the response is visible (poll all event types via raw GET)
        let verify_url = ch.url(&format!(
            "v1/sessions/{}/events?user_id={}&after=0&types=response&wait=5",
            urlencoding_encode(&session_id),
            urlencoding_encode(&user_id)
        ));
        let verify: RelayEventsResponse = ch
            .auth(ch.client.get(&verify_url))
            .send()
            .await
            .expect("verify get")
            .json()
            .await
            .expect("verify json");
        assert!(
            verify
                .events
                .iter()
                .any(|e| e.text.as_deref() == Some(reply.as_str())),
            "response event should be readable back from the relay"
        );
        eprintln!("LIVE ROUNDTRIP OK: prompt -> poll -> response verified");
    }

    #[test]
    fn test_confirmation_tracker_basic() {
        let mut tracker = ConfirmationTracker::new();
        let prompt = tracker.request("abort", "session_abc123".to_string());
        // The prompt shows the 8-char short id, so assert on the rendered value.
        assert!(prompt.contains(&format!("Confirm `abort` session `{}`", short_id("session_abc123"))));
        // Verify matches
        let Some((action, id)) = tracker.verify("__confirm__") else {
            panic!("Expected verification to succeed");
        };
        assert_eq!(action, "abort");
        assert_eq!(id, "session_abc123");
        // After consume, should be None
        assert!(tracker.verify("__confirm__").is_none());
        // Clear works
        tracker.request("free", "session_xyz".to_string());
        assert!(tracker.clear());
        assert!(!tracker.clear());
    }

    #[test]
    fn test_confirmation_tracker_ttl() {
        use std::time::{Duration, Instant};
        let mut tracker = ConfirmationTracker::new();
        tracker.request("abort", "session_test".to_string());
        // Manually expire
        tracker.pending.as_mut().unwrap().expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(tracker.verify("__confirm__").is_none());
    }

    #[test]
    fn test_help_footer() {
        let footer = help_footer();
        assert!(footer.contains("/help"));
        assert!(footer.contains("/list"));
    }

    #[test]
    fn test_peek_preview_lines_strips_prefix_and_escapes() {
        // A block as render_session_history emits it.
        let entries = "🧑 *you:* hello _world_ & code `x`\n\n🤖 *jcode:* reply ok `z`";
        let lines = peek_preview_lines(entries);
        // One line per message paragraph, no doubled role label.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello \\_world\\_ & code \\`x\\`");
        assert_eq!(lines[1], "reply ok \\`z\\`");
        // Body reserved chars are escaped; role label asterisks are gone.
        assert!(!lines[0].contains("*you:*"));
        assert!(lines[0].contains("\\_world\\_"));
    }

    #[test]
    fn test_peek_preview_lines_single_and_empty() {
        assert_eq!(
            peek_preview_lines("🧑 *you:* only one"),
            vec!["only one".to_string()]
        );
        assert!(peek_preview_lines("").is_empty());
        assert!(peek_preview_lines("   \n\n  ").is_empty());
    }

    #[test]
    fn test_escape_history_entries_preserves_labels_and_escapes_bodies() {
        // The `*role*` markers stay bold; only the dynamic body is escaped.
        let entries = "🧑 *you:* hello _world_\n\n🤖 *jcode:* reply `x` & [y]";
        let out = escape_history_entries(entries);
        let expected =
            "🧑 *you:* hello \\_world\\_\n\n🤖 *jcode:* reply \\`x\\` & \\[y\\]";
        assert_eq!(out, expected);
        // A body with no role prefix is escaped wholesale.
        let plain = escape_history_entries("solo _line_");
        assert_eq!(plain, "solo \\_line\\_");
    }
}
