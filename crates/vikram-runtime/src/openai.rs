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
            let response = match client
                .post(&endpoint)
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    let _ = tx
                        .send(Err(VikramError::ProviderTransient(format!(
                            "request failed: {err}"
                        ))))
                        .await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let _ = tx
                    .send(Err(VikramError::ProviderRejected(format!(
                        "{status}: {}",
                        truncate(&body, 400)
                    ))))
                    .await;
                return;
            }

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

        for block in &message.blocks {
            match block {
                Block::Text { text: body, .. } => text.push_str(body),
                // Reasoning is model-authored scratchpad; do not re-send as instruction text.
                Block::Reasoning { .. } => {}
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
                out.push(json!({"role": role_name(message.role), "content": text}));
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
}

fn stream_state() -> StreamState {
    StreamState::default()
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
        if let Some(thought) = delta.get(field).and_then(Value::as_str) {
            if !thought.is_empty() {
                events.push(ProviderEvent::ThinkingDelta(thought.to_owned()));
            }
        }
    }
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            events.push(ProviderEvent::TextDelta(content.to_owned()));
        }
    }

    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for (fallback_index, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as usize)
                .unwrap_or(fallback_index);
            let entry = state.calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    entry.id = Some(id.to_owned());
                }
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    if !name.is_empty() {
                        entry.name = Some(from_wire_name(name));
                    }
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
