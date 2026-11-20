//! The model seam.
//!
//! Requests carry [`vikram_contracts::Message`]s with their trust intact, so a provider cannot
//! silently strip provenance. A provider only translates; the kernel still decides whether a tool
//! call may run.

use futures_util::Stream;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use vikram_contracts::{Message, ToolCall, VikramError};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools,
            max_output_tokens: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderEvent {
    TextDelta(String),
    /// A thinking/reasoning delta exposed by the provider (only when permitted by the model).
    ThinkingDelta(String),
    /// A complete tool call, emitted by the provider once its arguments are fully streamed.
    ToolCall(ToolCall),
    /// The endpoint refused the request and it is being sent again. `status` is the HTTP code that
    /// caused it, or `0` when the connection never got that far. Reported so an interface can say
    /// why the clock is moving: without it, a backoff is indistinguishable from a hung turn.
    Retrying {
        attempt: u32,
        of: u32,
        wait_ms: u64,
        status: u16,
    },
    Finished {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
}

pub type ProviderStream =
    Pin<Box<dyn Stream<Item = std::result::Result<ProviderEvent, VikramError>> + Send>>;

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    /// Only from configuration. A provider that does not know must return `None`; inventing a
    /// window would corrupt the context metric.
    fn context_window(&self) -> Option<u64>;
    fn stream(&self, request: CompletionRequest) -> ProviderStream;
}

/// Build the provider `config.provider` names. An unknown kind is an error, not a silent
/// default: a typo in `provider` must fail at startup rather than bill the wrong endpoint.
/// When `config.fallback_model` is set, the result wraps the primary in a [`FallbackProvider`]
/// that retries the same request against that model before any output has been produced.
pub fn for_config(config: &crate::config::Config) -> crate::Result<Option<Arc<dyn Provider>>> {
    let Some(primary) = build(config)? else {
        return Ok(None);
    };
    let fallback_model = config.fallback_model.trim();
    if fallback_model.is_empty() {
        return Ok(Some(primary));
    }
    let mut fallback_config = config.clone();
    fallback_config.model = fallback_model.to_owned();
    match build(&fallback_config)? {
        Some(fallback) => Ok(Some(Arc::new(FallbackProvider::new(primary, fallback)))),
        // Same endpoint, key and kind as the primary, so a non-empty fallback model builds;
        // if it somehow did not, the primary still answers rather than failing the boot.
        None => Ok(Some(primary)),
    }
}

fn build(config: &crate::config::Config) -> crate::Result<Option<Arc<dyn Provider>>> {
    match config.provider.trim() {
        "openai" | "" => Ok(crate::openai::OpenAiProvider::from_config(config)?
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)),
        "anthropic" => Ok(crate::anthropic::AnthropicProvider::from_config(config)?
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)),
        other => Err(crate::RuntimeError::Config {
            path: crate::config::config_path(),
            reason: format!("unknown provider `{other}`; expected `openai` or `anthropic`"),
        }),
    }
}

/// Two providers on the same endpoint: the fallback is tried only when the primary's stream
/// fails *before yielding any event*. Once output has started, an error propagates — replaying
/// a half-written answer under a different model would corrupt the transcript.
pub struct FallbackProvider {
    primary: Arc<dyn Provider>,
    fallback: Arc<dyn Provider>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn Provider>, fallback: Arc<dyn Provider>) -> Self {
        Self { primary, fallback }
    }
}

impl Provider for FallbackProvider {
    fn name(&self) -> &str {
        self.primary.name()
    }

    fn model(&self) -> &str {
        self.primary.model()
    }

