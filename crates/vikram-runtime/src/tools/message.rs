use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// Most options one poll may carry; chat platforms cap native polls far lower and a longer list
/// is a wall of text anyway.
const MAX_POLL_OPTIONS: usize = 10;

/// Longest question the marker line will carry; the whole `POLL:` line must stay well under
/// [`MAX_TOOL_OUTPUT`] so truncation can never cut the JSON in half.
const MAX_POLL_QUESTION: usize = 500;

/// Largest serialized card payload the marker line will carry; same [`MAX_TOOL_OUTPUT`] budget
/// as the poll marker, with headroom for the prefix and the trailing summary line.
const MAX_CARD_BYTES: usize = 16_000;

/// Largest serialized draft payload the marker line will carry; same [`MAX_TOOL_OUTPUT`] budget
/// as the card marker.
const MAX_DRAFT_BYTES: usize = 16_000;

/// `message.poll` asks the chat a question with tappable answers. The tool itself only emits the
/// `POLL:{...}` marker line the gateway parses out of the committed tool result and delivers as
/// an outbound poll verb; like `ask_user`, the permission shape is a read of the conversation,
/// not a mutation of the workspace.
pub(crate) struct MessagePoll;

impl Tool for MessagePoll {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message.poll".into(),
            description: "Ask the user a poll question with 2-10 options; the answer arrives as a normal reply.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "the poll question"},
                    "options": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 10}
                },
                "required": ["question", "options"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("poll");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "message.poll",
            question,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let question = arg_str(args, "question")?;
        let question = question.trim();
        if question.is_empty() {
            return Err(VikramError::ProviderRejected(
                "`question` must not be empty".to_owned(),
            ));
        }
        if question.len() > MAX_POLL_QUESTION {
            return Err(VikramError::ProviderRejected(format!(
                "`question` is capped at {MAX_POLL_QUESTION} characters"
            )));
        }
        let raw_options = args
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                VikramError::ProviderRejected("`options` must be an array of strings".to_owned())
            })?;
        let mut options: Vec<String> = Vec::new();
        for raw in raw_options {
            let option = raw.as_str().map(str::trim).unwrap_or("");
            if option.len() > MAX_POLL_QUESTION {
                return Err(VikramError::ProviderRejected(format!(
                    "each option is capped at {MAX_POLL_QUESTION} characters"
                )));
            }
            if !option.is_empty() && !options.iter().any(|seen| seen == option) {
                options.push(option.to_owned());
            }
        }
        if options.len() < 2 {
            return Err(VikramError::ProviderRejected(
                "`options` needs at least 2 non-empty unique strings".to_owned(),
            ));
        }
        if options.len() > MAX_POLL_OPTIONS {
            return Err(VikramError::ProviderRejected(format!(
                "`options` is capped at {MAX_POLL_OPTIONS} unique entries"
            )));
        }
        // The marker line must stay a single line of compact JSON; serde_json escapes any
        // newline or quote inside the strings so `POLL:` never splits mid-payload.
        let marker = json!({"question": question, "options": options});
        let content = format!(
            "POLL:{}\nPoll sent: {question} ({} options)",
            serde_json::to_string(&marker).expect("poll marker serializes"),
            options.len()
        );
        Ok(ToolOutput { content, ok: true })
    }
}

/// `message.card` posts a provider-native rich card to the chat. The tool only emits the
/// `CARD:{...}` marker line the gateway parses out of the committed tool result and delivers as
/// an outbound card verb; the card schema itself is adapter-defined (Slack Block Kit `blocks`,
/// Teams adaptive card, Google Chat `cardsV2`, …) and passed through untouched, so the model must
/// shape `card` for the platform it is talking to. Like `message.poll`, the permission shape is
/// a read of the conversation, not a mutation of the workspace.
pub(crate) struct MessageCard;

