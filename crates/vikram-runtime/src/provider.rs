//! The model seam.
//!
//! Requests carry [`vikram_contracts::Message`]s with their trust intact, so a provider cannot
//! silently strip provenance. A provider only translates; the kernel still decides whether a tool
//! call may run.

use futures_util::Stream;
use serde_json::Value;
use std::pin::Pin;
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
