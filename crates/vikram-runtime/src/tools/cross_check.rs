use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use vikram_contracts::{ActionKind, Message, PermissionRequest, SideEffect, Trust, VikramError};

use crate::provider::{CompletionRequest, Provider, ProviderEvent};
use crate::tools::*;

const ANSWER_CHARS: usize = 1_200;
const ANSWER_TOKENS: u32 = 512;
const ANSWER_BUDGET: Duration = Duration::from_secs(60);

/// `cross_check` — one question, two models, both answers shown. Divergence is reported only on
/// lexical evidence; the tool never claims that agreement means the answer is right.
pub(crate) struct CrossCheck;

impl Tool for CrossCheck {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cross_check".into(),
            description: "Ask the same question of the model in force and one other named model, \
                          show both answers, and flag only a concrete disagreement."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "the question both models are asked"},
                    "other_model": {"type": "string", "description": "the second model to compare against; it is required"}
                },
                "required": ["question"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("question")
            .to_owned();
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "cross_check",
            question,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let question = arg_str(args, "question")?;
        let other_model = args
            .get("other_model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some(other_model) = other_model else {
            return Ok(ToolOutput {
                content: "cross_check needs `other_model`: pass the name of the second model to \
                          compare against. Picking one silently would make the comparison \
                          meaningless, so nothing was asked."
                    .into(),
                ok: false,
            });
        };
        let Some(runtime) = ctx.runtime.as_ref() else {
            return Ok(ToolOutput {
                content: "cross_check needs a live session: there is no runtime to read the model \
                          and endpoint configuration from."
                    .into(),
                ok: false,
            });
        };
        let config = runtime
            .config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let primary_model = runtime.model();
        if primary_model.trim().is_empty() {
            return Ok(ToolOutput {
                content: "cross_check cannot run: no model is in force for this session.".into(),
                ok: false,
            });
        }
        if primary_model.trim() == other_model {
            return Ok(ToolOutput {
                content: format!(
                    "cross_check needs a different `other_model`: `{other_model}` is the model \
                     already in force, and a model compared with itself is not a cross-check."
                ),
                ok: false,
            });
        }

        // The fallback wrapper would answer under a third model when one side fails, which is
        // exactly what a cross-check must not do silently.
        let mut primary_config = config.clone();
        primary_config.model = primary_model.clone();
        primary_config.fallback_model = String::new();
        let mut other_config = config;
        other_config.model = other_model.to_owned();
        other_config.fallback_model = String::new();

        let primary = match crate::provider::for_config(&primary_config) {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                return Ok(ToolOutput {
                    content: "cross_check is not configured: no model or API key is available."
                        .into(),
                    ok: false,
                });
            }
            Err(error) => {
                return Ok(ToolOutput {
                    content: format!("cross_check cannot build the primary provider: {error}"),
                    ok: false,
                });
            }
        };
        let other = match crate::provider::for_config(&other_config) {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                return Ok(ToolOutput {
                    content: format!(
                        "cross_check cannot build `{other_model}`: the endpoint has no model or \
                         API key for it."
                    ),
                    ok: false,
                });
            }
            Err(error) => {
                return Ok(ToolOutput {
                    content: format!(
                        "cross_check cannot build the provider for `{other_model}`: {error}"
                    ),
                    ok: false,
                });
            }
        };

        let output = compare(
            &question,
            primary.as_ref(),
            &primary_model,
            other.as_ref(),
            other_model,
        );
        Ok(ToolOutput {
            content: truncated(output.content),
            ok: output.ok,
        })
    }
}

