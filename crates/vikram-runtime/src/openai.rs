//! OpenAI-compatible chat completions with SSE streaming.
//!
//! The provider translates only. Provenance survives the translation: model text stays model
//! authored, tool results re-enter as tool messages, and the kernel still gates every call.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use vikram_contracts::{Block, Message, Role, VikramError};

use crate::config::Config;
use crate::error::Result;
use crate::provider::{CompletionRequest, Provider, ProviderEvent, ProviderStream};

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    context_window: Option<u64>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is deliberately omitted.
        f.debug_struct("OpenAiProvider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        if config.model.trim().is_empty() {
            return Ok(None);
        }
        let Some(api_key) = config.api_key() else {
            return Ok(None);
        };
        Ok(Some(Self::new(
            config.base_url.clone(),
            config.model.clone(),
            api_key,
            config.context_window,
        )))
    }

    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        context_window: Option<u64>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key: api_key.into(),
            context_window,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

/// The model identifiers an endpoint offers, from the body of a `GET {base_url}/models` reply.
/// Two shapes are read because the servers speaking this protocol do not agree on the key name:
/// OpenAI-style `{"data": [{"id": …}]}` and llama.cpp/Ollama-style `{"models": [{"name": …}]}`.
/// Anything else yields an empty list, which the caller reports as an unusable reply rather than
/// as an endpoint that offers nothing.
pub fn model_ids(body: &Value) -> Vec<String> {
    let entries = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array);
    let Some(entries) = entries else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The longest server-directed wait a turn will sit through. Beyond this the operator is better off
/// being told to wait than watching a silent clock, so nothing is retried.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);

/// Statuses a model endpoint asks us to try again on, rather than treat as a final refusal. Every
/// 5xx qualifies, because gateways disagree about which of them are permanent (529 overloaded, for
/// one) and a single retry costs less than a wrong failure.
fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
}

/// Exponential backoff with a ceiling: 0.5s, 1s, 2s, then 4s.
fn backoff(attempt: u32) -> Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1)).min(8);
    Duration::from_millis(500 * factor)
}

/// `Retry-After` when the server names a delay; capped so a hostile header cannot stall a turn.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let seconds = value.to_str().ok()?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds.min(300)))
}

