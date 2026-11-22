use crate::{Result, VoiceError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation shared between the voice loop and every audio stage.
///
/// Stages poll the token between buffers rather than blocking uninterruptibly, so a cancel
/// lands within one poll interval, not at the end of a full recording or inference.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(VoiceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_token_fails_the_check_and_reset_clears_it() {
        let token = CancelToken::new();
        token.check().expect("fresh token is not cancelled");
        token.cancel();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(VoiceError::Cancelled)));
        token.reset();
        token.check().expect("reset token is usable again");
    }

    #[test]
    fn clones_share_the_same_flag() {
        let token = CancelToken::new();
        let twin = token.clone();
        twin.cancel();
        assert!(token.is_cancelled());
    }
}