fn compare(
    question: &str,
    primary: &dyn Provider,
    primary_model: &str,
    other: &dyn Provider,
    other_model: &str,
) -> ToolOutput {
    let (primary_result, other_result) = match block_on(async {
        futures_util::future::join(
            ask(primary, primary_model, question),
            ask(other, other_model, question),
        )
        .await
    }) {
        Ok(results) => results,
        Err(error) => {
            return ToolOutput {
                content: format!("cross_check could not run: {error}"),
                ok: false,
            };
        }
    };

    match (primary_result, other_result) {
        (Ok(primary_answer), Ok(other_answer)) => ToolOutput {
            content: digest(primary_model, &primary_answer, other_model, &other_answer),
            ok: true,
        },
        (Err(error), Ok(_)) => ToolOutput {
            content: format!(
                "cross_check failed: the primary model `{primary_model}` did not answer: {error}. \
                 No comparison was made."
            ),
            ok: false,
        },
        (Ok(_), Err(error)) => ToolOutput {
            content: format!(
                "cross_check failed: the other model `{other_model}` did not answer: {error}. No \
                 comparison was made."
            ),
            ok: false,
        },
        (Err(primary_error), Err(other_error)) => ToolOutput {
            content: format!(
                "cross_check failed: neither model answered. `{primary_model}`: {primary_error}. \
                 `{other_model}`: {other_error}."
            ),
            ok: false,
        },
    }
}

fn digest(
    primary_model: &str,
    primary_answer: &str,
    other_model: &str,
    other_answer: &str,
) -> String {
    let mut out = format!(
        "primary ({primary_model}):\n{}\n\nother ({other_model}):\n{}\n\n",
        bounded_answer(primary_answer),
        bounded_answer(other_answer)
    );
    let evidence = evidence(primary_answer, other_answer);
    if evidence.is_empty() {
        out.push_str(
            "no divergence detected: the two answers do not contradict on any marker or number \
             this check reads. Two agreeing answers are not proof of correctness.\n",
        );
    } else {
        for clause in evidence {
            out.push_str(&format!(
                "possible divergence: the two answers disagree about {clause}.\n"
            ));
        }
        out.push_str(
            "This is a lexical check, not proof: confirm the two answers address the same claim.\n",
        );
    }
    out
}

fn evidence(primary: &str, other: &str) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(clause) = marker_clash(primary, other) {
        found.push(clause);
    }
    if let Some(clause) = numeric_clash(primary, other) {
        found.push(clause);
    }
    found
}

const POLARITY: &[(&str, &str)] = &[
    ("is supported", "is not supported"),
    ("is safe", "is not safe"),
    ("is possible", "is not possible"),
    ("is correct", "is incorrect"),
    ("is true", "is false"),
    ("does work", "does not work"),
    ("does support", "does not support"),
    ("can be done", "cannot be done"),
    ("can be used", "cannot be used"),
    ("is recommended", "is not recommended"),
    ("is available", "is not available"),
    ("yes", "no"),
];

/// A side is clean only when it carries one polarity of the pair and not the other. An answer that
/// says both ("supported on Linux, unsupported on Windows") is no signal, not a conflict.
fn side(text: &str, positive: &str, negative: &str) -> Option<bool> {
    match (has_phrase(text, positive), has_phrase(text, negative)) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

fn marker_clash(primary: &str, other: &str) -> Option<String> {
    let primary = primary.to_ascii_lowercase();
    let other = other.to_ascii_lowercase();
    for (positive, negative) in POLARITY {
        let (Some(left), Some(right)) = (
            side(&primary, positive, negative),
            side(&other, positive, negative),
        ) else {
            continue;
        };
        if left == right {
            continue;
        }
        let (asserted, denied) = if left {
            (*positive, *negative)
        } else {
            (*negative, *positive)
        };
        return Some(format!(
            "whether a point holds: one answer asserts \"{asserted}\" while the other asserts \
             \"{denied}\""
        ));
    }
    None
}

fn has_phrase(haystack: &str, phrase: &str) -> bool {
    if phrase.contains(' ') {
        haystack.contains(phrase)
    } else {
        has_word(haystack, phrase)
    }
}

fn has_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        before.is_none_or(|ch| !ch.is_alphanumeric())
            && after.is_none_or(|ch| !ch.is_alphanumeric())
    })
}

