//! The durable allow-list.
//!
//! An entry here is a permission a human granted once and asked to keep: "`cargo test` may run
//! without asking again". It is data, not authority. Entries are read as operator-tier rules by
//! [`vikram_policy::Policy`], so the mandatory floor still refuses them outside the workspace and
//! a saved rule can only ever end a prompt — never widen what a refusal covers.
//!
//! Entries are written in the same one-line grammar the runtime proposes when it asks
//! (`allow execute shell.run resource=cargo test *`), and [`vikram_policy::parse_amendment`] is the
//! only way in, so a hand-written line cannot claim a broader scope than an approval ever granted.

use std::path::{Path, PathBuf};
use vikram_policy::{Rule, parse_amendment};

use crate::config::{ensure_home, vikram_home};
use crate::error::{Result, RuntimeError};

const FILE: &str = "rules.json";

/// The rules in `~/.vikram/rules.json`, plus the file they came from.
#[derive(Debug)]
pub struct RuleBook {
    path: PathBuf,
    rules: Vec<Rule>,
    /// The workspace an `allow` may reach. Without it a saved grant is unchecked, so it stays
    /// `None` only for read-only views of the file.
    workspace_root: Option<PathBuf>,
    /// Set when the file could not be read or written; the reason the operator needs to see.
    error: Option<String>,
}

