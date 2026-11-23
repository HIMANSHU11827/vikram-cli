//! The live settings `vikram-evolution` may change, offered to it as a `ConfigSurface`.
//!
//! The engine lives in a crate that cannot see this one, so it asks by key and this module answers.
//! Only keys about *how the assistant learns* are writable. Anything that decides what it is
//! allowed to do — tools, permissions, providers, the model — is refused here, which is why the
//! allowlist is a literal list rather than a prefix rule.

use std::sync::Arc;

use vikram_evolution::self_config::ConfigSurface;

use crate::learning::LearningPolicy;
use crate::runtime::RuntimeInner;

/// Keys this runtime will let the learning engine write.
pub(crate) const WRITABLE: &[&str] = &[
    "context:memory_budget_tokens",
    "runtime:learning_memory_policy",
    "runtime:learning_skill_policy",
];

const MEMORY_BUDGET: &str = "context:memory_budget_tokens";
const MEMORY_POLICY: &str = "runtime:learning_memory_policy";
const SKILL_POLICY: &str = "runtime:learning_skill_policy";
const MIN_MEMORY_BUDGET: u64 = 200;

pub(crate) struct RuntimeConfigSurface {
    inner: Arc<RuntimeInner>,
}

impl RuntimeConfigSurface {
    pub(crate) fn new(inner: Arc<RuntimeInner>) -> Self {
        Self { inner }
    }
}

impl ConfigSurface for RuntimeConfigSurface {
    fn read(&self, key: &str) -> Option<String> {
        match key {
            MEMORY_BUDGET => Some(self.config().context.memory_budget_tokens.to_string()),
            MEMORY_POLICY => Some(
                self.inner
                    .learning
                    .surface
                    .policy_memory()
                    .as_str()
                    .to_owned(),
            ),
            SKILL_POLICY => Some(
                self.inner
                    .learning
                    .surface
                    .policy_skill()
                    .as_str()
                    .to_owned(),
            ),
            _ => None,
        }
    }

    fn write(&self, key: &str, value: &str) -> Result<(), String> {
        match key {
            MEMORY_BUDGET => {
                let budget: u64 = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("`{value}` is not a token budget"))?;
                if budget < MIN_MEMORY_BUDGET {
                    return Err(format!(
                        "a memory budget under {MIN_MEMORY_BUDGET} tokens leaves no room for a single recalled entry"
                    ));
                }
                let mut config = self.inner.config.write().unwrap_or_else(|e| e.into_inner());
                config.context.memory_budget_tokens = budget;
                config.save().map_err(|e| e.to_string())
            }
            MEMORY_POLICY | SKILL_POLICY => {
                let policy = LearningPolicy::parse(value);
                if policy.as_str() != value.trim() {
                    return Err(format!(
                        "`{value}` is not a learning policy; expected auto, approval, suggest or disabled"
                    ));
                }
                if key == MEMORY_POLICY {
                    self.inner.learning.surface.set_policy_memory(policy);
                } else {
                    self.inner.learning.surface.set_policy_skill(policy);
                }
                self.inner.learning.surface.save();
                Ok(())
            }
            other => Err(format!(
                "`{other}` is not a setting this runtime exposes to the learning engine"
            )),
        }
    }

    fn writable(&self, key: &str) -> bool {
        WRITABLE.contains(&key)
    }
}

impl RuntimeConfigSurface {
    fn config(&self) -> crate::config::Config {
        self.inner
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
        guard
    }

    #[allow(clippy::await_holding_lock)]
    async fn surface() -> (RuntimeConfigSurface, Arc<RuntimeInner>) {
        let dir = tempfile::tempdir().unwrap();
        let _home = isolated_home(dir.path());
        let runtime = crate::Runtime::start(Config {
            model: "test-model".into(),
            workspace_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        })
        .await
        .expect("runtime starts");
        let inner = runtime.handle().inner.clone();
        (RuntimeConfigSurface::new(inner.clone()), inner)
    }

    #[tokio::test]
    async fn the_allowlist_is_exactly_the_learning_settings() {
        let (surface, _inner) = surface().await;
        for key in WRITABLE {
            assert!(surface.writable(key), "{key}");
        }
        assert!(!surface.writable("tool:shell"));
        assert!(!surface.writable("model:gpt-4"));
        assert!(!surface.writable("provider:openai"));
    }

    #[tokio::test]
    async fn a_memory_budget_is_written_and_read_back() {
        let (surface, inner) = surface().await;
        surface.write(MEMORY_BUDGET, "2400").unwrap();
        assert_eq!(
            inner.config.read().unwrap().context.memory_budget_tokens,
            2400
        );
        assert_eq!(surface.read(MEMORY_BUDGET).as_deref(), Some("2400"));
    }

    #[tokio::test]
    async fn a_budget_too_small_to_hold_an_entry_is_refused() {
        let (surface, inner) = surface().await;
        let before = inner.config.read().unwrap().context.memory_budget_tokens;
        let err = surface.write(MEMORY_BUDGET, "50").unwrap_err();
        assert!(err.contains("no room"), "{err}");
        assert_eq!(
            inner.config.read().unwrap().context.memory_budget_tokens,
            before
        );
    }

    #[tokio::test]
    async fn a_policy_is_written_and_read_back() {
        let (surface, inner) = surface().await;
        surface.write(MEMORY_POLICY, "auto").unwrap();
        assert_eq!(inner.learning.surface.policy_memory(), LearningPolicy::Auto);
        assert_eq!(surface.read(MEMORY_POLICY).as_deref(), Some("auto"));
    }

    #[tokio::test]
    async fn nonsense_in_a_policy_is_refused() {
        let (surface, _inner) = surface().await;
        let err = surface.write(SKILL_POLICY, "sometimes").unwrap_err();
        assert!(err.contains("not a learning policy"), "{err}");
    }
}