fn canonical_unit(word: &str) -> Option<&'static str> {
    Some(match word {
        "%" | "percent" => "%",
        "ms" | "millisecond" | "milliseconds" => "ms",
        "s" | "sec" | "secs" | "second" | "seconds" => "seconds",
        "min" | "mins" | "minute" | "minutes" => "minutes",
        "h" | "hr" | "hrs" | "hour" | "hours" => "hours",
        "kb" | "kilobyte" | "kilobytes" => "kb",
        "mb" | "megabyte" | "megabytes" => "mb",
        "gb" | "gigabyte" | "gigabytes" => "gb",
        "tb" | "terabyte" | "terabytes" => "tb",
        "token" | "tokens" => "tokens",
        "usd" | "dollar" | "dollars" => "usd",
        "eur" | "euro" | "euros" => "eur",
        _ => return None,
    })
}

/// Numbers only count when a known unit follows them, so a bare year or list index is ignored.
fn readings(text: &str) -> Vec<(&'static str, f64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut saw_dot = false;
        while i < chars.len() {
            let ch = chars[i];
            if ch.is_ascii_digit()
                || (matches!(ch, ',' | '_') && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
            {
                i += 1;
            } else if ch == '.' && !saw_dot && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()
            {
                saw_dot = true;
                i += 1;
            } else {
                break;
            }
        }
        let raw: String = chars[start..i]
            .iter()
            .filter(|ch| !matches!(ch, ',' | '_'))
            .collect();
        let value = raw.parse::<f64>().ok();
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if j < chars.len() && chars[j] == '%' {
            if let Some(value) = value {
                out.push(("%", value));
            }
            i = j + 1;
            continue;
        }
        let word_start = j;
        while j < chars.len() && chars[j].is_alphabetic() {
            j += 1;
        }
        if j > word_start {
            let word: String = chars[word_start..j]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            if let (Some(unit), Some(value)) = (canonical_unit(&word), value) {
                out.push((unit, value));
            }
        }
        i = j.max(start + 1);
    }
    out
}

fn numeric_clash(primary: &str, other: &str) -> Option<String> {
    let primary = grouped(&readings(primary));
    let other = grouped(&readings(other));
    for (unit, values) in &primary {
        let Some(theirs) = other.get(unit) else {
            continue;
        };
        if values
            .iter()
            .any(|left| theirs.iter().any(|right| left == right))
        {
            continue;
        }
        let clause = if *unit == "%" {
            format!("a percentage: {} vs {}", numbers(values), numbers(theirs))
        } else {
            format!(
                "a number in {unit}: {} vs {}",
                numbers(values),
                numbers(theirs)
            )
        };
        return Some(clause);
    }
    None
}

