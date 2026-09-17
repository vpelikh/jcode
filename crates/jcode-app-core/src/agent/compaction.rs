use super::*;
use anyhow::Context;

impl Agent {
    pub(super) fn note_compaction_applied(&mut self) {
        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;
    }

    /// Invalidate provider context after a deterministic prune. Unlike a real
    /// compaction, a prune does not change tool definitions, so it preserves
    /// the locked tool surface (which `note_compaction_applied` clears). It
    /// still resets the provider session and cache, because the shrunk
    /// transcript no longer matches what the provider cached. This path runs
    /// on the frequent scheduled per-step prune, so preserving `locked_tools`
    /// avoids churning the tool surface mid-turn.
    pub(super) fn note_prune_applied(&mut self) {
        self.cache_tracker.reset();
        self.provider_session_id = None;
        self.session.provider_session_id = None;
        // The per-step prune shrunk existing content in the consumed prefix
        // without changing message counts. Tell the compaction manager exactly
        // how much content now remains so it reseeds its rolling char estimate
        // from the already-pruned transcript instead of keeping a stale
        // (over-counted) pre-prune figure.
        if let Ok(mut manager) = self.registry.compaction().try_write() {
            let provider_messages = self.session.messages_for_provider();
            manager.note_prune_applied(&provider_messages);
        }
    }