/// Bodies that mean "this account cannot pay for a request", whatever status carries them. OpenAI
/// answers 429 with `insufficient_quota`, others answer 402 or 403.
fn credit_exhausted(body: &str) -> bool {
    let body = body.to_lowercase();
    [
        "insufficient_quota",
        "insufficient quota",
        "quota exceeded",
        "exceeded your current quota",
        "credit balance",
        "out of credit",
        "no credit",
        "billing",
        "payment required",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

/// Put a refused request into words the operator can act on. The raw provider body is kept, because
/// it is often the only place the request id and the exact reason appear, but it never leads.
fn explain_rejection(status: reqwest::StatusCode, body: &str, endpoint: &str) -> String {
    let detail = truncate(body, 240);
    if status.as_u16() == 402 || credit_exhausted(body) {
        return format!(
            "the account has no credit left for {endpoint} ({status}). Top up, or switch with \
             `vikram config set model <name>` (and `--base-url` for another provider). \
             Provider said: {detail}"
        );
    }
    match status.as_u16() {
        401 | 403 => format!(
            "credentials refused ({status}) by {endpoint}. Check the key named by `api_key_env` \
             in `vikram config show`. Provider said: {detail}"
        ),
        404 => format!(
            "no endpoint or model at ({status}) {endpoint}. Check `model` and `base_url`. \
             Provider said: {detail}"
        ),
        429 => format!(
            "rate limited ({status}) by {endpoint} after retries. Wait for the window to reset, \
             or switch model. Provider said: {detail}"
        ),
        500..=599 => format!(
            "provider unavailable ({status}) at {endpoint} after retries; this is their side, not \
             the request. Retry in a moment. Provider said: {detail}"
        ),
        _ => format!("{status} from {endpoint}. Provider said: {detail}"),
    }
}

/// The same for a failure that never reached a status line: the transport broke.
/// The error and everything that caused it, as text. `reqwest`'s own `Display` stops at "error
/// sending request", while the reason (a DNS lookup failure, a refused connect) lives in the chain.
fn error_chain(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn explain_transport(err: &reqwest::Error, endpoint: &str, attempts: u32) -> String {
    let text = error_chain(err).to_lowercase();
    if text.contains("dns") || text.contains("lookup address") {
        format!(
            "cannot resolve the host for {endpoint} after {attempts} attempts: DNS is not \
             answering, which is what a dropped network looks like."
        )
    } else if err.is_timeout() {
        format!("{endpoint} did not answer before the timeout, after {attempts} attempts")
    } else if err.is_connect() {
        format!(
            "cannot reach {endpoint}: the connection failed after {attempts} attempts. \
             The network or DNS is down, or the endpoint is wrong."
        )
    } else {
        format!("{endpoint} failed after {attempts} attempts: {err}")
    }
}

/// A transport error worth another attempt. A refusal by DNS or TLS will say the same thing every
/// time, and retrying it only makes the operator wait longer for the same message.
fn retryable_transport(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

/// Ask the configured endpoint what it serves. Kept separate from the provider object because the
/// listing is a property of the *server*, not of the model currently chosen.
pub async fn list_models(
    base_url: &str,
    api_key: &str,
) -> std::result::Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| err.to_string())?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let reply = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|err| format!("could not reach {url}: {err}"))?;
    let status = reply.status();
    let body: Value = reply
        .json()
        .await
        .map_err(|err| format!("{status} from {url}, and the body was not json: {err}"))?;
    if !status.is_success() {
        return Err(format!("{status} from {url}"));
    }
    let ids = model_ids(&body);
    if ids.is_empty() {
        return Err(format!("{url} returned no model list we can read"));
    }
    Ok(ids)
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai-compat"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<std::result::Result<ProviderEvent, VikramError>>(64);
        let client = self.client.clone();
        let endpoint = self.endpoint();
        let api_key = self.api_key.clone();
        let body = build_request_body(&request);

        tokio::spawn(async move {
            const MAX_ATTEMPTS: u32 = 3;
            let mut attempt = 0u32;
            let response = loop {
                attempt += 1;
                match client
                    .post(&endpoint)
                    .bearer_auth(&api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            break response;
                        }
                        // The body decides between "wait and retry" and "this account cannot pay":
                        // OpenAI folds quota exhaustion into 429, so the status alone lies.
                        let wait = retry_after(&response);
                        let body = response.text().await.unwrap_or_default();
                        let billing = status.as_u16() == 402 || credit_exhausted(&body);
                        if !billing && retryable_status(status) && attempt < MAX_ATTEMPTS {
                            match wait {
                                Some(delay) if delay > MAX_RETRY_WAIT => {
                                    let text = format!(
                                        "{endpoint} asked to wait {}s before retrying ({status}). \
                                         That would stall the turn, so nothing was retried: wait, \
                                         then send again.",
                                        delay.as_secs()
                                    );
                                    let _ = tx.send(Err(VikramError::ProviderRejected(text))).await;
                                    return;
                                }
                                other => {
                                    let delay = other.unwrap_or_else(|| backoff(attempt));
                                    tokio::time::sleep(delay).await;
                                    continue;
                                }
                            }
                        }
                        let text = explain_rejection(status, &body, &endpoint);
                        let _ = tx
                            .send(Err(if billing || status.as_u16() == 429 {
                                VikramError::ProviderRejected(text)
                            } else {
                                VikramError::ProviderTransient(text)
                            }))
                            .await;
                        return;
                    }
                    Err(err) => {
                        // A connection that failed before any bytes were sent is safe to retry:
                        // the request never reached the model, so it cannot run twice.
                        if attempt < MAX_ATTEMPTS && retryable_transport(&err) {
                            tokio::time::sleep(backoff(attempt)).await;
                            continue;
                        }
                        let text = explain_transport(&err, &endpoint, attempt);
                        let _ = tx.send(Err(VikramError::ProviderTransient(text))).await;
                        return;
                    }
                }
            };

            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut pending = stream_state();
            let mut finished = false;

            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        let _ = tx
                            .send(Err(VikramError::ProviderTransient(format!(
                                "stream interrupted: {err}"
                            ))))
                            .await;
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(index) = buffer.find('\n') {
                    let line = buffer[..index].trim_end_matches('\r').to_owned();
                    buffer.drain(..=index);
                    if let Some(events) = parse_sse_line(&line, &mut pending) {
                        if let Some(text) = pending.error.take() {
                            let _ = tx.send(Err(VikramError::ProviderRejected(text))).await;
                            return;
                        }
                        for event in events {
                            match event {
                                ProviderEvent::Finished { .. } => {
                                    if tx.send(Ok(event)).await.is_err() {
                                        return;
                                    }
                                    finished = true;
                                }
                                other => {
                                    if tx.send(Ok(other)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !finished {
                let mut sent_finish = false;
                for event in finish_pending(&mut pending) {
                    if matches!(event, ProviderEvent::Finished { .. }) {
                        sent_finish = true;
                    }
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                if !sent_finish {
                    let _ = tx
                        .send(Ok(ProviderEvent::Finished {
                            input_tokens: None,
                            output_tokens: None,
                        }))
                        .await;
                }
            }
        });

        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// OpenAI-compatible endpoints restrict function names to `[a-zA-Z0-9_-]`, while VIKRAM registers
/// dotted names. Underscore, dash and dot each get a two-character code so the mapping is
/// reversible and two different tools can never arrive at the same wire name.
pub fn wire_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '_' => out.push_str("__"),
            '-' => out.push_str("--"),
            '.' => out.push_str("-_"),
            other if is_endpoint_legal(other) => out.push(other),
            _ => out.push('_'),
        }
    }
    out
}

fn is_endpoint_legal(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

/// The inverse of [`wire_name`]. Characters the encoder never emits pass through untouched, so a
/// provider that echoes a plain dotted name still resolves.
pub fn from_wire_name(wire: &str) -> String {
    let chars: Vec<char> = wire.chars().collect();
    let mut out = String::with_capacity(wire.len());
    let mut i = 0;
    while i < chars.len() {
        let next = chars.get(i + 1).copied();
        match (chars[i], next) {
            ('_', Some('_')) => {
                out.push('_');
                i += 2;
            }
            ('-', Some('-')) => {
                out.push('-');
                i += 2;
            }
            ('-', Some('_')) => {
                out.push('.');
                i += 2;
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn build_request_body(request: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": map_messages(&request.messages),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(max) = request.max_output_tokens {
        body["max_tokens"] = json!(max);
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": wire_name(&tool.name),
                        "description": tool.description,
                        "parameters": tool.schema,
                    }
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

pub fn map_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        let mut images: Vec<&vikram_contracts::ImageSource> = Vec::new();

        for block in &message.blocks {
            match block {
                Block::Text { text: body, .. } => text.push_str(body),
                // Reasoning is model-authored scratchpad; do not re-send as instruction text.
                Block::Reasoning { .. } => {}
                Block::Image { source, .. } => images.push(source),
                Block::ToolCall { call } => tool_calls.push(json!({
                    "id": call.id.as_str(),
                    "type": "function",
                    "function": {
                        "name": wire_name(&call.name),
                        "arguments": call.arguments.to_string(),
                    }
                })),
                Block::ToolResult { call, content, .. } => tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": call.as_str(),
                    "content": content,
                })),
            }
        }

        match message.role {
            Role::Tool => out.extend(tool_results),
            Role::System | Role::User => {
                if message.role == Role::User && !images.is_empty() {
                    // Vision requests carry the OpenAI content-parts array; a plain string is
                    // still the right shape for text-only turns (and for every other role).
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    for image in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": image.wire_url()},
                        }));
                    }
                    out.push(json!({"role": "user", "content": parts}));
                } else {
                    out.push(json!({"role": role_name(message.role), "content": text}));
                }
                out.extend(tool_results);
            }
            Role::Assistant => {
                let mut value = json!({"role": "assistant"});
                if tool_calls.is_empty() {
                    value["content"] = json!(text);
                } else {
                    value["content"] = if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    };
                    value["tool_calls"] = json!(tool_calls);
                }
                out.push(value);
                out.extend(tool_results);
            }
        }
    }
    out
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Debug, Default, Clone)]
struct ToolAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// What the stream has accumulated so far. A usage chunk can arrive before `[DONE]`, so the
/// terminal `Finished` event waits for any tool call still being assembled: a consumer that stops
/// reading at `Finished` must never miss a call.
#[derive(Debug, Default)]
struct StreamState {
    calls: BTreeMap<usize, ToolAccumulator>,
    finish: Option<(Option<u64>, Option<u64>)>,
    /// An error the provider reported *inside* a 200 response. OpenAI-compatible endpoints do this
    /// once the headers are out: the failure is in the body, and ignoring it turns a broken request
    /// into an empty answer.
    error: Option<String>,
}