fn grouped(readings: &[(&'static str, f64)]) -> BTreeMap<&'static str, Vec<f64>> {
    let mut map: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    for (unit, value) in readings {
        map.entry(*unit).or_default().push(*value);
    }
    map
}

fn numbers(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| {
            if value.fract() == 0.0 && value.abs() < 1e15 {
                format!("{}", *value as i64)
            } else {
                format!("{value}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn bounded_answer(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ANSWER_CHARS {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(ANSWER_CHARS).collect();
    format!(
        "{}… [answer truncated at {ANSWER_CHARS} characters]",
        head.trim_end()
    )
}

async fn ask(provider: &dyn Provider, model: &str, question: &str) -> Result<String, String> {
    let request = CompletionRequest {
        model: model.to_owned(),
        messages: vec![
            Message::system("Answer the question directly in plain prose, at most six sentences."),
            Message::user(question),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(ANSWER_TOKENS),
    };
    let mut stream = provider.stream(request);
    let mut answer = String::new();
    loop {
        match tokio::time::timeout(ANSWER_BUDGET, stream.next()).await {
            Err(_) => {
                return Err(format!(
                    "no complete answer within {}s",
                    ANSWER_BUDGET.as_secs()
                ));
            }
            Ok(None) => break,
            Ok(Some(Ok(ProviderEvent::TextDelta(text)))) => answer.push_str(&text),
            Ok(Some(Ok(ProviderEvent::Finished { .. }))) => break,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => return Err(error.to_string()),
        }
    }
    let answer = answer.trim().to_owned();
    if answer.is_empty() {
        return Err("returned an empty answer".to_owned());
    }
    Ok(answer)
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, VikramError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| VikramError::EffectFailed(error.to_string()))?;
            Ok(runtime.block_on(future))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderStream;
    use vikram_contracts::VikramError;

    struct Stub {
        model: String,
        answer: String,
    }

    impl Stub {
        fn replying(model: &str, answer: &str) -> Self {
            Self {
                model: model.to_owned(),
                answer: answer.to_owned(),
            }
        }
    }

    impl Provider for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            &self.model
        }
        fn context_window(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _request: CompletionRequest) -> ProviderStream {
            let events: Vec<Result<ProviderEvent, VikramError>> = vec![
                Ok(ProviderEvent::TextDelta(self.answer.clone())),
                Ok(ProviderEvent::Finished {
                    input_tokens: None,
                    output_tokens: None,
                }),
            ];
            Box::pin(futures_util::stream::iter(events))
        }
    }

    #[test]
    fn two_models_that_diverge_are_both_shown() {
        let primary = Stub::replying("model-a", "Rust async is supported on every platform here.");
        let other = Stub::replying("model-b", "Rust async is not supported on this platform.");

        let output = compare(
            "is rust async supported?",
            &primary,
            "model-a",
            &other,
            "model-b",
        );

        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("primary (model-a):"),
            "{}",
            output.content
        );
        assert!(
            output
                .content
                .contains("is supported on every platform here"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("other (model-b):"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("is not supported on this platform"),
            "{}",
            output.content
        );
        assert!(
            output
                .content
                .contains("possible divergence: the two answers disagree about"),
            "{}",
            output.content
        );
    }

    #[test]
    fn two_models_that_agree_are_not_flagged() {
        let primary = Stub::replying("model-a", "Rust async is supported on this platform.");
        let other = Stub::replying("model-b", "Rust async is supported on this platform too.");

        let output = compare(
            "is rust async supported?",
            &primary,
            "model-a",
            &other,
            "model-b",
        );

        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("primary (model-a):"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("other (model-b):"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("no divergence detected"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("not proof of correctness"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("possible divergence"),
            "{}",
            output.content
        );
    }

    #[test]
    fn a_numeric_mismatch_in_the_same_unit_is_flagged() {
        let primary = Stub::replying("model-a", "The build takes 30 seconds on this machine.");
        let other = Stub::replying("model-b", "The build takes 45 seconds on this machine.");

        let output = compare(
            "how long does the build take?",
            &primary,
            "model-a",
            &other,
            "model-b",
        );

        assert!(output.ok, "{}", output.content);
        assert!(
            output
                .content
                .contains("possible divergence: the two answers disagree about"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("a number in seconds: 30 vs 45"),
            "{}",
            output.content
        );
    }

    #[test]
    fn a_missing_other_model_is_refused() {
        let output = CrossCheck
            .run(
                &json!({"question": "is rust async supported?"}),
                &ToolContext::default(),
            )
            .unwrap();

        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("other_model"), "{}", output.content);
        assert!(!output.content.contains("primary ("), "{}", output.content);
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry
            .get("cross_check")
            .expect("cross_check is registered");
        let spec = tool.spec();

        assert_eq!(spec.name, "cross_check");
        assert_eq!(spec.schema["type"], "object");
        assert_eq!(spec.schema["properties"]["question"]["type"], "string");
        assert_eq!(spec.schema["properties"]["other_model"]["type"], "string");
        assert_eq!(spec.schema["required"][0], "question");

        let permission = tool
            .permission(&json!({"question": "q", "other_model": "m"}))
            .unwrap();
        assert_eq!(permission.action, ActionKind::Network);
        assert_eq!(permission.verb, "cross_check");
        assert_eq!(permission.resource, "q");
        assert_eq!(permission.side_effect, SideEffect::None);
        assert_eq!(permission.originating_trust, Trust::Model);
    }
}
