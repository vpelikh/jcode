use super::*;
use jcode_provider_openrouter::stream::OpenRouterStream;

fn local_endpoint_troubleshooting_hint(api_base: &str, model: &str) -> &'static str {
    let lower = api_base.to_ascii_lowercase();
    if lower.contains("localhost:11434") || lower.contains("127.0.0.1:11434") {
        return "Ollama hint: make sure `ollama serve` is running, the model is installed with `ollama pull <model>`, and run jcode with an installed model, for example `jcode --provider ollama --model llama3.2 run 'hello'`. If replies ignore earlier turns, Ollama is truncating the prompt to its serving context: restart it with a larger window, e.g. `OLLAMA_CONTEXT_LENGTH=65536 ollama serve`.";
    }

    if lower.contains("localhost:1234") || lower.contains("127.0.0.1:1234") {
        return "LM Studio hint: start the Local Server in LM Studio, load a chat model, and run jcode with the exact model id shown by LM Studio's /v1/models endpoint.";
    }

    if lower.contains("localhost") || lower.contains("127.0.0.1") || lower.contains("[::1]") {
        return "Local endpoint hint: make sure the server is running, the base URL includes /v1, the selected model is loaded, and the server supports streaming POST /chat/completions.";
    }

    let _ = model;
    "Hint: check network connectivity, DNS/TLS, that the base URL includes the API version (usually /v1), and that the model exists on the provider."
}

/// A troubleshooting hint for a provider that rejected the request because its
/// serialized body was too large (HTTP 413). The generic network hint is
/// misleading here: a 413 payload rejection is a *request-body size* failure,
/// not a connectivity fault, so we point the user at shrinking the transcript
/// (dropping images or large tool outputs) instead.
fn payload_too_large_hint() -> &'static str {
    "Hint: the provider rejected the request because the serialized body exceeded \
its size limit (not a network error). Reduce the payload: keep recent turns, drop \
or re-run image-heavy tools, shorten large tool outputs, or start a fresh \
conversation with /new and /compact."
}

// ============================================================================
// SSE Stream Parser
// ============================================================================

#[expect(
    clippy::too_many_arguments,
    reason = "stream helpers thread transport, auth, request, event channel, and pin state explicitly"
)]
pub(super) async fn run_stream_with_retries(
    client: Client,
    api_base: String,
    auth: ProviderAuth,
    send_openrouter_headers: bool,
    request: Value,
    tx: mpsc::Sender<Result<StreamEvent>>,
    provider_pin: Arc<Mutex<Option<ProviderPin>>>,
    model: String,
) {
    let mut last_error = None;
    let mut next_retry_delay = None;
    let config = jcode_base::config::config();
    let max_retries = config.provider.max_retries.max(1);
    let retry_backoff_cap =
        std::time::Duration::from_secs(config.provider.retry_backoff_cap_secs.max(1));

    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay =
                retry_delay_after_failure(attempt, next_retry_delay.take(), retry_backoff_cap);
            tokio::time::sleep(delay).await;
            jcode_base::logging::info(&format!(
                "Retrying API request using {} (attempt {}/{})",
                auth.label(),
                attempt + 1,
                max_retries
            ));
        }

        jcode_base::logging::info(&format!(
            "API stream attempt {}/{} over HTTPS transport (model: {}, endpoint: {}, auth: {})",
            attempt + 1,
            max_retries,
            model,
            api_base,
            auth.label()
        ));

        // Track whether this attempt streams replay-visible output so a
        // mid-stream transport fault can roll the partial output back on the
        // consumer before the retry replays the response from the top.
        let (attempt_tx, attempt_guard) =
            jcode_provider_core::attempt_tracker::track_attempt_output(tx.clone());

        // Retries use a fresh unpooled client: the fault that broke attempt N
        // (e.g. TLS BadRecordMac from a corrupting middlebox) may also have
        // poisoned other idle pooled connections opened through the same path,
        // so reusing the shared pool can fail identically. A fresh client
        // guarantees a brand-new TCP+TLS connection.
        let attempt_client = if attempt == 0 {
            client.clone()
        } else {
            jcode_provider_core::fresh_transport_client()
        };

        match stream_response(
            attempt_client,
            api_base.clone(),
            auth.clone(),
            send_openrouter_headers,
            request.clone(),
            attempt_tx,
            Arc::clone(&provider_pin),
            model.clone(),
        )
        .await
        {
            Ok(()) => {
                let _ = attempt_guard.finish().await;
                return;
            }
            Err(e) => {
                let saw_output = attempt_guard.finish().await;
                // Full anyhow chain ({:#}) so a `.context(...)`-wrapped transport
                // cause (e.g. TLS BadRecordMac) is visible to the classifier.
                let error_str = format!("{e:#}").to_lowercase();
                if is_retryable_error(&error_str) && attempt + 1 < max_retries {
                    if saw_output {
                        // Partial output already reached the consumer; tell it
                        // to discard the partial attempt so the retried
                        // response replays cleanly instead of duplicating.
                        jcode_base::logging::warn(&format!(
                            "Transient API error after partial output; rolling back partial attempt and retrying: {}",
                            e
                        ));
                        let _ = tx
                            .send(Ok(StreamEvent::RetryRollback {
                                attempt: attempt + 2,
                                max: max_retries,
                            }))
                            .await;
                    } else {
                        jcode_base::logging::info(&format!(
                            "Transient API error, will retry: {}",
                            e
                        ));
                    }
                    next_retry_delay = jcode_provider_core::retry_after::retry_after_from_error(&e);
                    last_error = Some(e);
                    continue;
                }

                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }

    if let Some(e) = last_error {
        let _ = tx
            .send(Err(anyhow::anyhow!(
                "Failed after {} retries: {}",
                max_retries,
                e
            )))
            .await;
    }
}