fn stream_state() -> StreamState {
    StreamState::default()
}

/// Read a provider's own error object into one sentence, keeping the code when it names one.
fn summarize_stream_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (message.is_empty(), code.is_empty()) {
        (false, true) => format!("the provider reported an error mid-stream: {message}"),
        (false, false) => {
            format!("the provider reported an error mid-stream ({code}): {message}")
        }
        _ => format!(
            "the provider reported an error mid-stream: {}",
            truncate(&error.to_string(), 240)
        ),
    }
}

/// Parse one SSE line into zero or more provider events.
fn parse_sse_line(line: &str, state: &mut StreamState) -> Option<Vec<ProviderEvent>> {
    let data = line.strip_prefix("data:")?;
    let data = data.trim();
    if data.is_empty() {
        return None;
    }
    if data == "[DONE]" {
        return Some(finish_pending(state));
    }
    let value: Value = serde_json::from_str(data).ok()?;

    // A frame can carry a failure instead of a delta. Read it before anything else, or the stream
    // ends with a synthetic finish and the turn reports success with no words in it.
    if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
        state.error = Some(summarize_stream_error(error));
        return Some(Vec::new());
    }
    let finish_reason = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str);
    if finish_reason == Some("error") {
        state.error = Some(
            "the provider ended the stream with finish_reason `error` and no message".to_owned(),
        );
        return Some(Vec::new());
    }

    if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
        let input = usage.get("prompt_tokens").and_then(Value::as_u64);
        let output = usage.get("completion_tokens").and_then(Value::as_u64);
        if input.is_some() || output.is_some() {
            if !state.calls.is_empty() {
                state.finish = Some((input, output));
                return None;
            }
            return Some(vec![ProviderEvent::Finished {
                input_tokens: input,
                output_tokens: output,
            }]);
        }
    }

    let delta = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))?;

    let mut events = Vec::new();
    // Reasoning/thinking deltas (provider-dependent field names).
    for field in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(thought) = delta.get(field).and_then(Value::as_str)
            && !thought.is_empty()
        {
            events.push(ProviderEvent::ThinkingDelta(thought.to_owned()));
        }
    }
    if let Some(content) = delta.get("content").and_then(Value::as_str)
        && !content.is_empty()
    {
        events.push(ProviderEvent::TextDelta(content.to_owned()));
    }

    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for (fallback_index, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as usize)
                .unwrap_or(fallback_index);
            let entry = state.calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                entry.id = Some(id.to_owned());
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    entry.name = Some(from_wire_name(name));
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    entry.arguments.push_str(arguments);
                }
            }
        }
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