    fn context_window(&self) -> Option<u64> {
        self.primary.context_window()
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream {
        let mut fallback_request = request.clone();
        fallback_request.model = self.fallback.model().to_owned();
        Box::pin(FallbackStream {
            current: self.primary.stream(request),
            pending: Some((self.fallback.clone(), fallback_request)),
            started: false,
        })
    }
}

struct FallbackStream {
    current: ProviderStream,
    /// Held until needed: `Provider::stream` spawns the request eagerly, so building the
    /// fallback up front would send it twice.
    pending: Option<(Arc<dyn Provider>, CompletionRequest)>,
    started: bool,
}

impl Stream for FallbackStream {
    type Item = std::result::Result<ProviderEvent, VikramError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.current.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    self.started = true;
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(Some(Err(err))) => {
                    if self.started {
                        return Poll::Ready(Some(Err(err)));
                    }
                    let Some((provider, request)) = self.pending.take() else {
                        return Poll::Ready(Some(Err(err)));
                    };
                    self.current = provider.stream(request);
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    struct MockProvider {
        model: String,
        events: Vec<std::result::Result<ProviderEvent, VikramError>>,
        seen_model: std::sync::Mutex<Vec<String>>,
    }

    impl MockProvider {
        fn new(model: &str, events: Vec<std::result::Result<ProviderEvent, VikramError>>) -> Self {
            Self {
                model: model.to_owned(),
                events,
                seen_model: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            &self.model
        }
        fn context_window(&self) -> Option<u64> {
            None
        }
        fn stream(&self, request: CompletionRequest) -> ProviderStream {
            if let Ok(mut seen) = self.seen_model.lock() {
                seen.push(request.model.clone());
            }
            let events = self.events.clone();
            Box::pin(futures_util::stream::iter(events))
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest::new("primary-model", vec![], vec![])
    }

    fn boom() -> VikramError {
        VikramError::ProviderRejected("endpoint said no".into())
    }

    #[tokio::test]
    async fn fallback_takes_over_when_the_primary_fails_before_output() {
        let primary = Arc::new(MockProvider::new("primary-model", vec![Err(boom())]));
        let fallback = Arc::new(MockProvider::new(
            "fallback-model",
            vec![
                Ok(ProviderEvent::TextDelta("hi".into())),
                Ok(ProviderEvent::Finished {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                }),
            ],
        ));
        let provider = FallbackProvider::new(primary.clone(), fallback.clone());
        let events: Vec<ProviderEvent> = provider
            .stream(request())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(provider.model(), "primary-model");
        assert_eq!(
            fallback.seen_model.lock().unwrap().as_slice(),
            &["fallback-model".to_owned()],
            "the fallback request must name the fallback model, not the primary's"
        );
    }

    #[tokio::test]
    async fn fallback_does_not_replay_once_output_started() {
        let primary = Arc::new(MockProvider::new(
            "primary-model",
            vec![Ok(ProviderEvent::TextDelta("partial".into())), Err(boom())],
        ));
        let fallback = Arc::new(MockProvider::new("fallback-model", vec![]));
        let provider = FallbackProvider::new(primary, fallback.clone());
        let mut stream = provider.stream(request());
        assert!(matches!(
            stream.next().await,
            Some(Ok(ProviderEvent::TextDelta(_)))
        ));
        assert!(matches!(stream.next().await, Some(Err(_))));
        assert!(
            fallback.seen_model.lock().unwrap().is_empty(),
            "a half-written answer must not be replayed under another model"
        );
    }

    #[test]
    fn for_config_rejects_an_unknown_provider_kind() {
        let config = crate::config::Config {
            provider: "not-a-provider".into(),
            ..Default::default()
        };
        let err = for_config(&config)
            .err()
            .expect("an unknown kind must fail loudly");
        assert!(err.to_string().contains("not-a-provider"));
    }

    #[test]
    fn for_config_returns_none_until_a_model_and_key_exist() {
        let config = crate::config::Config::default();
        assert!(for_config(&config).unwrap().is_none());
    }

    #[test]
    fn for_config_wraps_the_fallback_model_when_one_is_configured() {
        unsafe { std::env::set_var("VIKRAM_PROVIDER_TEST_KEY", "test-key") };
        let config = crate::config::Config {
            model: "primary-model".into(),
            api_key_env: "VIKRAM_PROVIDER_TEST_KEY".into(),
            fallback_model: "fallback-model".into(),
            base_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        };
        let provider = for_config(&config)
            .unwrap()
            .expect("a configured model builds");
        assert_eq!(provider.model(), "primary-model");
        unsafe { std::env::remove_var("VIKRAM_PROVIDER_TEST_KEY") };
    }
}