    /// Full-reseed the compaction manager after a one-shot manual prune (the
    /// `/prune` command / server route). This is the heavier, user-invoked
    /// counterpart to the cheap per-step [`Self::note_prune_applied`]: it resets
    /// the manager and rebuilds from the already-pruned transcript, exactly like
    /// the TUI `/prune` handler's `reseed_compaction_from_provider_messages` and
    /// the 413 recovery path. Rebuilding (rather than the light set_exact
    /// recompute) is correct here because a manual full prune supersedes any
    /// in-flight background compaction. Provider/cache state is reset too, so
    /// the next request sends the reduced payload.
    pub(super) fn reseed_compaction_from_pruned_transcript(&mut self) {
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            let provider_messages = self.session.messages_for_provider();
            manager.reset();
            manager.set_budget(self.provider.context_window());
            if let Some(state) = self.session.compaction.as_ref() {
                manager.restore_persisted_state_with(state, &provider_messages);
            } else {
                manager.seed_restored_messages_with(&provider_messages);
            }
            self.sync_session_compaction_state_from_manager(&manager);
        }
        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;
    }

    /// Physically consolidate a just-completed compaction when
    /// `[compaction] physically_consolidate` is enabled (deepseek-harness
    /// takeaway #5). Callers pass the already-write-locked manager guard, so
    /// the helper does not re-acquire the compaction lock.
    ///
    /// The manager has already applied the compaction internally (advanced
    /// `compacted_count` and set `active_summary`) but left `session.messages`
    /// un-rewritten (the virtual model). This helper rewrites the transcript to
    /// `[summary_message, recent_tail...]` via
    /// [`Session::physically_consolidate_compaction`], which records a balanced
    /// `CompactionStart`/`CompactionEnd` bracket and marks the persisted state
    /// `physically_consolidated`. It then marks the manager physically
    /// consolidated so the next provider-view derivation returns the transcript
    /// as-is instead of double-prepending the summary.
    ///
    /// Returns `true` when a physical consolidation was applied, `false`
    /// otherwise (feature disabled, no active summary, or invalid span).
    pub(super) fn physically_consolidate_if_enabled(
        &mut self,
        manager: &mut crate::compaction::CompactionManager,
    ) -> bool {
        if !crate::config::config().compaction.physically_consolidate {
            return false;
        }
        // Only a genuine (non-empty) summary can be physically consolidated; a
        // compaction that produced nothing has no summary to place at index 0.
        let Some(state) = manager.persisted_state() else {
            return false;
        };
        let manager_compacted = manager.compacted_count();
        if manager_compacted == 0 {
            return false;
        }
        // Build the recent tail from the still-un-rewritten transcript. The
        // manager applied the compaction virtually, so messages[compacted_count..]
        // are the messages that survive the cut.
        let start = manager_compacted.min(self.session.messages.len());
        let tail = self.session.messages[start..].to_vec();
        let applied = self
            .session
            .physically_consolidate_compaction(
                crate::id::new_id("compact"),
                state.summary_text.clone(),
                state.openai_encrypted_content.clone(),
                state.covers_up_to_turn,
                state.original_turn_count,
                state.compacted_count,
                tail,
            )
            .is_some();
        if applied {
            manager.mark_physically_consolidated();
        }
        applied
    }

    pub fn poll_compaction_completion_event(&mut self) -> Option<CompactionEvent> {
        let provider_messages = self.session.messages_for_provider();
        let compaction = self.registry.compaction();
        let event = match compaction.try_write() {
            Ok(mut manager) => {
                let event = manager.poll_compaction_event_with(&provider_messages);
                if event.is_some() {
                    let consolidated = self.physically_consolidate_if_enabled(&mut manager);
                    if !consolidated {
                        self.sync_session_compaction_state_from_manager(&manager);
                    }
                }
                event
            }
            Err(_) => return None,
        };

        if event.is_some() {
            self.note_compaction_applied();
            self.persist_session_best_effort("compaction completion");
        }

        event
    }

    pub fn request_manual_compaction(&mut self) -> (String, bool) {
        if !self.provider.supports_compaction() {
            return (
                "Manual compaction is not available for this provider.".to_string(),
                false,
            );
        }

        let provider = self.provider.fork();
        let messages = self.session.messages_for_provider();
        let compaction = self.registry.compaction();

        match compaction.try_write() {
            Ok(mut manager) => {
                let stats = manager.stats_with(&messages);
                let status_msg = format!(
                    "**Context Status:**\n\
                    • Messages: {} (active), {} (total history)\n\
                    • Token usage: ~{}k (estimate ~{}k) / {}k ({:.1}%)\n\
                    • Has summary: {}\n\
                    • Compacting: {}",
                    stats.active_messages,
                    stats.total_turns,
                    stats.effective_tokens / 1000,
                    stats.token_estimate / 1000,
                    manager.token_budget() / 1000,
                    stats.context_usage * 100.0,
                    if stats.has_summary { "yes" } else { "no" },
                    if stats.is_compacting {
                        "in progress..."
                    } else {
                        "no"
                    }
                );

                match manager.force_compact_with(&messages, provider) {
                    Ok(()) => (
                        format!(
                            "{}\n\n📦 **Compacting context** (manual) — summarizing older messages in the background to stay within the context window.\n\
                            The summary will be applied automatically when ready.",
                            status_msg
                        ),
                        true,
                    ),
                    Err(reason) => (
                        format!("{status_msg}\n\n⚠ **Cannot compact:** {reason}"),
                        false,
                    ),
                }
            }
            Err(_) => (
                "⚠ Cannot access compaction manager (lock held)".to_string(),
                false,
            ),
        }
    }

    /// Run a deterministic, model-free prune over the session transcript
    /// (takeaway #6): shrink any oversized tool result / inline image under the
    /// per-node caps, without invoking the summarizer. Returns a report of what
    /// was pruned and a human-readable message.
    pub fn request_manual_prune(&mut self) -> Result<(crate::compaction::prune::PruneReport, String)> {
        let report = self
            .session
            .prune_transcript(&crate::compaction::prune::PrunePolicy::node_caps_with(
                crate::config::config().compaction.prune_tool_result_max_bytes,
                crate::config::config().compaction.prune_image_max_bytes,
            ));
        let message = if report.is_empty() {
            "Prune: nothing oversized to shrink (context already within per-node caps).".to_string()
        } else {
            format!(
                "Pruned transcript: replaced {} oversized image(s) and truncated {} oversized tool result(s).",
                report.images_stripped, report.tool_results_truncated
            )
        };
        if !report.is_empty() {
            // One-shot manual prune over the whole transcript: full-reseed the
            // compaction manager from the already-pruned transcript (not the
            // cheap per-step recompute), so this user-invoked prune matches the
            // TUI `/prune` handler's `reseed_compaction_from_provider_messages`
            // and the 413 recovery path. Resetting also supersedes any in-flight
            // background compaction and drops the pre-prune over-count.
            self.reseed_compaction_from_pruned_transcript();
        }
        // Also retry persistence on a no-op after a previous failed save.
        self.session
            .save()
            .context("Prune applied in memory but failed to save session")?;
        Ok((report, message))
    }

    fn is_context_limit_error(error: &str) -> bool {
        let lower = error.to_lowercase();
        lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("maximum context")
            || lower.contains("max context")
            || lower.contains("token limit")
            || lower.contains("too many tokens")
            || lower.contains("prompt is too long")
            || lower.contains("input is too long")
            || lower.contains("request too large")
            || lower.contains("length limit")
            || lower.contains("maximum tokens")
            || (lower.contains("exceeded") && lower.contains("tokens"))
    }

    /// Best-effort emergency recovery after a context-limit or provider
    /// request-too-large (HTTP 413) error.
    ///
    /// Attempts (in order): OpenAI-encrypted-content recovery, stripping oversized
    /// inline images / truncating oversized tool results (413), then a synchronous
    /// hard compaction. On success it resets provider session state so the caller
    /// can retry the same turn immediately.
    pub(super) fn try_auto_compact_after_context_limit(&mut self, error: &str) -> bool {
        if crate::provider::openai_request::is_openai_encrypted_content_too_large_error(error)
            && self.try_recover_oversized_openai_native_compaction()
        {
            return true;
        }
        // A provider HTTP 413 ("request too large") is a *byte-size* failure
        // driven by inline base64 images, not a token-context overflow. Token
        // accounting deliberately undercounts images, so ordinary compaction
        // would not shrink the payload and the retry would 413 again. Strip
        // oversized images first.
        if self.try_recover_after_payload_too_large(error) {
            return true;
        }

        // A 413 that image/tool-result stripping could not resolve (e.g. the
        // payload is large because of accumulated message volume rather than any
        // single oversized image or tool result) still needs the request body
        // shrunk, so fall through to a hard compaction to drop older messages.
        let is_payload_too_large = crate::compaction::is_request_payload_too_large_error(error);

        if !is_payload_too_large && !Self::is_context_limit_error(error) {
            return false;
        }
        if !self.provider.supports_compaction() {
            return false;
        }

        let context_limit = self.provider.context_window() as u64;
        let compaction = self.registry.compaction();

        let (dropped, usage_pct) = match compaction.try_write() {
            Ok(mut manager) => {
                let (dropped, usage_pct) = {
                    let all_messages = self.session.provider_messages();
                    manager.update_observed_input_tokens(context_limit);
                    let usage_pct = manager.context_usage_with(all_messages) * 100.0;
                    let dropped = match manager.hard_compact_with(all_messages) {
                        Ok(dropped) => dropped,
                        Err(reason) => {
                            if is_payload_too_large {
                                logging::warn(&format!(
                                    "Request-too-large recovery failed: hard compact failed ({})",
                                    reason
                                ));
                            } else {
                                logging::warn(&format!(
                                    "Context-limit auto-recovery failed: hard compact failed ({})",
                                    reason
                                ));
                            }
                            return false;
                        }
                    };
                    (dropped, usage_pct)
                };
                let consolidated = self.physically_consolidate_if_enabled(&mut manager);
                if !consolidated {
                    self.sync_session_compaction_state_from_manager(&manager);
                }
                (dropped, usage_pct)
            }
            Err(_) => {
                if is_payload_too_large {
                    logging::warn(
                        "Request-too-large recovery skipped: compaction manager lock busy",
                    );
                } else {
                    logging::warn("Context-limit auto-recovery skipped: compaction manager lock busy");
                }
                return false;
            }
        };

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        if is_payload_too_large {
            logging::warn(&format!(
                "Request body exceeded provider size limit; hard-compacted to shrink accumulated messages (dropped {} messages, usage was {:.1}%)",
                dropped, usage_pct
            ));
            crate::runtime_memory_log::emit_event(
                crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                    "payload_too_large_hard_compacted",
                    "request_payload_too_large",
                )
                .with_session_id(self.session.id.clone())
                .with_detail(format!("dropped_messages={dropped},usage_pct={usage_pct:.1}"))
                .force_attribution(),
            );
        } else {
            logging::warn(&format!(
                "Context limit exceeded; auto-compacted and retrying (dropped {} messages, usage was {:.1}%)",
                dropped, usage_pct
            ));
            crate::runtime_memory_log::emit_event(
                crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                    "auto_compaction_applied",
                    "context_limit_auto_compaction",
                )
                .with_session_id(self.session.id.clone())
                .with_detail(format!(
                    "dropped_messages={dropped},usage_pct={usage_pct:.1}"
                ))
                .force_attribution(),
            );
        }

        // For request-too-large, only report success if hard compaction actually
        // dropped messages. Hard compact can no-op (e.g. already compacted to
        // the floor); if nothing was dropped, the payload would not shrink and a
        // retry would 413 again, so report failure to avoid a pointless loop.
        if is_payload_too_large && dropped == 0 {
            logging::warn(
                "Hard compaction did not drop any messages; request-too-large recovery cannot shrink the payload",
            );
            return false;
        }

        true
    }

    /// Message for the terminal error surfaced after compaction retries are
    /// exhausted. Compaction here is triggered both by token-context overflow
    /// and by provider 413 "request too large" (byte-size) rejections, so the
    /// wording must match the actual failure rather than always blaming the
    /// context window.
    pub(super) fn compaction_retry_limit_error(&self, err_str: &str) -> String {
        if crate::compaction::is_request_payload_too_large_error(err_str) {
            format!(
                "Request body still exceeds provider size limit after {} compaction retries; start a new conversation (/new) or compact manually (/compact)",
                Self::MAX_CONTEXT_LIMIT_RETRIES
            )
        } else {
            format!(
                "Context limit exceeded after {} compaction retries",
                Self::MAX_CONTEXT_LIMIT_RETRIES
            )
        }
    }

    /// Best-effort recovery after a provider HTTP 413 "request too large" error.
    ///
    /// This failure is caused by the serialized request body (dominated by inline
    /// base64 images and/or accumulated large tool outputs) exceeding the
    /// provider's size cap, which is independent of the token context window. We
    /// strip oversized images from the persisted transcript first, oldest-first,
    /// down to a conservative byte budget; if the oversized payload is instead
    /// driven by large tool-result text (the common case for a coding session
    /// with big file/cat/read outputs), we truncate those tool results as a
    /// fallback. Either way we then reset the provider session/cache so the
    /// caller can retry the same turn immediately.
    fn try_recover_after_payload_too_large(&mut self, error: &str) -> bool {
        if !crate::compaction::is_request_payload_too_large_error(error) {
            return false;
        }

        // Run the deterministic prune stage (takeaway #6) with the shared
        // HTTP 413 policy: strip images oldest-first to the emergency image
        // budget, and only if that reclaims no image, trim tool results to the
        // payload tool-result budget. The single `prune_contents` entrypoint
        // preserves the historical escalation order and report; the policy
        // lives in one place so every 413 call site stays in lockstep.
        let report = self
            .session
            .prune_transcript(&crate::compaction::prune::PrunePolicy::payload_413());
        let stripped = report.images_stripped;
        let truncated = report.tool_results_truncated;

        if stripped == 0 && truncated == 0 {
            logging::warn(
                "Request-too-large recovery skipped: no oversized inline images or tool results to strip",
            );
            return false;
        }

        // Persist the prune immediately so the mutation survives even if the
        // retry is interrupted; the caller also saves on a successful retry.
        if let Err(err) = self.session.save() {
            logging::warn(&format!(
                "Failed to persist 413 prune for session {}: {}",
                self.session.id, err
            ));
        }

        // The transcript changed; reseed compaction bookkeeping and reset
        // provider session/cache state so the retry sends the reduced payload.
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            let provider_messages = self.session.messages_for_provider();
            manager.reset();
            manager.set_budget(self.provider.context_window());
            if let Some(state) = self.session.compaction.as_ref() {
                manager.restore_persisted_state_with(state, &provider_messages);
            } else {
                manager.seed_restored_messages_with(&provider_messages);
            }
            self.sync_session_compaction_state_from_manager(&manager);
        }

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        logging::warn(&format!(
            "Request body exceeded provider size limit; stripped {} oversized inline image(s), truncated {} tool result(s), retrying",
            stripped, truncated
        ));
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "payload_too_large_recovered",
                "request_payload_too_large",
            )
            .with_session_id(self.session.id.clone())
            .with_detail(format!("images_stripped={stripped},tool_results_truncated={truncated}"))
            .force_attribution(),
        );

        true
    }

    fn try_recover_oversized_openai_native_compaction(&mut self) -> bool {
        let compaction = self.registry.compaction();
        let recovered = match compaction.try_write() {
            Ok(mut manager) => {
                if !manager.discard_oversized_openai_native_compaction() {
                    return false;
                }
                self.sync_session_compaction_state_from_manager(&manager);
                true
            }
            Err(_) => {
                logging::warn(
                    "OpenAI native compaction recovery skipped: compaction manager lock busy",
                );
                false
            }
        };

        if !recovered {
            return false;
        }

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        logging::warn(
            "OpenAI native compaction payload exceeded provider size limit; discarded native state and retrying with text fallback",
        );
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "native_compaction_payload_recovered",
                "openai_encrypted_content_too_large",
            )
            .with_session_id(self.session.id.clone())
            .force_attribution(),
        );

        true
    }

    fn effective_context_tokens_from_usage(
        &self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) -> u64 {
        // Shared heuristic (jcode-compaction-core): keeps the compaction
        // manager's observed-token feed consistent with the client-side
        // context display.
        crate::compaction::effective_context_tokens_from_usage(
            self.provider.name(),
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        )
    }

    pub(super) fn update_compaction_usage_from_stream(
        &mut self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) {
        if !self.provider.uses_jcode_compaction() || input_tokens == 0 {
            return;
        }
        let observed = self.effective_context_tokens_from_usage(
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        );
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.update_observed_input_tokens(observed);
            manager.push_token_snapshot(observed);
        };
    }

    /// Push an embedding snapshot for the semantic compaction mode.
    /// Called after each assistant turn with a short text snippet.
    /// No-op if the embedding model is unavailable or mode is not semantic.
    pub(super) fn push_embedding_snapshot_if_semantic(&mut self, text: &str) {
        use crate::config::CompactionMode;
        let is_semantic = {
            let compaction = self.registry.compaction();
            compaction
                .try_read()
                .map(|m| m.mode() == CompactionMode::Semantic)
                .unwrap_or(false)
        };
        if !is_semantic {
            return;
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.push_embedding_snapshot(text);
        };
    }
}
