use crate::ambient_runner::AmbientRunnerHandle;
use crate::config::SafetyConfig;
use crate::logging;
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
    async fn reply_loop(&self, runner: Option<AmbientRunnerHandle>);
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
    client: reqwest::Client,
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
            client,
        }
    }

    /// Handle a slash command received over Telegram, returning the reply text.
    /// Read-only commands: `/help`, `/status`, `/list`. Write commands
    /// (create/resume/abort) are reserved for a later phase.
    async fn handle_command(
        &self,
        trimmed: &str,
        runner: Option<&AmbientRunnerHandle>,
    ) -> String {
        let (cmd, rest) = split_command(trimmed);
        match cmd.as_str() {
            "/help" | "/start" | "help" | "start" => HELP_TEXT.to_string(),
            "/list" | "/sessions" => {
                self.send_session_picker().await;
                String::new()
            }
            "/status" => self.status_reply(runner).await,
            "/use" => self.use_session_reply(&rest).await,
            "/history" => self.history_reply(&rest),
            "/clear" | "/stop" => {
                let cleared =
                    crate::server::telegram_control::active_session_for(&self.chat_id).is_some();
                crate::server::telegram_control::clear_active_session(&self.chat_id);
                if cleared {
                    "✓ Cleared the active session. Use `/use <id>` to select another.".to_string()
                } else {
                    "No active session to clear.".to_string()
                }
            }
            "/resume" => {
                let prompt = rest.trim();
                self.resume_reply(prompt).await
            }
            _ => format!(
                "Unknown command `{}`. Use `/help` for available commands.",
                cmd
            ),
        }
    }

    /// Send an inline-keyboard session picker to the chat. Each button's
    /// `callback_data` is the session id, so tapping it selects that session.
    async fn send_session_picker(&self) {
        let entries = crate::recent_session_index::recent(12);
        let sessions = match entries {
            Ok(list) => list,
            Err(e) => {
                logging::warn(&format!("telegram session picker index error: {e}"));
                return;
            }
        };
        if sessions.is_empty() {
            let _ = self.send("No sessions found yet.");
            return;
        }
        use crate::telegram::{InlineKeyboardButton, InlineKeyboardRow};
        let active = crate::server::telegram_control::active_session_for(&self.chat_id);
        let mut rows: Vec<InlineKeyboardRow> = Vec::new();
        for s in sessions.iter() {
            let title = s
                .display_title()
                .unwrap_or("<untitled>")
                .chars()
                .take(30)
                .collect::<String>();
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
        let _ = crate::telegram::send_message_with_keyboard(
            &self.client,
            &self.token,
            &self.chat_id,
            "📚 Select a session:",
            &rows,
            self.api_base.as_deref(),
        )
        .await;
    }

    /// Handle an inline-keyboard tap (`callback_query`). `callback_data` is a
    /// session id; selecting it sets the active session for this chat.
    async fn handle_callback_query(&self, cb: crate::telegram::CallbackQuery) {
        let Some(chat_id) = cb
            .message
            .as_ref()
            .and_then(|m| m.chat.as_ref())
            .map(|c| c.id.to_string())
        else {
            return;
        };
        if chat_id != self.chat_id {
            return;
        }
        if !self.is_allowed_sender(cb.from.as_ref()) {
            logging::warn("ignoring callback_query from disallowed sender");
            let _ = crate::telegram::answer_callback_query(
                &self.client,
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
                &self.client,
                &self.token,
                &cb.id,
                "",
                self.api_base.as_deref(),
            )
            .await;
            return;
        };

        let session_id = data.trim().to_string();
        crate::server::telegram_control::set_active_session(&self.chat_id, &session_id);
        let ack = format!("Selected session `{}`", short_id(&session_id));
        crate::logging::info(&format!("telegram callback selected session={session_id}"));
        let _ = crate::telegram::answer_callback_query(
            &self.client,
            &self.token,
            &cb.id,
            &ack,
            self.api_base.as_deref(),
        )
        .await;
        let _ = self
            .send(&format!(
                "✅ Selected `{}`. Send a message to talk to it, or `/history` to view.",
                short_id(&session_id)
            ))
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

    /// `/history [n]`: show recent messages of the active session.
    fn history_reply(&self, arg: &str) -> String {
        let Some(session_id) = crate::server::telegram_control::active_session_for(&self.chat_id)
        else {
            return "No active session. Use `/use <n>` after `/list`.".to_string();
        };
        let limit = arg
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=50).contains(n))
            .unwrap_or(10);
        match crate::server::telegram_control::render_session_history(&session_id, limit) {
            Ok(text) if text != "(no visible messages)" => format!("📜 [{}]\n{}", short_id(&session_id), text),
            Ok(text) => format!("[{}] {}", short_id(&session_id), text),
            Err(e) => format!(
                "⚠️ Could not read history for `{}`: {}",
                short_id(&session_id),
                e
            ),
        }
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
        match crate::server::telegram_control::resume_session_for_control_or_spawn(
            &session_id,
            prompt,
        )
        .await
        {
            Ok(reply) => format!("💬 [{}] {}", short_id(&session_id), reply),
            Err(e) => format!("⚠️ Could not resume `{}`: {}", short_id(&session_id), e),
        }
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
                reference
            )),
            1 => Ok(matches[0].clone()),
            _ => Err(format!(
                "`{}` matches {} live sessions; use a longer prefix.",
                reference,
                matches.len()
            )),
        }
    }

    /// `/status`: report ambient mode availability and remote-control readiness.
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
        format!(
            "🤖 jcode session control\n*Ambient mode:* {}\n{}\n*Commands:* /list /use /history /clear /help",
            ambient, active_line
        )
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

