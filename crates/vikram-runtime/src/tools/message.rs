use serde_json::{Value, json};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// Most options one poll may carry; chat platforms cap native polls far lower and a longer list
/// is a wall of text anyway.
const MAX_POLL_OPTIONS: usize = 10;

/// Longest question the marker line will carry; the whole `POLL:` line must stay well under
/// [`MAX_TOOL_OUTPUT`] so truncation can never cut the JSON in half.
const MAX_POLL_QUESTION: usize = 500;

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
}
