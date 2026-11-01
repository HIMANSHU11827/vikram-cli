use std::path::PathBuf;

use crate::skill::TrustLevel;

pub type Result<T> = std::result::Result<T, SkillError>;

/// Every failure in this crate is typed and, where a file is involved, carries its path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("skill `{name}` not found in {sources} sources")]
    NotFound { name: String, sources: usize },

    #[error("invalid skill name `{name}`: {reason}")]
    InvalidName { name: String, reason: String },

    #[error("io error at {}: {reason}", path.display())]
    Io { path: PathBuf, reason: String },

    #[error("invalid frontmatter at {}: {reason}", path.display())]
    Frontmatter { path: PathBuf, reason: String },

    #[error("missing `{field}` at {}", path.display())]
    MissingField { path: PathBuf, field: &'static str },

    #[error("path escapes its root: {}", path.display())]
    PathEscape { path: PathBuf },

    #[error("skill `{name}` is disabled")]
    Disabled { name: String },

    #[error("skill `{name}` already exists at {}", path.display())]
    AlreadyExists { name: String, path: PathBuf },

    #[error("skill `{owner}` requires `{name}`, which is not available")]
    MissingDependency { owner: String, name: String },

    #[error("circular skill dependency: {chain}")]
    CircularDependency { chain: String },

    #[error("skill `{name}` may not run scripts at trust level {level:?}")]
    UntrustedScript { name: String, level: TrustLevel },

    #[error("{what} is {size} bytes, over the {limit} byte limit")]
    TooLarge {
        what: String,
        size: usize,
        limit: usize,
    },

    #[error("skill `{name}` has no resource `{rel}`")]
    MissingResource { name: String, rel: String },

    #[error("skill `{name}` was modified locally since install: {}", path.display())]
    UpdateConflict { name: String, path: PathBuf },

    #[error("invalid bundle at {}: {reason}", path.display())]
    InvalidBundle { path: PathBuf, reason: String },

    #[error("install failed: {0}")]
    Install(String),

    #[error("unsupported: {what}")]
    Unsupported { what: String },

    #[error("denied: {reason}")]
    Denied { reason: String },
}

impl SkillError {
    pub(crate) fn io(path: impl Into<PathBuf>, err: &std::io::Error) -> Self {
        SkillError::Io {
            path: path.into(),
            reason: err.to_string(),
        }
    }

    pub(crate) fn frontmatter(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        SkillError::Frontmatter {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// Errors that mean "the skill itself is broken", as opposed to a filesystem or
    /// permission failure. Discovery keeps broken skills in the catalog, marked invalid.
    pub fn is_content_error(&self) -> bool {
        matches!(
            self,
            SkillError::Frontmatter { .. }
                | SkillError::MissingField { .. }
                | SkillError::InvalidName { .. }
                | SkillError::TooLarge { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_name_the_path() {
        let err = SkillError::Io {
            path: PathBuf::from("/tmp/x/SKILL.md"),
            reason: "permission denied".into(),
        };
        assert_eq!(
            err.to_string(),
            "io error at /tmp/x/SKILL.md: permission denied"
        );
    }

    #[test]
    fn content_errors_are_distinguishable_from_environment_errors() {
        let bad = SkillError::Frontmatter {
            path: PathBuf::from("a"),
            reason: "oops".into(),
        };
        let io = SkillError::Io {
            path: PathBuf::from("a"),
            reason: "oops".into(),
        };
        assert!(bad.is_content_error());
        assert!(!io.is_content_error());
    }

    #[test]
    fn helper_constructors_carry_context() {
        let err = SkillError::frontmatter(PathBuf::from("/s/SKILL.md"), "duplicate key `a`");
        assert!(err.to_string().contains("duplicate key"));
        assert!(err.to_string().contains("/s/SKILL.md"));
    }
}