const HELP_TEXT: &str = "\
🤖 *jcode Telegram session control*

Commands:
/list — list sessions
/use <n or id> — select a session to talk to
/history [n] — show recent messages of the selected session
/clear — stop talking to the selected session
/status — show ambient & control status
/help — this help

After `/use`, send any plain message to talk to that session.";

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
        crate::telegram::send_message_with_base(
            &self.client,
            &self.token,
            &self.chat_id,
            text,
            self.api_base.as_deref(),
        )
        .await
    }

    async fn reply_loop(&self, runner: Option<AmbientRunnerHandle>) {
        let mut offset: Option<i64> = None;

        loop {
            match crate::telegram::get_updates_with_base(
                &self.client,
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

                        if msg.chat.id.to_string() != self.chat_id {
                            continue;
                        }

                        if !self.is_allowed_sender(msg.from.as_ref()) {
                            logging::warn(&format!(
                                "ignoring telegram message from disallowed sender from={:?}",
                                msg.from.as_ref().map(|f| f.id)
                            ));
                            continue;
                        }

                        let text = match msg.text {
                            Some(t) => t,
                            None => continue,
                        };

                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Some(req_id) = crate::notifications::extract_permission_id(trimmed)
                        {
                            let (approved, message) =
                                crate::notifications::parse_permission_reply(trimmed);
                            if let Err(e) = crate::safety::record_permission_via_file(
                                &req_id,
                                approved,
                                "telegram_reply",
                                message,
                            ) {
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
                                    .send(&format!(
                                        "✅ Permission {} for `{}`",
                                        if approved { "approved" } else { "denied" },
                                        req_id
                                    ))
                                    .await;
                            }
                        } else if trimmed.starts_with('/') {
                            let reply = self.handle_command(trimmed, runner.as_ref()).await;
                            if !reply.is_empty() {
                                let _ = self.send(&reply).await;
                            }
                        } else if let Some(active_id) =
                            crate::server::telegram_control::active_session_for(&self.chat_id)
                        {
                            match crate::server::telegram_control::resume_session_for_control_or_spawn(
                                &active_id,
                                trimmed,
                            )
                            .await
                            {
                                Ok(reply) => {
                                    let _ = self
                                        .send(&format!("💬 [{}] {}", short_id(&active_id), reply))
                                        .await;
                                }
                                Err(e) => {
                                    let _ = self
                                        .send(&format!(
                                            "⚠️ Could not reach session `{}`: {}",
                                            short_id(&active_id),
                                            e
                                        ))
                                        .await;
                                }
                            }
                        } else if let Some(ref runner) = runner {
                            let injected = runner.inject_message(trimmed, "telegram").await;
                            logging::info(&format!(
                                "telegram reply injected into session injected={}",
                                injected
                            ));
                            let ack = if injected {
                                format!("💬 Message sent to active session: _{}_", trimmed)
                            } else {
                                format!("📋 Message queued, waking agent: _{}_", trimmed)
                            };
                            let _ = self.send(&ack).await;
                        } else {
                            let _ = self
                                .send(&format!(
                                    "ℹ️ Select a session first: use `/list` then `/use <n>`, or run `/help`."
                                ))
                                .await;
                        }
                    }
                }
                Err(e) => {
                    logging::error(&format!("Telegram poll error: {}", e));
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

    async fn reply_loop(&self, runner: Option<AmbientRunnerHandle>) {
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

    async fn reply_loop(&self, runner: Option<AmbientRunnerHandle>) {
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
}