impl RuleBook {
    /// Read the default file. A missing file is an empty list; a corrupt one is reported through
    /// [`Self::error`] and treated as empty rather than silently rewritten.
    pub fn open() -> Self {
        Self::at(vikram_home().join(FILE))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (rules, error) = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SavedRules>(&text) {
                Ok(saved) => (saved.rules, None),
                Err(err) => (
                    Vec::new(),
                    Some(format!(
                        "{} is not readable as rules: {err}",
                        path.display()
                    )),
                ),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
            Err(err) => (
                Vec::new(),
                Some(format!("cannot read {}: {err}", path.display())),
            ),
        };
        Self {
            path,
            rules,
            workspace_root: None,
            error,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Why the file could not be read, if it could not.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn snapshot(&self) -> Vec<Rule> {
        self.rules.clone()
    }

    /// Point the book at the workspace an `allow` is confined to. Called wherever a rule is about
    /// to be written; a read-only view does not need it.
    pub fn set_workspace_root(&mut self, root: Option<PathBuf>) {
        self.workspace_root = root;
    }

    /// Save one approved amendment. Returns `Ok(false)` when the same rule was already listed.
    pub fn add_amendment(&mut self, text: &str) -> Result<bool> {
        let rule = parse_amendment(text).ok_or_else(|| {
            RuntimeError::InvalidState(format!(
                "`{text}` is not an amendment in the form the runtime proposes"
            ))
        })?;
        self.add(rule)
    }

    pub fn add(&mut self, rule: Rule) -> Result<bool> {
        if let Some(root) = &self.workspace_root {
            if vikram_policy::allow_reaches_outside(&rule, root) {
                let resource = rule.resource.to_wire();
                return Err(RuntimeError::InvalidState(format!(
                    "`{resource}` is outside the workspace ({})",
                    root.display()
                )));
            }
        }
        if self.rules.contains(&rule) {
            return Ok(false);
        }
        self.rules.push(rule);
        if let Err(err) = self.persist() {
            self.rules.pop();
            return Err(err);
        }
        Ok(true)
    }

    /// Drop the entry at `index`, in listing order.
    pub fn remove(&mut self, index: usize) -> Result<Rule> {
        if index >= self.rules.len() {
            return Err(RuntimeError::InvalidState(format!(
                "there is no allow-list entry {index}"
            )));
        }
        let rule = self.rules.remove(index);
        if let Err(err) = self.persist() {
            self.rules.insert(index, rule.clone());
            return Err(err);
        }
        Ok(rule)
    }

    fn persist(&self) -> Result<()> {
        let home = ensure_home()?;
        if self.path.parent().is_some_and(|dir| dir != home) {
            std::fs::create_dir_all(self.path.parent().expect("parent")).map_err(io_failure)?;
        }
        let saved = SavedRules {
            rules: self.rules.clone(),
        };
        let text = serde_json::to_string_pretty(&saved)
            .map_err(|e| RuntimeError::Io(format!("serialize rules: {e}")))?;
        atomic_write(&self.path, &text)?;
        Ok(())
    }
}

fn io_failure(err: std::io::Error) -> RuntimeError {
    RuntimeError::Io(err.to_string())
}

/// Write the whole file through a sibling temp name, so a crash mid-save cannot leave a half-written
/// rules file that reads back as an empty allow-list.
fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(io_failure)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        io_failure(err)
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedRules {
    rules: Vec<Rule>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust};
    use vikram_policy::{MandatoryFloor, Policy, ResourcePattern};

    fn temp_book(name: &str) -> (RuleBook, PathBuf) {
        let dir = std::env::temp_dir().join(format!("vikram-rules-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        (RuleBook::at(dir.join(FILE)), dir)
    }

    fn execute(command: &str) -> PermissionRequest {
        PermissionRequest::new(
            ActionKind::Execute,
            "shell.run",
            command,
            SideEffect::LocalMutation,
            Trust::Model,
        )
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let (book, dir) = temp_book("missing");
        assert!(book.rules().is_empty());
        assert_eq!(book.error(), None);
        assert!(!book.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_saved_amendment_survives_a_restart_and_ends_the_prompt() {
        let (mut book, dir) = temp_book("restart");
        assert!(
            book.add_amendment("allow execute shell.run resource=cargo test *")
                .expect("save")
        );
        let reloaded = RuleBook::at(book.path());
        assert_eq!(reloaded.rules().len(), 1);

        let floor = MandatoryFloor {
            workspace_root: Some("/work".into()),
            ..MandatoryFloor::default()
        };
        let defaults = vec![Rule::ask(
            ActionKind::Execute,
            ResourcePattern::Any,
            "commands run with your authority",
        )];
        let policy = Policy::with_defaults(reloaded.snapshot(), defaults, floor);
        assert!(policy.evaluate(&execute("cargo test")).is_allowed());
        assert!(
            !policy.evaluate(&execute("cargo build")).is_allowed(),
            "the list covers the command it was saved for"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_the_same_entry_twice_does_not_grow_the_file() {
        let (mut book, dir) = temp_book("dedupe");
        assert!(
            book.add_amendment("allow execute shell.run resource=cargo test *")
                .expect("first")
        );
        assert!(
            !book
                .add_amendment("allow execute shell.run resource=cargo test *")
                .expect("second"),
            "an already-listed entry reports itself"
        );
        assert_eq!(book.rules().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_line_that_outruns_the_proposal_is_not_saved() {
        let (mut book, dir) = temp_book("reject");
        for text in [
            "allow execute shell.run resource=*",
            "allow execute shell.run resource=cargo test && rm *",
            "grant me everything",
        ] {
            assert!(
                book.add_amendment(text).is_err(),
                "{text} must not be saved"
            );
        }
        assert!(book.rules().is_empty());
        assert!(
            !book.path().exists(),
            "a refused save must not touch the file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_allow_that_reaches_outside_the_workspace_is_not_saved() {
        let (mut book, dir) = temp_book("containment");
        book.set_workspace_root(Some(PathBuf::from("/work/proj")));
        assert!(
            book.add_amendment("allow write fs.write resource=/work/proj/src/**")
                .expect("a path inside the workspace is the ordinary case")
        );
        for text in [
            "allow write fs.write resource=/home/other/.ssh/**",
            "allow write fs.write resource=/work/proj/../elsewhere/**",
            "allow write fs.write resource=/etc/**",
        ] {
            assert!(book.add_amendment(text).is_err(), "{text} must not be kept");
        }
        assert_eq!(book.rules().len(), 1, "only the contained entry survived");
        // A refusal is not a widening, so it stays allowed even for a path outside the workspace.
        assert!(
            book.add_amendment("deny write fs.write resource=/home/other/.ssh/**")
                .expect("a deny is not a grant")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removing_an_entry_rewrites_the_file() {
        let (mut book, dir) = temp_book("remove");
        book.add_amendment("allow execute shell.run resource=cargo test *")
            .expect("save");
        book.add_amendment("allow write fs.write resource=/work/**")
            .expect("save");
        let removed = book.remove(0).expect("remove");
        assert_eq!(removed.verb_prefix.as_deref(), Some("shell.run"));
        assert_eq!(book.rules().len(), 1);
        assert_eq!(RuleBook::at(book.path()).rules().len(), 1);
        assert!(book.remove(4).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_file_is_reported_and_kept_until_a_successful_write() {
        let (mut book, dir) = temp_book("corrupt");
        book.add_amendment("allow execute shell.run resource=cargo test *")
            .expect("save");
        std::fs::write(book.path(), "not json at all").expect("write garbage");
        let reloaded = RuleBook::at(book.path());
        assert!(reloaded.error().is_some(), "the operator is told");
        assert!(reloaded.rules().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
