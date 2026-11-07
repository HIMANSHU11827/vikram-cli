//! Anthropic Messages API provider with SSE streaming.
//!
//! Speaks the native Messages format (`/v1/messages`) instead of the OpenAI
//! `/chat/completions` shape, so a Vikram configured with `provider = "anthropic"`
//! can talk to Claude directly. Tool use and (when the model emits them) thinking
//! blocks are translated into the same `ProviderEvent`s the OpenAI provider emits.

use serde_json::{Value, json};
use std::time::Duration;
use vikram_contracts::{Block, Role, ToolCall, VikramError};

use crate::config::Config;
use crate::error::Result;
use crate::provider::{CompletionRequest, Provider, ProviderEvent, ProviderStream};

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    context_window: Option<u64>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        if config.model.trim().is_empty() {
            return Ok(None);
        }
        let Some(api_key) = config.api_key() else {
            return Ok(None);
        };
        Ok(Some(Self::new(
            if config.base_url.trim().is_empty() {
                "https://api.anthropic.com".to_string()
            } else {
                config.base_url.clone()
            },
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
        format!("{}/v1/messages", self.base_url)
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    let _ = tx
                        .send(Err(VikramError::ProviderTransient(format!(
                            "cannot reach {endpoint}: {err}"
                        ))))
                        .await;
                    return;
                }
            };
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let _ = tx.send(Err(rejection(status, &body, &endpoint))).await;
                return;
            }

            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut pending = StreamState::default();
            let mut finished = false;

            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
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
                for event in finish_pending(&mut pending) {
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                let _ = tx
                    .send(Ok(ProviderEvent::Finished {
                        input_tokens: None,
                        output_tokens: None,
                    }))
                    .await;
            }
        });

        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

#[derive(Default)]
struct StreamState {
    /// Tool use currently being streamed: (id, name, accumulated input json).
    tool: Option<(String, String, String)>,
    /// Whether we are inside a text block.
    in_text: bool,
    error: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn parse_sse_line(line: &str, pending: &mut StreamState) -> Option<Vec<ProviderEvent>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // Anthropic SSE: "event: <type>" then "data: <json>".
    if let Some(rest) = line.strip_prefix("event:") {
        // We don't need to track event type separately; data carries a "type" field.
        let _ = rest;
        return None;
    }
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(vec![]);
    }
    let value: Value = serde_json::from_str(data).ok()?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match kind {
        "message_start" => {
            if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                pending.input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
            }
            Some(vec![])
        }
        "content_block_start" => {
            let block = value.get("content_block")?;
            let btype = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match btype {
                "text" => {
                    pending.in_text = true;
                    Some(vec![])
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    pending.tool = Some((id, name, String::new()));
                    Some(vec![])
                }
                "thinking" => Some(vec![]),
                _ => Some(vec![]),
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta")?;
            let dtype = delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match dtype {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.is_empty() {
                        Some(vec![])
                    } else {
                        Some(vec![ProviderEvent::TextDelta(text.to_owned())])
                    }
                }
                "input_json_delta" => {
                    if let Some(tool) = pending.tool.as_mut()
                        && let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                    {
                        tool.2.push_str(partial);
                    }
                    Some(vec![])
                }
                "thinking_delta" => {
                    let text = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.is_empty() {
                        Some(vec![])
                    } else {
                        Some(vec![ProviderEvent::ThinkingDelta(text.to_owned())])
                    }
                }
                _ => Some(vec![]),
            }
        }
        "content_block_stop" => {
            if let Some((id, name, input)) = pending.tool.take() {
                let arguments: Value = if input.trim().is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&input).unwrap_or(Value::Null)
                };
                let call = ToolCall {
                    id: vikram_contracts::ToolCallId::new(id),
                    name: crate::openai::from_wire_name(&name),
                    arguments,
                };
                pending.in_text = false;
                return Some(vec![ProviderEvent::ToolCall(call)]);
            }
            pending.in_text = false;
            Some(vec![])
        }
        "message_delta" => {
            if let Some(usage) = value.get("usage") {
                pending.output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
            }
            Some(vec![])
        }
        "message_stop" => Some(vec![ProviderEvent::Finished {
            input_tokens: pending.input_tokens,
            output_tokens: pending.output_tokens,
        }]),
        "error" => {
            let msg = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_owned();
            pending.error = Some(msg);
            Some(vec![])
        }
        _ => Some(vec![]),
    }
}

