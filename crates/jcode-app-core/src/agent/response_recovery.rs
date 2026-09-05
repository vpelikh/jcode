use super::*;

impl Agent {
    fn parse_text_wrapped_tool_call(
        text: &str,
    ) -> Option<(String, String, serde_json::Value, String)> {
        let marker = "to=functions.";
        let marker_idx = text.find(marker)?;
        let after_marker = &text[marker_idx + marker.len()..];

        let mut tool_name_end = 0usize;
        for (idx, ch) in after_marker.char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                tool_name_end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if tool_name_end == 0 {
            return None;
        }

        let tool_name = after_marker[..tool_name_end].to_string();
        let remaining = &after_marker[tool_name_end..];
        let mut fallback: Option<(String, String, serde_json::Value, String)> = None;

        for (brace_idx, ch) in remaining.char_indices() {
            if ch != '{' {
                continue;
            }
            let slice = &remaining[brace_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            let parsed = match stream.next() {
                Some(Ok(value)) => value,
                Some(Err(_)) | None => continue,
            };
            let consumed = stream.byte_offset();
            if !parsed.is_object() {
                continue;
            }

            let prefix = text[..marker_idx].trim_end().to_string();
            let suffix = remaining[brace_idx + consumed..].trim().to_string();
            if suffix.is_empty() {
                return Some((prefix, tool_name.clone(), parsed, suffix));
            }
            if fallback.is_none() {
                fallback = Some((prefix, tool_name.clone(), parsed, suffix));
            }
        }

        fallback
    }

    pub(super) fn recover_text_wrapped_tool_call(
        &self,
        text_content: &mut String,
        tool_calls: &mut Vec<ToolCall>,
    ) -> bool {
        if !tool_calls.is_empty() || text_content.trim().is_empty() {
            return false;
        }

        let Some((prefix, tool_name, arguments, suffix)) =
            Self::parse_text_wrapped_tool_call(text_content)
        else {
            return false;
        };

        let mut sanitized = String::new();
        if !prefix.is_empty() {
            sanitized.push_str(&prefix);
        }
        if !suffix.is_empty() {
            if !sanitized.is_empty() {
                sanitized.push('\n');
            }
            sanitized.push_str(&suffix);
        }
        *text_content = sanitized;

        let call_id = format!("fallback_text_call_{}", id::new_id("call"));
        let recovered_total = RECOVERED_TEXT_WRAPPED_TOOL_CALLS
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        logging::warn(&format!(
            "[agent] Recovered text-wrapped tool call for '{}' ({}, total={})",
            tool_name, call_id, recovered_total
        ));
        let intent = ToolCall::intent_from_input(&arguments);
        tool_calls.push(ToolCall {
            id: call_id,
            name: tool_name,
            input: arguments,
            intent,
            thought_signature: None,
        });

        true
    }

    pub(crate) fn should_continue_after_stop_reason(stop_reason: &str) -> bool {
        let reason = stop_reason.trim().to_ascii_lowercase();
        if reason.is_empty() {
            return false;
        }

        if matches!(reason.as_str(), "stop" | "end_turn" | "tool_use") {
            return false;
        }

        reason.contains("incomplete")
            || reason.contains("max_output_tokens")
            || reason.contains("max_tokens")
            || reason.contains("length")
            || reason.contains("trunc")
            || reason.contains("commentary")
    }

    /// True when the provider's stop reason indicates a model-side
    /// guardrail/safety stop (e.g. Anthropic `refusal`), as opposed to a
    /// normal end-of-turn or truncation.
    pub(crate) fn is_guardrail_stop_reason(stop_reason: Option<&str>) -> bool {
        let Some(reason) = stop_reason else {
            return false;
        };
        let reason = reason.trim().to_ascii_lowercase();
        matches!(reason.as_str(), "refusal" | "content_filter" | "safety")
            || reason.contains("guardrail")
            || reason.contains("policy_violation")
    }

    pub(crate) const FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS: [&'static str; 3] = [
        "Reassess the user's request using its full context and likely intent rather than isolated keywords. Determine whether answering would enable a concrete harmful action. If no specific safety concern applies, answer normally. Preserve the refusal only when it is genuinely warranted, and briefly identify the specific concern.",
        "Review the request again and distinguish legitimate coding, debugging, defensive, educational, or analytical work from instructions that facilitate harm. If only part of the request is unsafe, help with the safe portions and narrowly limit the rest instead of issuing a blanket refusal.",
        "Perform one final, independent policy check. Prefer a safe and useful constrained response when possible. Refuse only the specific content that creates a concrete safety risk; otherwise continue with the user's actual task. Do not weaken a refusal that remains genuinely necessary.",
    ];

    /// Try a small sequence of differently framed policy checks after Fable
    /// guardrails a response. Every prompt preserves warranted refusals, and the
    /// fixed suite size prevents an unbounded refusal/retry loop.
    pub(crate) fn maybe_reconsider_fable_guardrail(
        &mut self,
        stop_reason: Option<&str>,
        attempts: &mut u32,
    ) -> Result<bool> {
        let model = self.provider.model();
        if !Self::should_reconsider_fable_guardrail(
            &model,
            stop_reason,
            *attempts,
            Self::FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS.len() as u32,
        ) {
            return Ok(false);
        }

        let prompt = Self::FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS[*attempts as usize];
        *attempts += 1;
        logging::warn(&format!(
            "Fable 5 guardrail stopped the response (stop_reason={:?}); trying reconsideration prompt {}/{}",
            stop_reason,
            attempts,
            Self::FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS.len(),
        ));
        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: prompt.to_string(),
                cache_control: None,
            }],
        );
        self.session.save()?;
        Ok(true)
    }

    pub(crate) fn should_reconsider_fable_guardrail(
        model: &str,
        stop_reason: Option<&str>,
        attempts: u32,
        max_attempts: u32,
    ) -> bool {
        Self::is_guardrail_stop_reason(stop_reason)
            && model.to_ascii_lowercase().contains("fable-5")
            && attempts < max_attempts
    }

    /// Builds the user-facing notice for a turn that ended with no visible
    /// assistant output (no text, no tool calls). Returns `None` when the turn
    /// looks normal and no notice should be surfaced.
    pub(crate) fn provider_guardrail_notice(
        stop_reason: Option<&str>,
        visible_text_empty: bool,
        had_reasoning: bool,
    ) -> Option<String> {
        let guardrail = Self::is_guardrail_stop_reason(stop_reason);
        if !guardrail && !visible_text_empty {
            return None;
        }
        let reason_label = stop_reason
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("unknown");
        if guardrail {
            return Some(format!(
                "Provider guardrail stopped the response (stop_reason: {}). The model declined to answer this request. Rephrasing, narrowing the request, or providing more context may help.",
                reason_label
            ));
        }
        // Empty visible output with a non-guardrail stop reason: still surface,
        // since the user otherwise sees nothing at all. Do not assert a content
        // filter here: in practice this is usually a transient upstream failure
        // (a dropped or empty stream), not a provider guardrail (issue #672).
        let reasoning_hint = if had_reasoning {
            " after producing only internal reasoning"
        } else {
            ""
        };
        Some(format!(
            "The model ended its turn without any visible output{} (stop_reason: {}). The provider returned an empty response; this is usually a transient upstream failure rather than a content filter. Retrying the request may help.",
            reasoning_hint, reason_label
        ))
    }

    /// Log-event label for an empty final turn: real guardrail stops keep the
    /// `PROVIDER_GUARDRAIL` name, transient empty responses get their own so
    /// the two are separable in logs (issue #672).
    pub(crate) fn empty_turn_log_event(stop_reason: Option<&str>) -> &'static str {
        if Self::is_guardrail_stop_reason(stop_reason) {
            "PROVIDER_GUARDRAIL"
        } else {
            "PROVIDER_EMPTY_RESPONSE"
        }
    }

    /// Retry a whitespace-only final response that arrived right after tool
    /// results, by asking the model to produce the final answer. Shared by the
    /// non-streaming and streaming (mpsc) turn loops so their recovery
    /// behavior cannot drift (issue #672). Returns true when a continuation
    /// message was injected and the caller should re-issue the request.
    pub(crate) fn maybe_continue_empty_post_tool_response(
        &mut self,
        visible_text_empty: bool,
        prompt_has_recent_tool_result: bool,
        stop_reason: Option<&str>,
        attempts: &mut u32,
    ) -> Result<bool> {
        if !visible_text_empty || !prompt_has_recent_tool_result {
            return Ok(false);
        }
        // A model-side refusal is deliberate; retrying it just burns tokens.
        if Self::is_guardrail_stop_reason(stop_reason) {
            return Ok(false);
        }
        if *attempts >= Self::MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS {
            return Ok(false);
        }
        *attempts += 1;
        logging::warn(&format!(
            "Provider returned whitespace-only final response after tool results (stop_reason={:?}); requesting final answer continuation (attempt {}/{})",
            stop_reason,
            attempts,
            Self::MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS
        ));
        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                // Keep this as a user-role message for provider compatibility,
                // but mark it as internal so transcript renderers never present
                // the synthetic recovery instruction as a prompt from the user.
                text: "<system-reminder>The previous provider response was empty after tool results. Provide the final answer to the user's last request using the tool results above. Do not call more tools unless absolutely necessary.</system-reminder>".to_string(),
                cache_control: None,
            }],
        );
        self.session.save()?;
        Ok(true)
    }

    fn continuation_prompt_for_stop_reason(stop_reason: &str) -> String {
        format!(
            "[System reminder: your previous response ended before completion (stop_reason: {}). Continue exactly where you left off, do not repeat completed content, and if the next step is a tool call, emit the tool call now.]",
            stop_reason.trim()
        )
    }

    pub(crate) fn maybe_continue_incomplete_response(
        &mut self,
        stop_reason: Option<&str>,
        attempts: &mut u32,
    ) -> Result<bool> {
        let Some(stop_reason) = stop_reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
        else {
            return Ok(false);
        };

        if !Self::should_continue_after_stop_reason(stop_reason) {
            return Ok(false);
        }

        if *attempts >= Self::MAX_INCOMPLETE_CONTINUATION_ATTEMPTS {
            logging::warn(&format!(
                "Response ended with stop_reason='{}' after {} continuation attempts; returning partial output",
                stop_reason, attempts
            ));
            return Ok(false);
        }

        *attempts += 1;
        logging::warn(&format!(
            "Response ended with stop_reason='{}'; requesting continuation (attempt {}/{})",
            stop_reason,
            attempts,
            Self::MAX_INCOMPLETE_CONTINUATION_ATTEMPTS
        ));

        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: Self::continuation_prompt_for_stop_reason(stop_reason),
                cache_control: None,
            }],
        );
        self.session.save()?;
        Ok(true)
    }

    /// Remove fenced code blocks (``` ... ```) from a turn's text. A genuinely
    /// complete answer can *quote* the very "Let me..." filler it is reporting
    /// or fixing (e.g. a diagnosis that reproduces a stalled excerpt, or a code
    /// example), which would otherwise be misclassified as a stall. Real stalled
    /// turns stream filler as prose (not inside a code fence), so stripping
    /// fenced blocks only removes quoting false-positives. Returns a new
    /// String only when a fence was found; otherwise returns the input slice.
    fn without_fenced_code_blocks(text: &str) -> std::borrow::Cow<'_, str> {
        let is_fence = |line: &str| line.trim_start().starts_with("```");
        if !text.lines().any(is_fence) {
            return std::borrow::Cow::Borrowed(text);
        }
        // Only strip fenced blocks when they are BALANCED (each opening has a
        // closing delimiter). A stray unclosed "```" (common from a streamed or
        // degenerate model) must not swallow the rest of the turn: that would
        // hide a genuine "I'll invoke..." stall behind an accidental fence.
        //
        // This is deliberately all-or-nothing (recall-first): when the marker
        // count is ODD, we cannot know which marker is the stray one, so ANY
        // greedy pairing could strip a block that actually contains the stall.
        // Returning the whole text as prose guarantees a stall is never hidden.
        // Each real observed stall streams filler as visible prose (never inside
        // a balanced code fence), so this loses no genuine coverage.
        let total = text.lines().filter(|line| is_fence(line)).count();
        if total % 2 != 0 {
            // Unbalanced fence delimiters: treat the whole text as prose so a
            // stall embedded around/after the stray marker stays detectable.
            return std::borrow::Cow::Borrowed(text);
        }
        let mut out = String::with_capacity(text.len());
        let mut in_fence = false;
        for line in text.lines() {
            if is_fence(line) {
                in_fence = !in_fence;
                continue;
            }
            if !in_fence {
                out.push_str(line);
                out.push('\n');
            }
        }
        std::borrow::Cow::Owned(out)
    }

    /// Detect an assistant turn that *promises* a concrete next action
    /// ("Let me read...", "Let me run...", "Let me grep...", "I'll ...") but
    /// stopped with a normal end-of-turn and no tool call. Some providers
    /// (observed: DeepSeek-V4-Flash via OpenRouter) degrade on very long
    /// contexts and produce these stalling filler turns repeatedly, which
    /// reads as "it says it'll do something but does nothing". We key on the
    /// *density* of action-promise phrases: legitimate turns rarely exceed a
    /// small fraction of intent words, while stalled filler turns are dense
    /// with them ("Let me ... Let me ... Let me run ..."). A single "let me"
    /// in a normal answer is not treated as stalling.
    pub(crate) fn is_stalled_promise_text(text: &str) -> bool {
        // Keep the ORIGINAL flattened length for the density denominator. A
        // genuine diagnosis can embed large fenced code blocks (diffs, repros)
        // alongside a handful of prose "let me" phrases. Counting density over
        // the fence-STRIPPED text would collapse the denominator and spike the
        // ratio, falsely flagging a legitimate answer. Measuring against the
        // full original length keeps the ratio honest. Real stalls stream prose
        // with no fences, so for them original == stripped and this is a no-op.
        let original_low_len = Self::flatten_whitespace(text).len();
        // Drop fenced code blocks first so quoting/reproducing a stalled
        // excerpt (common in review/diagnosis answers) does not count as a
        // stall. This runs on every check; without_fenced_code_blocks is a
        // cheap no-op when there is no "```".
        let text = Self::without_fenced_code_blocks(text);
        let low = Self::flatten_whitespace(&text);
        // Two independent failure modes produce "it promises an action but does
        // nothing". Each gets its own detector so one heuristic can't miss what
        // the other catches:
        //
        // 1. Dense rambling: the turn is full of matched action-promise phrases
        //    ("Let me ... Let me run ...") with no tool call. Detected by phrase
        //    density below.
        // 2. Compact explicit tool-request: a SHORT turn explicitly says it will
        //    invoke/call a tool (bash/command) "now" but ends without making the
        //    call (observed: DeepSeek via OpenRouter on long contexts, e.g.
        //    "I'll invoke bash now."). Such turns have only one or two promise
        //    phrases and would fall under the dense-rambling minimum count below,
        //    so we need a separate, much more specific signal.
        if Self::is_compact_unfulfilled_tool_request(&low, original_low_len) {
            return true;
        }
        let count = Self::count_action_promise_phrases(&low);
        // Require a non-trivial number of promise phrases: a single "let me"
        // in a short answer is normal and must not be treated as stalling.
        if count < Self::MIN_STALLED_PROMISE_PHRASE_COUNT {
            return false;
        }
        // Density: number of promise phrases per 100 chars. Measured failing
        // turns sit at ~4.4+ (224/4948, 107/2482). Legitimate turns, even huge
        // ones with a few "let me"s, stay below ~1. Require a healthy margin
        // above that. Uses the ORIGINAL (un-stripped) length so large balanced
        // fenced code blocks in a genuine answer do not inflate the ratio.
        let density = count as f64 / original_low_len.max(1) as f64 * 100.0;
        density >= Self::STALLED_PROMISE_DENSITY_THRESHOLD
    }

    /// Collapse runs of whitespace to a single space and lowercase, so a stall
    /// is still detected if streamed/degenerate output introduces extra spaces,
    /// tabs, or newlines inside a phrase ("let  me run", "let\tme run"). This
    /// mirrors inline_tail's whitespace flattening.
    fn flatten_whitespace(text: &str) -> String {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    }

    /// Count matched action-promise phrases, subtracting the "let me know"
    /// closing/sign-off phrasing which is not an action promise.
    fn count_action_promise_phrases(low: &str) -> usize {
        let phrases = [
            "let me",
            "i'll",
            "i will",
            "let's",
            "i am going to",
            "i'm going to",
        ];
        let mut count = 0usize;
        for p in phrases {
            count += low.matches(p).count();
        }
        // "let me know" is a closing/feedback phrasing (e.g. "let me know if you
        // want changes"), not a promise to perform an action the model then
        // fails to take. Subtracting it out of the "let me" count keeps the
        // heuristic focused on action-promise stalling and avoids flagging a
        // genuine final answer that merely asks for confirmation.
        count -= low.matches("let me know").count();
        count
    }

    /// Detect the *compact* unfulfilled-tool-request stall: a short assistant
    /// turn that explicitly says it will invoke/call a tool (bash, a command, a
    /// tool) "now" but ends with a normal stop and no tool call. Unlike the
    /// dense-rambling mode, such turns state a single, explicit future intent to
    /// act, so the presence of that explicit tool-invocation frame is the reliable
    /// signal rather than phrase density.
    ///
    /// This is deliberately strict to avoid flagging a genuine short final answer
    /// that happens to mention a tool in passing or in the past tense:
    ///  - The turn must be short (bounded ORIGINAL length, before fences are
    ///    stripped), so long legitimate prose that recounts an earlier
    ///    "I'll invoke..." does not match—even if it hides part of its length
    ///    inside a fenced code block.
    ///  - It must contain a first-person future/volitional invoke frame ("let me
    ///    invoke", "i'll invoke", "i will invoke", "i'm going to invoke", "i am
    ///    going to invoke", "let me call", "i'll call"). A bare
    ///    third-person/descriptive "will invoke" (e.g. "this setup will
    ///    invoke shell hooks") is NOT a promise by the agent to act, so it must
    ///    not match.
    ///  - The first-person verb must be immediately bound to an actual tool
    ///    target in the SAME phrase (e.g. "i'll invoke bash", "let me call the
    ///    bash tool"). This rejects a genuine answer like "I'll invoke the
    ///    policy; the release pipeline will run after CI", where the first-person
    ///    "i'll invoke" is abstract and the common word "run" belongs to a
    ///    different clause. It also rejects pairing a first-person abstract
    ///    "invoke" with a third-person tool description elsewhere ("I'll invoke
    ///    the policy. The harness will invoke the shell hook."). All observed
    ///    stalls name the tool right after the first-person verb, so this
    ///    same-clause coupling loses no coverage.
    ///
    /// NOTE: this uses EXACT multi-word phrase matching by design. On real
    /// archived sessions it yields zero false positives (only the true
    /// sabertooth stall); a looser structural "starter + verb + target" matcher
    /// was tried (round 9) but exploded to ~369 false positives on the same
    /// data because "let me"/"i'll" appear in ordinary turns. Exact matching is
    /// the empirically correct point on the precision/recall line for the
    /// observed degradation.
    fn is_compact_unfulfilled_tool_request(low: &str, original_low_len: usize) -> bool {
        // The length bound must use the ORIGINAL (un-stripped) turn length. A
        // genuinely long recount that buries most of its text inside a balanced
        // fenced code block would otherwise shrink below the bound after fence
        // stripping and be falsely flagged as a compact stall.
        if original_low_len > Self::COMPACT_STALLED_TOOL_REQUEST_MAX_LEN {
            return false;
        }
        // Detect a first-person, present-tense promise to invoke a tool. Only
        // these reliably signal that the AGENT intends to act right now; bare
        // non-first-person forms ("the harness will invoke bash now", "the cron
        // job invokes the tool") describe tooling or a dependency and are NOT a
        // promise by the agent, so they must not be treated as a stall. All real
        // observed stalls (giraffe 5/5, sabertooth) use a first-person frame, so
        // this tightening loses no coverage.
        //
        // The first-person verb must be IMMEDIATELY bound to a concrete
        // tool/command target (e.g. "i'll invoke bash", "let me call the bash
        // tool"). Coupling them into a single conjunct is essential: a check
        // that merely ANDs "a first-person frame somewhere" with "a tool word
        // somewhere" lets the two come from DIFFERENT clauses, falsely flagging
        // a genuine answer like "I'll invoke the policy. The harness will invoke
        // the shell hook on deploy." Here the first-person "i'll invoke" is
        // abstract and the "invoke the shell" is a third-person description.
        // Requiring them in the same phrase (verb then target) rejects that
        // false positive while keeping every observed stall, which always names
        // the tool right after the verb.
        const FIRST_PERSON_INVOKE: [&str; 7] = [
            "let me invoke",
            "i'll invoke",
            "i will invoke",
            "i'm going to invoke",
            "i am going to invoke",
            "let me call",
            "i'll call",
        ];
        const TOOL_TARGETS: [&str; 20] = [
            "bash",
            "the bash",
            "tool",
            "the tool",
            "a tool",
            "command",
            "the command",
            "a command",
            "grep",
            "sed",
            "run",
            "the run",
            "shell",
            "the shell",
            "a shell",
            "script",
            "the script",
            "a script",
            "cmd",
            "a cmd",
        ];
        // The detector runs on one short (<=700 char) turn per recovery check,
        // so building the few candidate phrases here is negligible.
        FIRST_PERSON_INVOKE.iter().any(|starter| {
            TOOL_TARGETS
                .iter()
                .any(|target| Self::contains_phrase_boundary(low, &format!("{starter} {target}")))
        })
    }

    /// True when `haystack` contains `needle` and the character immediately
    /// following it is a word boundary (space, a punctuation mark other than an
    /// apostrophe, or end-of-string). This rejects POSSESSIVE and word-joined
    /// forms of a tool target: "the tool's docs" or "the command_line" contain
    /// "the tool" / "the command" as substrings, but they are *references* to a
    /// tool, not a first-person commitment to invoke it now. Requiring a real
    /// boundary after the target keeps the compact stall detector from flagging
    /// genuine answers like "I'll call the tool's documentation when reviewing".
    fn contains_phrase_boundary(haystack: &str, needle: &str) -> bool {
        // Guard against an empty needle: find() on an empty string would match
        // at every index and never advance, looping forever. Callers always pass
        // a non-empty "{starter} {target}", but never loop on a logical invariant.
        if needle.is_empty() || haystack.is_empty() {
            return false;
        }
        let mut start = 0;
        // Iterate ALL occurrences: the FIRST match may have a poor boundary
        // (e.g. a possessive "the tool's"), while a LATER occurrence is a bare
        // invocation ("then i'll call the tool."). Return true if ANY occurrence
        // has a valid boundary so a real stall is never missed.
        while let Some(rel) = haystack[start..].find(needle) {
            let at = start + rel;
            let end = at + needle.len();
            let next = haystack.as_bytes().get(end);
            // Accept only end-of-string, whitespace, or clearly-sentence-final
            // punctuation. Reject anything that continues the word, which
            // signals a reference rather than a bare target: an apostrophe
            // (possessive, "tool's"), an alphanumeric (letter/digit, "tool2"),
            // or a join character ('_' or '-', "tool_x", "command-line").
            let ok = match next {
                None => true,
                Some(b) => matches!(
                    b,
                    b' ' | b'\t' | b'\n' | b'\r' | b'.' | b',' | b'!' | b'?' | b';' | b':' | b')'
                ),
            };
            if ok {
                return true;
            }
            start = end;
        }
        false
    }

    /// Request a single bounded continuation when the model stopped after
    /// promising an action it never performed. If it keeps stalling, we give
    /// up and surface the partial output rather than looping forever.
    pub(crate) fn maybe_continue_stalled_promise(
        &mut self,
        stop_reason: Option<&str>,
        text_content: &str,
        attempts: &mut u32,
    ) -> Result<bool> {
        // A tool_use stop with no parsed tool call is the stranded-tool-use
        // recovery's job (targeted message + its own budget). Don't preempt it
        // here with the generic "you promised an action" reminder, and don't
        // let two independent recovery counters both consume turns on the same
        // incident.
        if Self::is_stranded_tool_use_stop(stop_reason) {
            return Ok(false);
        }
        // Truncation/guardrail stops are owned by their own recovery paths
        // (maybe_continue_incomplete_response / the Fable reconsideration and
        // guardrail-notice handlers). This guard is specifically for a model
        // that *stopped normally* but stalled behind action-promise filler. It
        // must not steal a truncated or refused turn. Checking here (rather
        // than relying on loop ordering) keeps both loops consistent even if
        // the call order ever changes.
        if Self::is_guardrail_stop_reason(stop_reason) {
            return Ok(false);
        }
        if stop_reason.is_some_and(Self::should_continue_after_stop_reason) {
            return Ok(false);
        }
        if !Self::is_stalled_promise_text(text_content) {
            return Ok(false);
        }
        if *attempts >= Self::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS {
            logging::warn(&format!(
                "Assistant stalled behind action-promise filler after {} continuation attempts; surfacing partial output",
                attempts
            ));
            return Ok(false);
        }
        *attempts += 1;
        logging::warn(&format!(
            "Assistant stopped after promising an action without performing it (attempt {}/{}); requesting continuation",
            attempts,
            Self::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS
        ));
        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "<system-reminder>Your previous response said you would perform an action (e.g. \"Let me...\" or \"I'll invoke...\") but ended without making the tool call. If a further step is needed, emit the tool call now and continue the task instead of restating your intent. If the task is genuinely complete, give the final answer directly.</system-reminder>"
                    .to_string(),
                cache_control: None,
            }],
        );
        self.session.save()?;
        Ok(true)
    }

    /// True when the provider said it stopped to call a tool but no tool call
    /// survived parsing.
    ///
    /// `stop_reason: tool_use` with zero tool calls is a contradiction: the
    /// model intended to act and the harness has nothing to run. Breaking out
    /// of the turn there strands the agent mid-task, which on a benchmark run
    /// looks like an ordinary "the agent stopped early" failure and silently
    /// discards all of its uncommitted work. Treat it like any other
    /// incomplete response and ask for a continuation instead.
    ///
    /// Some providers spell this stop reason differently: Anthropic/Claude use
    /// `tool_use`, while OpenAI/OpenRouter (and DeepSeek via OpenRouter) use
    /// `tool_calls`. Both mean the model intended to call a tool, so both are
    /// stranded-tool stops when no tool call was parsed.
    pub(crate) fn is_stranded_tool_use_stop(stop_reason: Option<&str>) -> bool {
        stop_reason
            .map(str::trim)
            .map(|reason| {
                reason.eq_ignore_ascii_case("tool_use") || reason.eq_ignore_ascii_case("tool_calls")
            })
            .unwrap_or(false)
    }

    pub(crate) fn maybe_continue_stranded_tool_use(
        &mut self,
        stop_reason: Option<&str>,
        attempts: &mut u32,
    ) -> Result<bool> {
        if !Self::is_stranded_tool_use_stop(stop_reason) {
            return Ok(false);
        }
        if *attempts >= Self::MAX_INCOMPLETE_CONTINUATION_ATTEMPTS {
            logging::warn(&format!(
                "Provider reported stop_reason='tool_use' with no parsed tool call after {} continuation attempts; ending turn",
                attempts
            ));
            return Ok(false);
        }
        *attempts += 1;
        logging::warn(&format!(
            "Provider reported stop_reason='tool_use' but no tool call was parsed; requesting continuation (attempt {}/{})",
            attempts,
            Self::MAX_INCOMPLETE_CONTINUATION_ATTEMPTS
        ));
        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "[System reminder: your previous response ended with stop_reason \"tool_use\" but no tool call arrived. Nothing was executed. Re-issue the tool call you intended, do not repeat completed work, and continue the task.]"
                    .to_string(),
                cache_control: None,
            }],
        );
        self.session.save()?;
        Ok(true)
    }

    pub(super) fn filter_truncated_tool_calls(
        &mut self,
        stop_reason: Option<&str>,
        tool_calls: &mut Vec<ToolCall>,
        assistant_message_id: Option<&String>,
    ) {
        let stop_reason = stop_reason.unwrap_or("");
        if !Self::should_continue_after_stop_reason(stop_reason) {
            return;
        }

        let before = tool_calls.len();
        tool_calls.retain(|tc| !tc.input.is_null());
        let discarded = before - tool_calls.len();
        if discarded > 0 && tool_calls.is_empty() {
            logging::warn(&format!(
                "Discarded {} tool call(s) with null input (truncated by {}); requesting continuation",
                discarded,
                if stop_reason.is_empty() {
                    "unknown"
                } else {
                    stop_reason
                }
            ));
            if let Some(msg_id) = assistant_message_id {
                self.session.remove_tool_use_blocks(msg_id);
                self.persist_session_best_effort("truncated tool-call repair");
            }
        }
    }
}