/// Choose the delay before the next retry attempt.
///
/// An explicit server-suggested wait (e.g. a token-limit 422 telling us to
/// retry in N minutes) must be honored rather than truncated by the generic
/// backoff cap. Truncating it hammers an endpoint that is still over its
/// completion-token quota and exhausts the retry budget before the provider
/// recovers. The backoff cap only applies to the exponential-backoff fallback,
/// which is used when no server hint was recovered.
fn retry_delay_after_failure(
    attempt: u32,
    server_hint: Option<std::time::Duration>,
    backoff_cap: std::time::Duration,
) -> std::time::Duration {
    match server_hint {
        Some(hint) => hint,
        None => {
            jcode_provider_core::attempt_tracker::retry_backoff_delay(attempt, RETRY_BASE_DELAY_MS)
                .min(backoff_cap)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "stream helpers thread transport, auth, request, event channel, and pin state explicitly"
)]
async fn stream_response(
    client: Client,
    api_base: String,
    auth: ProviderAuth,
    send_openrouter_headers: bool,
    request: Value,
    tx: mpsc::Sender<Result<StreamEvent>>,
    provider_pin: Arc<Mutex<Option<ProviderPin>>>,
    model: String,
) -> Result<()> {
    use jcode_message_types::ConnectionPhase;
    let _ = tx
        .send(Ok(StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::SendingRequest,
        }))
        .await;
    let connect_start = std::time::Instant::now();
    let stream_idle_timeout = jcode_base::provider::stream_idle_timeout();

    let url = format!("{}/chat/completions", api_base);
    let mut req = apply_kimi_coding_agent_headers(
        auth.apply(
            client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept-Encoding", "identity"),
        )
        .await?,
        &api_base,
        Some(&model),
    );

    if send_openrouter_headers {
        req = req
            .header("HTTP-Referer", "https://github.com/jcode")
            .header("X-Title", "jcode");
    }

    let payload = serde_json::to_vec(&request).unwrap_or_default();
    let payload_bytes = payload.len() as u64;
    if payload_bytes > 0 {
        let _ = tx
            .send(Ok(StreamEvent::StatusDetail {
                detail: format!(
                    "sending {}",
                    jcode_provider_core::transport::readable_bytes(payload_bytes)
                ),
            }))
            .await;
    }

    let upload_tx = tx.clone();
    let response =
        jcode_provider_core::transport::send_body_with_upload_progress(
            req,
            payload,
            stream_idle_timeout,
            move |sent| {
                let detail = jcode_provider_core::transport::upload_progress_label(
                    sent,
                    payload_bytes,
                );
                if !detail.is_empty() {
                    let _ = upload_tx.try_send(Ok(StreamEvent::StatusDetail {
                        detail,
                    }));
                }
            },
            |msg: &str| {
                let tx = tx.clone();
                let msg = msg.to_string();
                async move {
                    let _ = tx
                        .send(Ok(StreamEvent::StatusDetail { detail: msg }))
                        .await;
                }
            },
        )
        .await
        .with_context(|| {
            let hint = local_endpoint_troubleshooting_hint(&api_base, &model);
            format!(
                "Failed to send OpenAI-compatible chat request\n  endpoint: {}\n  model: {}\n  auth: {}\n{}",
                url,
                model,
                auth.label(),
                hint
            )
        })?;

    let connect_ms = connect_start.elapsed().as_millis();
    jcode_base::logging::info(&format!(
        "HTTP connection established in {}ms (status={})",
        connect_ms,
        response.status()
    ));

    if !response.status().is_success() {
        let status = response.status();
        let retry_after = jcode_provider_core::retry_after::retry_after(response.headers());
        let body = jcode_base::util::http_error_body(response, "HTTP error").await;
        let hint = if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
            payload_too_large_hint()
        } else {
            local_endpoint_troubleshooting_hint(&api_base, &model)
        };

        // A 422 from an OpenAI-compatible endpoint with a token-limit body is a
        // transient exhaustion, not a permanent rejection: the request was
        // well-formed but the completion token cap would be exceeded, and a
        // short wait usually clears it. Treat it like a rate limit so the retry
        // loop reconnects after (preferably) the provider-suggested wait.
        // Gated by `is_token_limit_error` so unrelated 422s (malformed
        // requests, tool-schema rejections, etc.) are NOT retried.
        let token_limit_422 = status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
            && jcode_provider_core::token_limit::is_token_limit_error(&body);

        // Carry a retry hint to the retry loop: prefer an explicit Retry-After
        // header; otherwise synthesize one from the wait parsed out of the
        // token-limit body so the loop honours it instead of plain backoff.
        let retry_hint = if token_limit_422 {
            retry_after.or_else(|| {
                jcode_provider_core::token_limit::extract_wait_time(&body)
                    .map(jcode_provider_core::retry_after::RetryAfter::from_duration)
            })
        } else {
            retry_after
        };

        return Err(jcode_provider_core::retry_after::error_with_retry_after(
            format!(
                "OpenAI-compatible chat request failed\n  endpoint: {}\n  model: {}\n  auth: {}\n  status: {}\n  response: {}\n{}",
                url,
                model,
                auth.label(),
                status,
                body,
                hint
            ),
            retry_hint,
        ));
    }

    let _ = tx
        .send(Ok(StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::WaitingForResponse,
        }))
        .await;

    let mut stream = OpenRouterStream::new(response.bytes_stream(), model.clone(), provider_pin);

    // Idle timeout between streamed chunks. Configurable so slow reasoning
    // models (e.g. DeepSeek) that think silently for minutes before emitting
    // tokens don't trip a premature timeout (issue #196). Resolved from
    // `[provider] stream_idle_timeout_secs` / `JCODE_STREAM_IDLE_TIMEOUT_SECS`,
    // defaulting to 180s. Shared with the native provider paths (issue #434).
    let idle_timeout_secs = stream_idle_timeout.as_secs();

    loop {
        let event = match tokio::time::timeout(stream_idle_timeout, stream.next()).await {
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(e))) => anyhow::bail!(
                "OpenAI-compatible stream error\n  endpoint: {}\n  model: {}\n  auth: {}\n  error: {}",
                url,
                model,
                auth.label(),
                e
            ),
            Ok(None) => break, // stream ended normally
            Err(_) => {
                jcode_base::logging::warn(&format!(
                    "OpenRouter SSE stream timed out (no data for {}s)",
                    idle_timeout_secs
                ));
                anyhow::bail!(
                    "OpenAI-compatible stream timeout\n  endpoint: {}\n  model: {}\n  auth: {}\n  timeout: no data received for {} seconds\n{}",
                    url,
                    model,
                    auth.label(),
                    idle_timeout_secs,
                    local_endpoint_troubleshooting_hint(&api_base, &model)
                );
            }
        };
        if tx.send(Ok(event)).await.is_err() {
            return Ok(());
        }
    }

    Ok(())
}