impl Tool for MessageCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message.card".into(),
            description: "Post a rich card to the chat; `card` is the provider-native JSON payload (schema is platform-defined, e.g. Slack blocks or a Teams adaptive card).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "card": {"type": "object", "description": "the provider-native card JSON; its schema is defined by the chat platform's adapter"}
                },
                "required": ["card"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let target = args
            .get("card")
            .and_then(|card| card.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("card");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "message.card",
            target,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let card = args.get("card").ok_or_else(|| {
            VikramError::ProviderRejected("missing object argument `card`".to_owned())
        })?;
        if !card.is_object() || card.as_object().is_some_and(|map| map.is_empty()) {
            return Err(VikramError::ProviderRejected(
                "`card` must be a non-empty JSON object".to_owned(),
            ));
        }
        // The marker line must stay a single line of compact JSON under MAX_TOOL_OUTPUT;
        // serde_json escapes any newline or quote inside the payload so `CARD:` never splits.
        let body = serde_json::to_string(card).expect("card payload serializes");
        if body.len() > MAX_CARD_BYTES {
            return Err(VikramError::ProviderRejected(format!(
                "`card` is capped at {MAX_CARD_BYTES} bytes of JSON"
            )));
        }
        let content = format!("CARD:{body}\nCard sent.");
        Ok(ToolOutput { content, ok: true })
    }
}

/// `message.draft` previews an unsent message in the chat. The tool only emits the
/// `DRAFT:{...}` marker line the gateway parses out of the committed tool result and delivers as
/// an outbound draft verb; like `message.poll`, the permission shape is a read of the
/// conversation, not a mutation of the workspace.
pub(crate) struct MessageDraft;

impl Tool for MessageDraft {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message.draft".into(),
            description: "Preview an unsent message in the chat as a draft; `text` is the draft body, shown as a preview rather than delivered as a final reply.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "the draft text to preview"}
                },
                "required": ["text"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let target = args
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(120).collect::<String>())
            .unwrap_or_else(|| "draft".to_owned());
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "message.draft",
            target,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let text = arg_str(args, "text")?;
        if text.trim().is_empty() {
            return Err(VikramError::ProviderRejected(
                "`text` must not be empty".to_owned(),
            ));
        }
        // The marker line must stay a single line of compact JSON under MAX_TOOL_OUTPUT;
        // serde_json escapes any newline or quote inside the text so `DRAFT:` never splits.
        let marker = json!({"text": text});
        let body = serde_json::to_string(&marker).expect("draft marker serializes");
        if body.len() > MAX_DRAFT_BYTES {
            return Err(VikramError::ProviderRejected(format!(
                "`text` is capped at {MAX_DRAFT_BYTES} bytes of JSON"
            )));
        }
        let content = format!("DRAFT:{body}\nDraft preview shown.");
        Ok(ToolOutput { content, ok: true })
    }
}

/// Largest emoji payload the marker line will carry; ZWJ sequences run ~25 bytes, so 128 is
/// generous headroom while keeping the `REACT:` line trivially small.
const MAX_REACT_EMOJI_BYTES: usize = 128;

/// `message.react` attaches an emoji reaction to the chat message that triggered the turn. The
/// tool only emits the `REACT:{...}` marker line the gateway parses out of the committed tool
/// result; the target is resolved there from the inbound event's platform message id, so the
/// model never has to (and cannot) name a message. Like `message.poll`, the permission shape is
/// a read of the conversation, not a mutation of the workspace.
pub(crate) struct MessageReact;