fn finish_pending(pending: &mut StreamState) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    if let Some((id, name, input)) = pending.tool.take() {
        let arguments: Value = if input.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&input).unwrap_or(Value::Null)
        };
        out.push(ProviderEvent::ToolCall(ToolCall {
            id: vikram_contracts::ToolCallId::new(id),
            name: crate::openai::from_wire_name(&name),
            arguments,
        }));
    }
    out.push(ProviderEvent::Finished {
        input_tokens: pending.input_tokens,
        output_tokens: pending.output_tokens,
    });
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Bodies that mean "this account cannot pay for a request", whatever status carries them.
/// Anthropic reports it as a 400 whose message names the credit balance, so the status alone
/// would read as a bad request and the turn would be retried forever.
fn credit_exhausted(body: &str) -> bool {
    let body = body.to_lowercase();
    [
        "insufficient_quota",
        "insufficient quota",
        "quota exceeded",
        "credit balance",
        "out of credit",
        "billing",
        "payment required",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

/// Which failure a non-2xx answer is. The classification is deliberately the one `OpenAiProvider`
/// makes, so a status means the same thing to the kernel whichever provider answered: a rate limit
/// or a payment refusal ends the turn, and everything else is the provider's side and may be tried
/// again without touching the transcript.
fn rejection(status: reqwest::StatusCode, body: &str, endpoint: &str) -> VikramError {
    let text = format!("{status} from {endpoint}: {}", truncate(body, 240));
    let final_refusal = matches!(status.as_u16(), 402 | 429) || credit_exhausted(body);
    if final_refusal {
        VikramError::ProviderRejected(text)
    } else {
        VikramError::ProviderTransient(text)
    }
}

fn build_request_body(request: &CompletionRequest) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();

    // Merge consecutive same-role messages (Anthropic requires alternation).
    for message in &request.messages {
        let role_str = match message.role {
            Role::System => {
                // Accumulate system text.
                for block in &message.blocks {
                    if let Block::Text { text, .. } = block {
                        if !system.is_empty() {
                            system.push('\n');
                        }
                        system.push_str(text);
                    }
                }
                continue;
            }
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };

        // Build content array for this message.
        let mut content: Vec<Value> = Vec::new();
        for block in &message.blocks {
            match block {
                Block::Text { text, .. } => {
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                    }
                }
                Block::Image { source, .. } => {
                    // Anthropic accepts base64 or url image sources.
                    match source {
                        vikram_contracts::ImageSource::Url { url } => {
                            content.push(
                                json!({"type": "image", "source": {"type": "url", "url": url}}),
                            );
                        }
                        vikram_contracts::ImageSource::Data { media_type, data } => {
                            content.push(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}));
                        }
                    }
                }
                Block::ToolCall { call } => {
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id.as_str(),
                        "name": crate::openai::wire_name(&call.name),
                        "input": call.arguments,
                    }));
                }
                Block::ToolResult {
                    call, content: c, ..
                } => {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": call.as_str(),
                        "content": c,
                    }));
                }
                Block::Reasoning { .. } => {}
            }
        }

        // Role::Tool messages in Vikram carry tool results as a user turn.
        let role = if matches!(message.role, Role::Tool) {
            "user"
        } else {
            role_str
        };

        if content.is_empty() {
            continue;
        }

        // Merge with previous if same role.
        if let Some(last) = messages.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut)
        {
            arr.extend(content);
            continue;
        }
        messages.push(json!({"role": role, "content": content}));
    }

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_output_tokens.unwrap_or(4096),
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": crate::openai::wire_name(&tool.name),
                    "description": tool.description,
                    "input_schema": tool.schema,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;
    use vikram_contracts::{ImageSource, Message, ToolCallId, Trust};

    const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
    const MODEL: &str = "claude-sonnet-4-5";

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest::new(MODEL, messages, Vec::new())
    }

    fn tool_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "read a file".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }
    }

    /// Feed SSE lines through the same parser `stream()` uses, keeping whatever it left in
    /// `pending`: an error, a half-assembled tool call, the usage counters.
    fn feed(pending: &mut StreamState, lines: &[&str]) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        for line in lines {
            if let Some(mut parsed) = parse_sse_line(line, pending) {
                events.append(&mut parsed);
            }
        }
        events
    }

    // ---- request body mapping -------------------------------------------------------------

    #[test]
    fn the_system_prompt_travels_in_the_top_level_system_field() {
        let mut request = request(vec![Message::system("be brief"), Message::user("hi")]);
        request.max_output_tokens = Some(1024);

        let body = build_request_body(&request);

        assert_eq!(body["model"], json!(MODEL));
        assert_eq!(body["system"], json!("be brief"));
        assert_eq!(body["max_tokens"], json!(1024));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]),
            "the system prompt must not appear as a message: {body}"
        );
    }

    #[test]
    fn an_absent_system_prompt_is_left_out_and_max_tokens_is_always_present() {
        let body = build_request_body(&request(vec![Message::user("hi")]));

        assert!(
            body.get("system").is_none(),
            "there is nothing to send, so no field at all: {body}"
        );
        assert_eq!(
            body["max_tokens"],
            json!(4096),
            "the Messages API requires max_tokens, so it can never be absent"
        );
    }

    #[test]
    fn consecutive_same_role_turns_merge_so_the_alternation_the_api_requires_holds() {
        let body = build_request_body(&request(vec![
            Message::user("one"),
            Message::user("two"),
            Message::text(Role::Assistant, "three", Trust::Model),
            Message::user("four"),
        ]));

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two"},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "three"}]},
                {"role": "user", "content": [{"type": "text", "text": "four"}]},
            ]),
            "two user turns in a row are one message, not two"
        );
    }

    #[test]
    fn a_tool_call_and_its_result_become_tool_use_and_tool_result_blocks() {
        let call = ToolCall::new("fs.read", json!({"path": "a"}));
        let body = build_request_body(&request(vec![
            Message::user("read a"),
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall { call: call.clone() }],
            },
            Message::tool_result(call.id.clone(), "file body", true),
            Message::user("thanks"),
        ]));

        let messages = body["messages"].as_array().expect("a message list");
        assert_eq!(
            messages.len(),
            3,
            "a tool result is a user turn and merges with the user turn after it: {messages:?}"
        );
        assert_eq!(
            messages[1]["content"],
            json!([{
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": "fs-_read",
                "input": {"path": "a"},
            }]),
            "the tool call keeps the id the API issued and the name the model was given"
        );
        assert_eq!(messages[2]["role"], json!("user"));
        assert_eq!(
            messages[2]["content"],
            json!([
                {"type": "tool_result", "tool_use_id": call.id.as_str(), "content": "file body"},
                {"type": "text", "text": "thanks"},
            ])
        );
    }

    #[test]
    fn tool_definitions_use_input_schema_and_endpoint_legal_names() {
        let mut request = request(vec![Message::user("hi")]);
        request.tools = vec![tool_spec("fs.read")];

        let body = build_request_body(&request);

        assert_eq!(
            body["tools"],
            json!([{
                "name": "fs-_read",
                "description": "read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            }]),
            "the Messages API puts the schema under `input_schema`, with no `function` wrapper"
        );
        assert!(body["tools"][0].get("function").is_none(), "{body}");
    }

    #[test]
    fn a_request_without_tools_carries_no_tools_field() {
        let body = build_request_body(&request(vec![Message::user("hi")]));

        assert!(body.get("tools").is_none(), "{body}");
    }

    #[test]
    fn images_use_the_anthropic_source_shapes() {
        let body = build_request_body(&request(vec![Message::user_with_images(
            "describe",
            vec![
                ImageSource::url("https://x/y.png"),
                ImageSource::data("image/jpeg", "AAAA"),
            ],
        )]));

        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "describe"},
                {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAAA"}},
            ])
        );
    }

    #[test]
    fn empty_text_and_reasoning_blocks_never_reach_the_wire() {
        let body = build_request_body(&request(vec![
            Message::user("real"),
            Message::text(Role::User, "", Trust::User),
            Message {
                role: Role::Assistant,
                blocks: vec![Block::Reasoning {
                    text: "scratchpad".into(),
                    trust: Trust::Model,
                }],
            },
        ]));

        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "real"}]}]),
            "an empty turn carries nothing, and reasoning is not re-sent as instruction text"
        );
    }

    #[test]
    fn the_body_carries_exactly_the_messages_api_fields_and_invents_no_sampling_knobs() {
        let mut request = request(vec![Message::system("s"), Message::user("u")]);
        request.tools = vec![tool_spec("fs.read")];

        let body = build_request_body(&request);

        let mut keys: Vec<&str> = body
            .as_object()
            .expect("a json object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "system",
                "tools"
            ],
            "`CompletionRequest` carries no temperature, stop sequences or thinking config, and the \
             provider must not invent defaults for them"
        );
    }

    // ---- SSE translation ------------------------------------------------------------------

    #[test]
    fn a_text_answer_streams_as_deltas_and_finishes_with_the_reported_usage() {
        let mut pending = StreamState::default();
        let events = feed(
            &mut pending,
            &[
                "event: message_start",
                r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":25,"output_tokens":1}}}"#,
                "",
                "event: content_block_start",
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                "event: content_block_delta",
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                "event: content_block_stop",
                r#"data: {"type":"content_block_stop","index":0}"#,
                "event: message_delta",
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}"#,
                "event: message_stop",
                r#"data: {"type":"message_stop"}"#,
                "",
            ],
        );

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("Hel".into()),
                ProviderEvent::TextDelta("lo".into()),
                ProviderEvent::Finished {
                    input_tokens: Some(25),
                    output_tokens: Some(7),
                },
            ]
        );
        assert!(pending.error.is_none(), "no failure was reported");
    }

    #[test]
    fn a_streamed_tool_use_assembles_its_partial_json_into_one_call() {
        let mut pending = StreamState::default();
        let events = feed(
            &mut pending,
            &[
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"fs-_read","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}"#,
                r#"data: {"type":"content_block_stop","index":1}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        );

        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            ProviderEvent::ToolCall(call) => {
                assert_eq!(call.id, ToolCallId::new("toolu_01"));
                assert_eq!(
                    call.name, "fs.read",
                    "the wire name is decoded before the kernel sees it"
                );
                assert_eq!(call.arguments, json!({"path": "a"}));
            }
            other => panic!("expected one assembled tool call, got {other:?}"),
        }
        assert_eq!(
            events[1],
            ProviderEvent::Finished {
                input_tokens: None,
                output_tokens: None
            }
        );
        assert!(pending.tool.is_none(), "the call is not emitted twice");
    }

    #[test]
    fn thinking_blocks_surface_as_thinking_deltas_and_never_as_text() {
        let mut pending = StreamState::default();
        let events = feed(
            &mut pending,
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}"#,
            ],
        );

        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("weighing".into()),
                ProviderEvent::TextDelta("done".into()),
            ],
            "an empty thought and a signature are not events; thinking never becomes answer text"
        );
    }

    #[test]
    fn a_frame_split_across_two_chunks_is_not_parsed_until_it_is_whole() {
        let mut pending = StreamState::default();
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let (head, tail) = line.split_at(30);

        assert_eq!(
            parse_sse_line(head, &mut pending),
            None,
            "half an event must not be read as one"
        );
        // `stream()` joins the pieces in its buffer before parsing; the whole line then parses.
        assert_eq!(
            parse_sse_line(&format!("{head}{tail}"), &mut pending),
            Some(vec![ProviderEvent::TextDelta("hi".into())])
        );
    }

    #[test]
    fn malformed_and_unknown_frames_are_skipped_without_panicking() {
        let mut pending = StreamState::default();
        for line in [
            "data: not json at all",
            r#"data: {"type":"crystal_ball","index":0}"#,
            r#"data: {"type":"content_block_delta"}"#,
            r#"data: {"type":"content_block_start"}"#,
            r#"data: {"type":"content_block_delta","delta":{"type":"unknown_delta","text":"x"}}"#,
            r#"data: {"type":"content_block_delta","delta":"not an object"}"#,
            "data: {}",
            "event: ping",
            ": keep-alive comment",
            "data:",
            "",
        ] {
            let parsed = parse_sse_line(line, &mut pending);
            assert!(
                parsed.as_ref().is_none_or(Vec::is_empty),
                "{line} produced {parsed:?}"
            );
        }
        assert!(pending.error.is_none(), "none of these is a failure");
        assert!(pending.tool.is_none(), "none of these starts a tool call");

        let events = feed(
            &mut pending,
            &[
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"still alive"}}"#,
            ],
        );
        assert_eq!(events, vec![ProviderEvent::TextDelta("still alive".into())]);
    }

    #[test]
    fn an_error_frame_becomes_a_provider_rejection_carrying_the_provider_text() {
        let mut pending = StreamState::default();
        let parsed = parse_sse_line(
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            &mut pending,
        );

        assert_eq!(parsed, Some(Vec::new()), "an error frame emits no event");
        assert_eq!(
            pending.error.as_deref(),
            Some("Overloaded"),
            "the provider's own words are what the operator needs"
        );
    }

    #[test]
    fn an_error_frame_without_a_message_still_says_something() {
        let mut pending = StreamState::default();
        parse_sse_line(r#"data: {"type":"error","error":{}}"#, &mut pending);

        assert_eq!(pending.error.as_deref(), Some("provider error"));
    }

    #[test]
    fn a_stream_that_stops_mid_tool_call_flushes_the_call_before_finishing() {
        let mut pending = StreamState::default();
        feed(
            &mut pending,
            &[
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":11}}}"#,
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_9","name":"fs-_write","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            ],
        );

        let flushed = finish_pending(&mut pending);

        match &flushed[0] {
            ProviderEvent::ToolCall(call) => {
                assert_eq!(call.name, "fs.write");
                assert_eq!(call.arguments, json!({"path": "a"}));
            }
            other => panic!("a consumer that stops at Finished must not miss the call: {other:?}"),
        }
        assert_eq!(
            flushed[1],
            ProviderEvent::Finished {
                input_tokens: Some(11),
                output_tokens: None
            }
        );
        assert!(pending.tool.is_none(), "the call is flushed once");
    }

    #[test]
    fn usage_is_reported_only_when_the_provider_reports_it() {
        let mut pending = StreamState::default();
        let events = feed(
            &mut pending,
            &[
                r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        );

        assert_eq!(
            events,
            vec![ProviderEvent::Finished {
                input_tokens: None,
                output_tokens: None
            }],
            "a count the provider never sent is not invented"
        );
    }

    #[test]
    fn invalid_tool_arguments_do_not_panic_the_stream() {
        let mut pending = StreamState::default();
        let events = feed(
            &mut pending,
            &[
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"fs-_read","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not json"}}"#,
                r#"data: {"type":"content_block_stop","index":0}"#,
            ],
        );

        match &events[0] {
            ProviderEvent::ToolCall(call) => assert_eq!(
                call.arguments,
                Value::Null,
                "unparsable arguments arrive as null for the kernel to refuse, not as a panic"
            ),
            other => panic!("{other:?}"),
        }
    }

    // ---- error mapping --------------------------------------------------------------------

    #[test]
    fn every_refused_status_maps_to_the_variant_the_sibling_provider_uses() {
        let transient = |status: u16| {
            rejection(
                reqwest::StatusCode::from_u16(status).unwrap(),
                "nope",
                ENDPOINT,
            )
        };
        // The same statuses `OpenAiProvider` answers with `ProviderTransient`: the provider's side,
        // or a credential the operator can fix, never the transcript.
        for status in [400, 401, 403, 404, 422, 500, 502, 529] {
            let err = transient(status);
            assert!(
                matches!(err, VikramError::ProviderTransient(_)),
                "{status} must be transient so it can be retried, got {err:?}"
            );
        }
        // The ones it answers with `ProviderRejected`: this turn is over.
        for status in [402, 429] {
            let err = transient(status);
            assert!(
                matches!(err, VikramError::ProviderRejected(_)),
                "{status} must be a final refusal, got {err:?}"
            );
        }
    }

    #[test]
    fn a_credit_refusal_is_final_whatever_status_carries_it() {
        // Anthropic reports an unpaid account as a 400 whose message names the credit balance.
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        let err = rejection(reqwest::StatusCode::from_u16(400).unwrap(), body, ENDPOINT);

        assert!(
            matches!(err, VikramError::ProviderRejected(_)),
            "an account that cannot pay must not be retried: {err:?}"
        );
    }

    #[test]
    fn a_rejection_names_the_status_and_the_endpoint_and_bounds_the_body() {
        let err = rejection(
            reqwest::StatusCode::from_u16(401).unwrap(),
            &"x".repeat(4_000),
            ENDPOINT,
        );

        let text = err.to_string();
        assert!(text.contains("401"), "{text}");
        assert!(text.contains(ENDPOINT), "{text}");
        assert!(
            text.len() < 700,
            "a provider body must not flood the interface: {} chars",
            text.len()
        );
    }

    #[test]
    fn truncation_counts_characters_and_marks_the_cut() {
        assert_eq!(truncate("short", 240), "short");
        assert_eq!(truncate("widened", 3), "wid…");
        assert_eq!(
            truncate("éééé", 2),
            "éé…",
            "the limit is in characters, so a multi-byte body cannot be cut mid-character"
        );
    }

    // ---- a real socket, on loopback only --------------------------------------------------

    /// The byte index just past the blank line that ends an HTTP request head.
    fn head_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }

    /// The `content-length` a request head declares, in bytes.
    fn content_length(head: &[u8]) -> usize {
        String::from_utf8_lossy(head)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0)
    }

    /// One-shot HTTP server on a loopback port: records the request head, then answers with
    /// `body_pieces` written one at a time, so a frame can straddle two network reads. Nothing
    /// here leaves the machine.
    async fn loopback_server(
        status: &'static str,
        content_type: &'static str,
        body_pieces: Vec<String>,
    ) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let port = listener.local_addr().expect("loopback address").port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let length: usize = body_pieces.iter().map(String::len).sum();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept the provider");
            let mut received = Vec::new();
            let mut chunk = [0u8; 2048];
            let head = loop {
                let read = socket.read(&mut chunk).await.expect("read the request");
                if read == 0 {
                    break String::new();
                }
                received.extend_from_slice(&chunk[..read]);
                if let Some(end) = head_end(&received)
                    && received.len() >= end + content_length(&received[..end])
                {
                    break String::from_utf8_lossy(&received[..end]).into_owned();
                }
            };
            let _ = tx.send(head);

            let response_head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            );
            socket
                .write_all(response_head.as_bytes())
                .await
                .expect("write the response head");
            for piece in body_pieces {
                socket
                    .write_all(piece.as_bytes())
                    .await
                    .expect("write a response piece");
                socket.flush().await.expect("flush");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = socket.shutdown().await;
        });

        (port, rx)
    }

    /// Drain a provider stream, so an assertion can see the whole answer.
    async fn collect(
        mut stream: ProviderStream,
    ) -> Vec<std::result::Result<ProviderEvent, VikramError>> {
        use futures_util::StreamExt;

        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item);
        }
        events
    }

    fn sse(lines: &[&str]) -> String {
        let mut document = String::new();
        for line in lines {
            document.push_str(line);
            document.push('\n');
        }
        document
    }

    const TEXT_ANSWER: &[&str] = &[
        "event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":12,"output_tokens":1}}}"#,
        "",
        "event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        "event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        "",
        "event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}"#,
        "",
        "event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        "event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        "",
        "event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ];

    #[tokio::test]
    async fn a_stream_over_loopback_emits_the_translated_events_and_the_api_headers() {
        let document = sse(TEXT_ANSWER);
        // Split inside a text delta: the provider's buffer has to reassemble the frame itself.
        let cut = document
            .find("\"Hello\"")
            .expect("the canned answer carries a text delta")
            + 2;
        let (port, recorded) = loopback_server(
            "200 OK",
            "text/event-stream",
            vec![document[..cut].to_owned(), document[cut..].to_owned()],
        )
        .await;

        let provider = AnthropicProvider::new(
            format!("http://127.0.0.1:{port}"),
            MODEL,
            "sk-ant-test",
            Some(200_000),
        );
        assert_eq!(
            provider.endpoint(),
            format!("http://127.0.0.1:{port}/v1/messages")
        );
        let stream = provider.stream(CompletionRequest::new(
            MODEL,
            vec![Message::system("be brief"), Message::user("say hi")],
            vec![tool_spec("fs.read")],
        ));

        let events = tokio::time::timeout(Duration::from_secs(10), collect(stream))
            .await
            .expect("the loopback server answers well inside the timeout");

        assert_eq!(
            events,
            vec![
                Ok(ProviderEvent::TextDelta("Hello".into())),
                Ok(ProviderEvent::TextDelta(", world".into())),
                Ok(ProviderEvent::Finished {
                    input_tokens: Some(12),
                    output_tokens: Some(5),
                }),
            ],
            "a frame split across two reads must still become one delta"
        );

        let head = tokio::time::timeout(Duration::from_secs(5), recorded)
            .await
            .expect("the server recorded the request")
            .expect("the request head was sent");
        assert!(
            head.starts_with("POST /v1/messages "),
            "the Messages API lives at its own path: {head}"
        );
        let lower = head.to_lowercase();
        assert!(lower.contains("x-api-key: sk-ant-test"), "{head}");
        assert!(
            lower.contains("anthropic-version: 2023-06-01"),
            "the API version header is mandatory: {head}"
        );
        assert!(lower.contains("content-type: application/json"), "{head}");
        assert!(
            !lower.contains("authorization:"),
            "Anthropic wants `x-api-key`, not a bearer token: {head}"
        );
    }

    #[tokio::test]
    async fn an_error_frame_inside_a_live_stream_reaches_the_consumer_as_a_rejection() {
        let (port, _recorded) = loopback_server(
            "200 OK",
            "text/event-stream",
            vec![sse(&[
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
                "",
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                "",
            ])],
        )
        .await;
        let provider = AnthropicProvider::new(format!("http://127.0.0.1:{port}"), MODEL, "k", None);

        let events = tokio::time::timeout(
            Duration::from_secs(10),
            collect(provider.stream(request(vec![Message::user("hi")]))),
        )
        .await
        .expect("the loopback server answers inside the timeout");

        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0], Ok(ProviderEvent::TextDelta("partial".into())));
        match &events[1] {
            Err(VikramError::ProviderRejected(text)) => {
                assert!(text.contains("Overloaded"), "{text}")
            }
            other => panic!("a mid-stream failure must not read as a finished answer: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rate_limited_reply_becomes_a_rejection_over_the_wire() {
        let (port, _recorded) = loopback_server(
            "429 Too Many Requests",
            "application/json",
            vec![
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
                    .to_owned(),
            ],
        )
        .await;
        let provider = AnthropicProvider::new(format!("http://127.0.0.1:{port}"), MODEL, "k", None);

        let events = tokio::time::timeout(
            Duration::from_secs(10),
            collect(provider.stream(request(vec![Message::user("hi")]))),
        )
        .await
        .expect("the loopback server answers inside the timeout");

        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            Err(VikramError::ProviderRejected(text)) => {
                assert!(text.contains("429"), "{text}");
                assert!(text.contains("slow down"), "{text}");
            }
            other => panic!("expected a final refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_port_is_a_typed_error_rather_than_a_hang() {
        // Bind a port, then release it: nothing is listening, and the refusal stays on loopback.
        let port = {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port to release");
            listener.local_addr().expect("loopback address").port()
        };
        let provider = AnthropicProvider::new(format!("http://127.0.0.1:{port}"), MODEL, "k", None);

        let events = tokio::time::timeout(
            Duration::from_secs(20),
            collect(provider.stream(request(vec![Message::user("hi")]))),
        )
        .await
        .expect("a refused connection must not hang the turn");

        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            Err(VikramError::ProviderTransient(text)) => {
                assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
                assert!(text.contains("cannot reach"), "{text}");
            }
            other => panic!("expected a transient provider error, got {other:?}"),
        }
    }

    // ---- from_config ----------------------------------------------------------------------

    /// A key name no other test and no operator's `.env` can have.
    fn unique_key_env(label: &str) -> String {
        format!("VIKRAM_ANTHROPIC_TEST_{}_{label}", std::process::id()).to_uppercase()
    }

    #[test]
    fn an_empty_model_builds_no_provider() {
        let name = unique_key_env("empty-model");
        unsafe { std::env::set_var(&name, "sk-ant-test") };
        let config = Config {
            model: "   ".into(),
            api_key_env: name.clone(),
            ..Config::default()
        };

        assert!(
            AnthropicProvider::from_config(&config).unwrap().is_none(),
            "a key without a model is not a configured provider"
        );
        unsafe { std::env::remove_var(&name) };
    }

    #[test]
    fn a_missing_key_builds_no_provider() {
        let name = unique_key_env("missing-key");
        unsafe { std::env::remove_var(&name) };
        let config = Config {
            model: MODEL.into(),
            api_key_env: name,
            ..Config::default()
        };

        assert!(AnthropicProvider::from_config(&config).unwrap().is_none());
    }

    #[test]
    fn a_blank_base_url_falls_back_to_the_public_endpoint_and_keeps_the_context_window() {
        let name = unique_key_env("blank-base-url");
        unsafe { std::env::set_var(&name, "sk-ant-test") };
        let config = Config {
            base_url: "   ".into(),
            model: MODEL.into(),
            api_key_env: name.clone(),
            context_window: Some(200_000),
            ..Config::default()
        };

        let provider = AnthropicProvider::from_config(&config)
            .unwrap()
            .expect("a model and a key build a provider");

        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.model(), MODEL);
        assert_eq!(provider.endpoint(), ENDPOINT);
        assert_eq!(
            provider.context_window(),
            Some(200_000),
            "the window comes from configuration and is passed through untouched"
        );
        assert!(
            !format!("{provider:?}").contains("sk-ant-test"),
            "the key never appears in a debug rendering"
        );
        unsafe { std::env::remove_var(&name) };
    }

    #[test]
    fn a_trailing_slash_is_trimmed_from_the_base_url() {
        let provider = AnthropicProvider::new("https://api.anthropic.com///", MODEL, "k", None);

        assert_eq!(
            provider.endpoint(),
            ENDPOINT,
            "no double slash before the path"
        );
        assert_eq!(provider.context_window(), None);
    }

    #[test]
    fn a_config_without_a_context_window_reports_none_rather_than_a_guess() {
        let provider = AnthropicProvider::new("https://api.anthropic.com", MODEL, "k", None);

        assert_eq!(
            provider.context_window(),
            None,
            "inventing a window would corrupt the context metric"
        );
    }
}