/// Extract the HTTP status code reported in a formatted provider error string.
///
/// Error strings produced in this module embed the status as `status: <code>`
/// (e.g. `status: 402 Payment Required`). The input may be lowercased before
/// it reaches here, so matching is case-insensitive.
fn parsed_http_status(error_str: &str) -> Option<u16> {
    let lower = error_str.to_ascii_lowercase();
    let idx = lower.find("status:")?;
    let rest = lower[idx + "status:".len()..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

fn is_retryable_error(error_str: &str) -> bool {
    // A 422 that is a token-limit exhaustion is transient and should be
    // retried (issue: OpenAI-compatible providers return 422 when the
    // completion-token cap would be exceeded; the request is well-formed).
    // Detect it from the body markers before the hard 4xx exclusion below.
    if parsed_http_status(error_str) == Some(422)
        && jcode_provider_core::token_limit::is_token_limit_error(error_str)
    {
        return true;
    }

    // Explicit non-retryable HTTP statuses take precedence over the loose
    // substring heuristics below. These are deterministic client-side failures
    // (auth, billing, malformed request) where retrying is futile and just
    // burns time/credits. 429 (rate limit) is classified explicitly so it does
    // not depend on provider-specific body wording.
    match parsed_http_status(error_str) {
        Some(400 | 401 | 402 | 403 | 404 | 405 | 406 | 422) => return false,
        Some(429) => return true,
        _ => {}
    }

    jcode_provider_core::is_transient_transport_error(error_str)
        || error_str.contains("stream error")
        || error_str.contains("eof")
        || error_str.contains("5")
            && (error_str.contains("50")
                || error_str.contains("502")
                || error_str.contains("503")
                || error_str.contains("504")
                || error_str.contains("internal server error"))
        || error_str.contains("overloaded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_endpoint_hint_mentions_ollama_actions() {
        let hint = local_endpoint_troubleshooting_hint("http://localhost:11434/v1", "llama3.2");
        assert!(hint.contains("ollama serve"));
        assert!(hint.contains("ollama pull"));
        assert!(hint.contains("--provider ollama"));
    }

    #[test]
    fn local_endpoint_hint_mentions_lm_studio_server() {
        let hint = local_endpoint_troubleshooting_hint("http://127.0.0.1:1234/v1", "local-model");
        assert!(hint.contains("LM Studio"));
        assert!(hint.contains("Local Server"));
        assert!(hint.contains("/v1/models"));
    }

    #[test]
    fn payload_too_large_hint_is_about_request_size_not_network() {
        // A 413 is a request-body-size rejection; the hint must not blame
        // DNS/TLS or network connectivity, and should point at shrinking the
        // transcript (images / tool outputs).
        let hint = payload_too_large_hint();
        assert!(hint.contains("serialized body exceeded"));
        assert!(hint.contains("not a network error"));
        assert!(hint.contains("tool outputs"));
        assert!(!hint.contains("DNS/TLS"));
        assert!(!hint.contains("network connectivity"));
        assert!(!hint.to_lowercase().contains("dns"));
    }

    #[test]
    fn parsed_http_status_extracts_code() {
        assert_eq!(
            parsed_http_status("status: 402 payment required"),
            Some(402)
        );
        assert_eq!(parsed_http_status("  status:404 not found"), Some(404));
        assert_eq!(parsed_http_status("no status here"), None);
        // Embedded numbers elsewhere must not be misread as a status.
        assert_eq!(parsed_http_status("you requested 65536 tokens"), None);
    }

    #[test]
    fn retry_delay_honors_server_hint_over_backoff_cap() {
        use std::time::Duration;
        let cap = Duration::from_secs(30);

        // A server-suggested wait (e.g. a token-limit 422 "retry in N min")
        // must be honored even when it exceeds the generic backoff cap. The
        // pre-fix code truncated every delay at `cap`, hammering an endpoint
        // still over its completion-token quota.
        let hint = Duration::from_secs(6 * 60);
        assert_eq!(retry_delay_after_failure(1, Some(hint), cap), hint);
        assert_eq!(retry_delay_after_failure(8, Some(hint), cap), hint);

        // Shorter server hints pass through unchanged.
        let short = Duration::from_secs(5);
        assert_eq!(retry_delay_after_failure(3, Some(short), cap), short);

        // Without a hint, the capped exponential backoff is used.
        let backed_off = retry_delay_after_failure(4, None, cap);
        assert!(
            backed_off <= cap,
            "backoff fallback must stay within the cap"
        );
    }

    #[test]
    fn payment_required_is_not_retryable() {
        let err = "openai-compatible chat request failed\n  endpoint: \
            https://openrouter.ai/api/v1/chat/completions\n  model: openai/gpt-5.4\n  \
            auth: openrouter_api_key\n  status: 402 payment required\n  response: \
            {\"error\":{\"message\":\"this request requires more credits, or fewer \
            max_tokens. you requested up to 65536 tokens, but can only afford 34424\"}}";
        assert!(!is_retryable_error(err));
    }

    #[test]
    fn client_errors_are_not_retryable() {
        for status in [400u16, 401, 402, 403, 404, 405, 406, 422] {
            let err = format!("chat request failed\n  status: {status} client error");
            assert!(
                !is_retryable_error(&err),
                "status {status} should not be retryable"
            );
        }
    }

    #[test]
    fn server_errors_remain_retryable() {
        assert!(is_retryable_error(
            "chat request failed\n  status: 503 service unavailable"
        ));
        assert!(is_retryable_error(
            "chat request failed\n  status: 500 internal server error"
        ));
        // Provider overload messages should still be retried.
        assert!(is_retryable_error("overloaded"));
    }

    #[test]
    fn http_429_is_retryable_without_rate_limit_words_in_body() {
        assert!(is_retryable_error(
            "chat request failed\n  status: 429 unknown\n  response: {}"
        ));
    }

    #[test]
    fn token_limit_422_is_retryable() {
        // OpenAI-compatible endpoints return 422 when the completion-token cap
        // would be exceeded; the request is well-formed and a wait clears it.
        assert!(is_retryable_error(
            "openai-compatible chat request failed\n  endpoint: https://your-provider/chat/completions\n  model: DeepSeek-V4-Flash\n  auth: OPENAI_COMPAT_API_KEY\n  status: 422 Unprocessable Entity\n  response: {\"error\":\"Превышен лимит completion-токенов: использовано 60323, лимит 60000. Повторите попытку через 18 мин.\"}"
        ));
        assert!(is_retryable_error(
            "openai-compatible chat request failed\n  status: 422 unprocessable entity\n  response: {\"error\": \"Token limit exceeded: used 5000, limit 4096\"}"
        ));
    }

    #[test]
    fn unrelated_422_remains_non_retryable() {
        // A 422 that is NOT a token-limit error (malformed request, unsupported
        // model, tool-schema rejection) must stay non-retryable.
        assert!(!is_retryable_error(
            "chat request failed\n  status: 422 unprocessable entity\n  response: {\"error\": \"invalid request payload\"}"
        ));
        assert!(!is_retryable_error(
            "chat request failed\n  status: 422 unprocessable entity\n  response: {\"error\": \"model is not supported\"}"
        ));
    }
}