impl Tool for MessageReact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message.react".into(),
            description: "React to the user's message with an emoji; `emoji` is a single emoji or emoji sequence, `remove` clears a previous reaction.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "emoji": {"type": "string", "description": "the emoji to react with, e.g. 👍 or 🎉"},
                    "remove": {"type": "boolean", "description": "true removes the reaction instead of adding it"}
                },
                "required": ["emoji"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let target = args
            .get("emoji")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|emoji| !emoji.is_empty())
            .map(|emoji| emoji.chars().take(64).collect::<String>())
            .unwrap_or_else(|| "reaction".to_owned());
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "message.react",
            target,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let emoji = arg_str(args, "emoji")?.trim().to_owned();
        if emoji.is_empty() {
            return Err(VikramError::ProviderRejected(
                "`emoji` must not be empty".to_owned(),
            ));
        }
        if emoji.len() > MAX_REACT_EMOJI_BYTES {
            return Err(VikramError::ProviderRejected(format!(
                "`emoji` is capped at {MAX_REACT_EMOJI_BYTES} bytes"
            )));
        }
        if emoji.chars().any(char::is_control) {
            return Err(VikramError::ProviderRejected(
                "`emoji` must not contain control characters".to_owned(),
            ));
        }
        let remove = match args.get("remove") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| {
                VikramError::ProviderRejected("`remove` must be a boolean".to_owned())
            })?,
        };
        // The marker line must stay a single line of compact JSON; serde_json escapes any
        // quote inside the emoji so `REACT:` never splits mid-payload.
        let marker = json!({"emoji": emoji, "remove": remove});
        let body = serde_json::to_string(&marker).expect("reaction marker serializes");
        let content = if remove {
            format!("REACT:{body}\nReaction removed.")
        } else {
            format!("REACT:{body}\nReaction sent.")
        };
        Ok(ToolOutput { content, ok: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::default()
    }

    #[test]
    fn emits_a_marker_line_the_gateway_contract_parses() {
        let out = MessagePoll
            .run(
                &json!({"question": "Lunch?", "options": ["Pizza", "Sushi"]}),
                &ctx(),
            )
            .unwrap();
        assert!(out.ok);
        let marker = out
            .content
            .lines()
            .next()
            .expect("marker is the first line");
        assert!(marker.starts_with("POLL:{"), "{marker}");
        let body: Value = serde_json::from_str(marker.trim_start_matches("POLL:")).unwrap();
        assert_eq!(body["question"], "Lunch?");
        assert_eq!(body["options"], json!(["Pizza", "Sushi"]));
    }

    #[test]
    fn blank_duplicates_and_extra_options_are_normalized() {
        let out = MessagePoll
            .run(
                &json!({"question": " Pick? ", "options": [" a ", "a", "", "b", "b"]}),
                &ctx(),
            )
            .unwrap();
        let marker = out.content.lines().next().unwrap();
        let body: Value = serde_json::from_str(marker.trim_start_matches("POLL:")).unwrap();
        assert_eq!(body["question"], "Pick?");
        assert_eq!(body["options"], json!(["a", "b"]));
    }

    #[test]
    fn rejects_empty_question_and_thin_options() {
        for args in [
            json!({"question": "   ", "options": ["a", "b"]}),
            json!({"question": "Q?", "options": []}),
            json!({"question": "Q?", "options": ["only"]}),
            json!({"question": "Q?", "options": ["a", "a"]}),
            json!({"question": "Q?", "options": ["a", " ", "\t"]}),
            json!({"question": "Q?"}),
            json!({"question": "Q?", "options": "a,b"}),
        ] {
            assert!(
                MessagePoll.run(&args, &ctx()).is_err(),
                "{args} should be rejected"
            );
        }
    }

    #[test]
    fn caps_options_at_ten_unique_entries() {
        let eleven: Vec<String> = (0..11).map(|i| format!("opt{i}")).collect();
        assert!(
            MessagePoll
                .run(&json!({"question": "Q?", "options": eleven}), &ctx())
                .is_err()
        );
        let ten: Vec<String> = (0..10).map(|i| format!("opt{i}")).collect();
        assert!(
            MessagePoll
                .run(&json!({"question": "Q?", "options": ten}), &ctx())
                .is_ok()
        );
    }

    #[test]
    fn rejects_overlong_questions_and_options() {
        let long = "x".repeat(MAX_POLL_QUESTION + 1);
        assert!(
            MessagePoll
                .run(&json!({"question": long, "options": ["a", "b"]}), &ctx())
                .is_err()
        );
        assert!(
            MessagePoll
                .run(&json!({"question": "Q?", "options": ["a", long]}), &ctx())
                .is_err()
        );
    }

    #[test]
    fn marker_is_the_first_line_so_truncation_cannot_split_it() {
        let options: Vec<String> = (0..10)
            .map(|i| format!("opt{i}-{}", "y".repeat(MAX_POLL_QUESTION - 10)))
            .collect();
        let out = MessagePoll
            .run(
                &json!({
                    "question": "Q?".to_owned() + &"x".repeat(MAX_POLL_QUESTION - 2),
                    "options": options
                }),
                &ctx(),
            )
            .unwrap();
        let kept = truncated(out.content);
        let marker = kept.lines().next().unwrap();
        assert!(marker.starts_with("POLL:{"), "{marker}");
        let body: Value = serde_json::from_str(marker.trim_start_matches("POLL:")).unwrap();
        assert_eq!(body["options"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn permission_matches_ask_user_shape() {
        let req = MessagePoll
            .permission(&json!({"question": "Lunch?", "options": ["a", "b"]}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "message.poll");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);
    }

    #[test]
    fn registered_in_builtins() {
        let registry = ToolRegistry::with_builtins();
        assert_eq!(
            registry.get("message.poll").unwrap().spec().name,
            "message.poll"
        );
    }

    #[test]
    fn card_emits_a_marker_line_carrying_the_payload_verbatim() {
        let card = json!({
            "type": "AdaptiveCard",
            "body": [{"type": "TextBlock", "text": "Deploy finished"}]
        });
        let out = MessageCard
            .run(&json!({"card": card.clone()}), &ctx())
            .unwrap();
        assert!(out.ok);
        let marker = out
            .content
            .lines()
            .next()
            .expect("marker is the first line");
        assert!(marker.starts_with("CARD:{"), "{marker}");
        let body: Value = serde_json::from_str(marker.trim_start_matches("CARD:")).unwrap();
        assert_eq!(body, card);
    }

    #[test]
    fn card_rejects_missing_empty_and_non_object_payloads() {
        for args in [
            json!({}),
            json!({"card": {}}),
            json!({"card": []}),
            json!({"card": "blocks"}),
            json!({"card": 42}),
            json!({"card": null}),
        ] {
            assert!(
                MessageCard.run(&args, &ctx()).is_err(),
                "{args} should be rejected"
            );
        }
    }

    #[test]
    fn card_payload_is_capped_under_the_tool_output_budget() {
        let huge = json!({"pad": "x".repeat(MAX_CARD_BYTES)});
        assert!(MessageCard.run(&json!({"card": huge}), &ctx()).is_err());
        let fits = json!({"pad": "x".repeat(MAX_CARD_BYTES - 32)});
        let out = MessageCard.run(&json!({"card": fits}), &ctx()).unwrap();
        assert!(out.ok);
        // The whole first line — prefix plus compact JSON — stays far under MAX_TOOL_OUTPUT.
        let marker = out.content.lines().next().unwrap();
        assert!(marker.len() < MAX_TOOL_OUTPUT, "{}", marker.len());
        let kept = truncated(out.content);
        let kept_marker = kept.lines().next().unwrap();
        assert!(
            serde_json::from_str::<Value>(kept_marker.trim_start_matches("CARD:")).is_ok(),
            "truncation must not be able to split the marker"
        );
    }

    #[test]
    fn card_permission_matches_poll_shape() {
        let req = MessageCard
            .permission(&json!({"card": {"title": "Deploy status"}}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "message.card");
        assert_eq!(req.resource, "Deploy status");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);
        let req = MessageCard.permission(&json!({})).unwrap();
        assert_eq!(req.resource, "card");
    }

    #[test]
    fn draft_emits_a_marker_line_carrying_the_text_verbatim() {
        let text = "Draft reply:\nline two with \"quotes\"";
        let out = MessageDraft.run(&json!({"text": text}), &ctx()).unwrap();
        assert!(out.ok);
        let marker = out
            .content
            .lines()
            .next()
            .expect("marker is the first line");
        assert!(marker.starts_with("DRAFT:{"), "{marker}");
        let body: Value = serde_json::from_str(marker.trim_start_matches("DRAFT:")).unwrap();
        assert_eq!(body["text"], text);
    }

    #[test]
    fn draft_rejects_missing_and_blank_text() {
        for args in [
            json!({}),
            json!({"text": ""}),
            json!({"text": "   "}),
            json!({"text": "\n\t"}),
            json!({"text": 42}),
            json!({"text": null}),
        ] {
            assert!(
                MessageDraft.run(&args, &ctx()).is_err(),
                "{args} should be rejected"
            );
        }
    }

    #[test]
    fn draft_payload_is_capped_under_the_tool_output_budget() {
        let huge = json!({"text": "x".repeat(MAX_DRAFT_BYTES)});
        assert!(MessageDraft.run(&huge, &ctx()).is_err());
        let fits = json!({"text": "x".repeat(MAX_DRAFT_BYTES - 32)});
        let out = MessageDraft.run(&fits, &ctx()).unwrap();
        assert!(out.ok);
        let marker = out.content.lines().next().unwrap();
        assert!(marker.len() < MAX_TOOL_OUTPUT, "{}", marker.len());
        let kept = truncated(out.content);
        let kept_marker = kept.lines().next().unwrap();
        assert!(
            serde_json::from_str::<Value>(kept_marker.trim_start_matches("DRAFT:")).is_ok(),
            "truncation must not be able to split the marker"
        );
    }

    #[test]
    fn draft_permission_matches_poll_shape() {
        let req = MessageDraft
            .permission(&json!({"text": "here is a preview"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "message.draft");
        assert_eq!(req.resource, "here is a preview");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);
        let req = MessageDraft.permission(&json!({})).unwrap();
        assert_eq!(req.resource, "draft");
    }

    #[test]
    fn draft_registered_in_builtins() {
        let registry = ToolRegistry::with_builtins();
        assert_eq!(
            registry.get("message.draft").unwrap().spec().name,
            "message.draft"
        );
    }

    #[test]
    fn react_emits_a_marker_line_carrying_emoji_and_remove() {
        let out = MessageReact.run(&json!({"emoji": "👍"}), &ctx()).unwrap();
        assert!(out.ok);
        let marker = out
            .content
            .lines()
            .next()
            .expect("marker is the first line");
        assert!(marker.starts_with("REACT:{"), "{marker}");
        let body: Value = serde_json::from_str(marker.trim_start_matches("REACT:")).unwrap();
        assert_eq!(body["emoji"], "👍");
        assert_eq!(body["remove"], false);

        let out = MessageReact
            .run(&json!({"emoji": "👀", "remove": true}), &ctx())
            .unwrap();
        let marker = out.content.lines().next().unwrap();
        let body: Value = serde_json::from_str(marker.trim_start_matches("REACT:")).unwrap();
        assert_eq!(body["emoji"], "👀");
        assert_eq!(body["remove"], true);
    }

    #[test]
    fn react_rejects_missing_blank_and_oversized_emoji() {
        for args in [
            json!({}),
            json!({"emoji": ""}),
            json!({"emoji": "   "}),
            json!({"emoji": 42}),
            json!({"emoji": null}),
            json!({"emoji": "x".repeat(MAX_REACT_EMOJI_BYTES + 1)}),
            json!({"emoji": "bad\nemoji"}),
        ] {
            assert!(
                MessageReact.run(&args, &ctx()).is_err(),
                "{args} should be rejected"
            );
        }
        for args in [
            json!({"emoji": "👍", "remove": "yes"}),
            json!({"emoji": "👍", "remove": 1}),
        ] {
            assert!(
                MessageReact.run(&args, &ctx()).is_err(),
                "{args} should be rejected"
            );
        }
    }

    #[test]
    fn react_marker_stays_one_line_under_truncation() {
        let out = MessageReact
            .run(&json!({"emoji": "family\u{200d}emoji \"quoted\""}), &ctx())
            .unwrap();
        let kept = truncated(out.content);
        let marker = kept.lines().next().unwrap();
        assert!(marker.starts_with("REACT:{"), "{marker}");
        assert!(
            serde_json::from_str::<Value>(marker.trim_start_matches("REACT:")).is_ok(),
            "truncation must not be able to split the marker"
        );
    }

    #[test]
    fn react_permission_matches_poll_shape() {
        let req = MessageReact.permission(&json!({"emoji": "👍"})).unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "message.react");
        assert_eq!(req.resource, "👍");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);
        let req = MessageReact.permission(&json!({})).unwrap();
        assert_eq!(req.resource, "reaction");
    }

    #[test]
    fn react_registered_in_builtins() {
        let registry = ToolRegistry::with_builtins();
        assert_eq!(
            registry.get("message.react").unwrap().spec().name,
            "message.react"
        );
    }
}