/// Flush what the stream holds: assembled tool calls first, then a `Finished` that was waiting
/// for them.
fn finish_pending(state: &mut StreamState) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    for (_, entry) in std::mem::take(&mut state.calls) {
        let Some(name) = entry.name else { continue };
        let arguments = match serde_json::from_str::<Value>(&entry.arguments) {
            Ok(value) => value,
            Err(_) => {
                events.push(ProviderEvent::TextDelta(format!(
                    "\n[tool call `{name}` had invalid JSON arguments]"
                )));
                continue;
            }
        };
        let mut call = vikram_contracts::ToolCall::new(name, arguments);
        if let Some(id) = entry.id {
            call.id = vikram_contracts::ToolCallId::new(id);
        }
        events.push(ProviderEvent::ToolCall(call));
    }
    if let Some((input, output)) = state.finish.take() {
        events.push(ProviderEvent::Finished {
            input_tokens: input,
            output_tokens: output,
        });
    }
    events
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .take_while(|(i, _)| *i < limit)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &text[..cut])
}

impl From<crate::error::RuntimeError> for VikramError {
    fn from(err: crate::error::RuntimeError) -> Self {
        VikramError::ProviderRejected(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;
    use vikram_contracts::{ToolCall, ToolCallId, Trust};

    #[test]
    fn a_payment_refusal_is_explained_as_no_credit_not_as_raw_json() {
        let status = reqwest::StatusCode::from_u16(402).unwrap();
        let body = r#"{"error":{"message":"payment required","type":"insufficient_quota"}}"#;
        let text = explain_rejection(status, body, "https://api.example/v1");
        assert!(text.contains("no credit left"), "{text}");
        assert!(text.contains("vikram config set model"), "{text}");
        assert!(!text.starts_with("402"), "the status must not lead: {text}");
        // OpenAI reports the same condition as 429 with a quota body.
        let limited = explain_rejection(
            reqwest::StatusCode::from_u16(429).unwrap(),
            r#"{"error":{"code":"insufficient_quota"}}"#,
            "https://api.example/v1",
        );
        assert!(limited.contains("no credit left"), "{limited}");
    }

    #[test]
    fn a_refused_key_names_the_setting_to_look_at() {
        let text = explain_rejection(
            reqwest::StatusCode::from_u16(401).unwrap(),
            "invalid api key",
            "https://api.example/v1",
        );
        assert!(text.contains("api_key_env"), "{text}");
        assert!(text.contains("401"), "{text}");
    }

    #[test]
    fn a_missing_model_names_the_address_and_the_setting() {
        let text = explain_rejection(
            reqwest::StatusCode::from_u16(404).unwrap(),
            "",
            "https://api.example/v1",
        );
        assert!(
            text.contains("model") && text.contains("base_url"),
            "{text}"
        );
    }

    #[test]
    fn a_server_error_is_attributed_to_the_provider_side() {
        let text = explain_rejection(
            reqwest::StatusCode::from_u16(503).unwrap(),
            "upstream busy",
            "https://api.example/v1",
        );
        assert!(text.contains("their side"), "{text}");
    }

    #[test]
    fn a_rate_limit_says_to_wait_and_keeps_the_body_bounded() {
        let long = "x".repeat(4_000);
        let text = explain_rejection(
            reqwest::StatusCode::from_u16(429).unwrap(),
            &long,
            "https://api.example/v1",
        );
        assert!(text.contains("rate limited"), "{text}");
        assert!(
            text.len() < 700,
            "body was not truncated: {} chars",
            text.len()
        );
    }

    #[test]
    fn every_server_error_is_retried_but_a_payment_refusal_is_not() {
        for code in [408, 425, 429, 500, 501, 502, 503, 504, 529] {
            assert!(
                retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should be retried"
            );
        }
        for code in [200, 400, 401, 402, 403, 404, 422] {
            assert!(
                !retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} must not be retried"
            );
        }
    }

    #[test]
    fn an_error_frame_inside_a_successful_stream_is_captured_not_dropped() {
        let mut state = stream_state();
        let events = parse_sse_line(
            r#"data: {"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
            &mut state,
        );
        assert_eq!(events.map(|events| events.len()), Some(0));
        let text = state.error.expect("the failure was captured");
        assert!(text.contains("insufficient_quota"), "{text}");
        assert!(text.contains("exceeded your current quota"), "{text}");
    }

    #[test]
    fn a_finish_reason_of_error_is_not_read_as_a_finished_answer() {
        let mut state = stream_state();
        parse_sse_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"error"}]}"#,
            &mut state,
        );
        assert!(state.error.is_some(), "silence must not look like success");
    }

    #[tokio::test]
    async fn a_host_that_cannot_be_resolved_is_named_as_dns() {
        let endpoint = "http://no-such-host-xyz.invalid/v1/chat/completions";
        let err = reqwest::Client::new()
            .get(endpoint)
            .send()
            .await
            .expect_err("a reserved TLD cannot resolve");
        let text = explain_transport(&err, endpoint, 3);
        assert!(text.contains("DNS"), "the cause must be named: {text}");
        assert!(text.contains(endpoint), "{text}");
    }

    #[test]
    fn backoff_grows_then_stops_growing() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(2000));
        assert_eq!(backoff(9), Duration::from_millis(4000));
    }

    #[test]
    fn a_dotted_function_name_is_escaped_for_endpoints_that_reject_dots() {
        assert_eq!(wire_name("fs.read"), "fs-_read");
        assert_eq!(wire_name("mcp.fs.read"), "mcp-_fs-_read");
        assert!(!wire_name("fs.read").contains('.'));
    }

    #[test]
    fn an_escaped_name_decodes_back_to_the_registered_tool() {
        for name in [
            "fs.read",
            "mcp.fs.read",
            "memory.save",
            "shell_run",
            "team-a.b",
        ] {
            assert_eq!(from_wire_name(&wire_name(name)), name);
        }
    }

    #[test]
    fn names_that_differ_only_by_their_separators_do_not_collide() {
        assert_ne!(wire_name("a.b"), wire_name("a_b"));
        assert_ne!(wire_name("a.b"), wire_name("a-_b"));
        assert_ne!(wire_name("a-b"), wire_name("a-_b"));
    }

    #[test]
    fn the_request_body_sends_only_endpoint_legal_function_names() {
        let request = CompletionRequest::new(
            "m",
            vec![Message::user("hi")],
            vec![ToolSpec {
                name: "fs.read".into(),
                description: "read a file".into(),
                schema: json!({"type": "object"}),
            }],
        );
        let body = build_request_body(&request);
        assert_eq!(body["tools"][0]["function"]["name"], "fs-_read");
    }

    #[test]
    fn an_escaped_tool_call_in_history_keeps_the_same_name_the_model_was_given() {
        let mapped = map_messages(&[Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                call: ToolCall::new("fs.read", json!({"path": "a"})),
            }],
        }]);
        assert_eq!(mapped[0]["tool_calls"][0]["function"]["name"], "fs-_read");
    }

    #[test]
    fn a_streamed_function_name_is_decoded_before_the_kernel_sees_it() {
        let mut pending = stream_state();
        parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs-_read","arguments":"{}"}}]}}]}"#,
            &mut pending,
        );
        let events = parse_sse_line("data: [DONE]", &mut pending).unwrap();
        match &events[0] {
            ProviderEvent::ToolCall(call) => assert_eq!(call.name, "fs.read"),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn a_usage_chunk_before_done_cannot_swallow_a_tool_call() {
        let mut pending = stream_state();
        parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs-_read","arguments":"{}"}}]}}]}"#,
            &mut pending,
        );
        let events = parse_sse_line(
            r#"data: {"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#,
            &mut pending,
        )
        .unwrap_or_default();
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, ProviderEvent::Finished { .. })),
            "Finished must wait for the unflushed tool call, got {events:?}"
        );

        let events = parse_sse_line("data: [DONE]", &mut pending).unwrap();
        assert!(
            matches!(events[0], ProviderEvent::ToolCall(_)),
            "got {events:?}"
        );
        assert_eq!(
            events[1],
            ProviderEvent::Finished {
                input_tokens: Some(9),
                output_tokens: Some(4)
            }
        );
    }

    #[test]
    fn a_text_delta_is_parsed_from_one_sse_line() {
        let mut pending = stream_state();
        let events = parse_sse_line(
            r#"data: {"choices":[{"delta":{"content":"hel"}}]}"#,
            &mut pending,
        )
        .unwrap();
        assert_eq!(events, vec![ProviderEvent::TextDelta("hel".into())]);
    }

    #[test]
    fn tool_call_fragments_accumulate_across_lines_and_emit_once() {
        let mut pending = stream_state();
        parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs.read","arguments":"{\"pa"}}]}}]}"#,
            &mut pending,
        );
        parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}"#,
            &mut pending,
        );
        let events = parse_sse_line("data: [DONE]", &mut pending).unwrap();
        match &events[0] {
            ProviderEvent::ToolCall(call) => {
                assert_eq!(call.name, "fs.read");
                assert_eq!(call.arguments, json!({"path": "a"}));
                assert_eq!(call.id.as_str(), "call_1");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn images_become_content_parts_only_for_user_turns() {
        let message = Message::user_with_images(
            "describe",
            vec![vikram_contracts::ImageSource::url("https://x/y.png")],
        );
        let mapped = map_messages(&[message]);
        let content = mapped[0]["content"].as_array().expect("content parts");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "https://x/y.png");

        let plain = map_messages(&[Message::user("plain")]);
        assert_eq!(plain[0]["content"], "plain");

        let mut system = Message::system("sys");
        system.blocks.push(Block::Image {
            source: vikram_contracts::ImageSource::url("https://x"),
            trust: Trust::External,
        });
        let mapped = map_messages(&[system]);
        assert_eq!(mapped[0]["content"], "sys");
    }

    #[test]
    fn usage_produces_a_finished_event_without_inventing_counts() {
        let mut pending = stream_state();
        let events = parse_sse_line(
            r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#,
            &mut pending,
        )
        .unwrap();
        assert_eq!(
            events,
            vec![ProviderEvent::Finished {
                input_tokens: Some(12),
                output_tokens: Some(3)
            }]
        );
    }

    #[test]
    fn messages_map_tool_calls_and_results_to_the_wire_format() {
        let call = ToolCall::new("fs.read", json!({"path": "a"}));
        let assistant = Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall { call: call.clone() }],
        };
        let result = Message {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                call: call.id.clone(),
                content: "ok".into(),
                trust: Trust::External,
                ok: true,
            }],
        };
        let mapped = map_messages(&[Message::system("s"), Message::user("u"), assistant, result]);
        assert_eq!(mapped[0]["role"], "system");
        assert_eq!(mapped[1]["content"], "u");
        assert_eq!(mapped[2]["content"], Value::Null);
        assert_eq!(mapped[2]["tool_calls"][0]["id"], call.id.as_str());
        assert_eq!(mapped[3]["role"], "tool");
        assert_eq!(mapped[3]["tool_call_id"], call.id.as_str());
    }

    #[test]
    fn a_provider_without_a_key_is_not_constructed() {
        let config = Config {
            model: "m".into(),
            api_key_env: format!("VIKRAM_ABSENT_KEY_{}", std::process::id()),
            ..Config::default()
        };
        assert!(OpenAiProvider::from_config(&config).unwrap().is_none());
    }

    #[test]
    fn a_listing_is_read_from_both_shapes_the_servers_use() {
        let openai = serde_json::json!({"object": "list", "data": [
            {"id": "gpt-5.2", "object": "model"},
            {"id": "qwen3-coder", "object": "model"},
            {"id": "gpt-5.2", "object": "model"}
        ]});
        assert_eq!(
            model_ids(&openai),
            vec!["gpt-5.2".to_owned(), "qwen3-coder".to_owned()],
            "duplicates collapse and the order is stable"
        );

        let local = serde_json::json!({"models": [
            {"name": "llama3.1:8b"},
            {"name": "  "},
            {"name": "qwen2.5-coder:32b"}
        ]});
        assert_eq!(
            model_ids(&local),
            vec!["llama3.1:8b".to_owned(), "qwen2.5-coder:32b".to_owned()],
            "a local server answers under `models[].name`, and a blank name is not a model"
        );

        assert!(
            model_ids(&serde_json::json!({"error": "unauthorized"})).is_empty(),
            "a reply that lists nothing reads as nothing, not as a guessed default"
        );
    }

    #[test]
    fn the_endpoint_is_built_without_a_double_slash() {
        let provider = OpenAiProvider::new("https://x/v1/", "m", "k", None);
        assert_eq!(provider.endpoint(), "https://x/v1/chat/completions");
        assert_eq!(provider.context_window(), None);
    }

    #[test]
    fn invalid_tool_arguments_surface_instead_of_panicking() {
        let mut pending = stream_state();
        pending.calls.insert(
            0,
            ToolAccumulator {
                id: Some("c".into()),
                name: Some("fs.read".into()),
                arguments: "{not json".into(),
            },
        );
        let events = finish_pending(&mut pending);
        assert!(matches!(events[0], ProviderEvent::TextDelta(_)));
    }

    #[test]
    fn tool_call_ids_from_the_provider_are_preserved() {
        let mut pending = stream_state();
        parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"abc","function":{"name":"fs.list","arguments":"{}"}}]}}]}"#,
            &mut pending,
        );
        let events = parse_sse_line("data: [DONE]", &mut pending).unwrap();
        match &events[0] {
            ProviderEvent::ToolCall(call) => assert_eq!(call.id, ToolCallId::new("abc")),
            other => panic!("{other:?}"),
        }
    }
}
