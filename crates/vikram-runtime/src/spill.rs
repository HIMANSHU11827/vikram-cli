//! Retrievable spillover for tool output too large to carry in context.
//!
//! An oversized result is digested before it is journaled, so the transcript stays inside the
//! budget. The bytes are not thrown away: the full text is written under the session's own
//! directory and the digest names the handle, so `output.read` can hand back any slice of it. The
//! store is per-session by construction — a handle from another session's directory is not
//! addressable, and the id charset makes traversal impossible.

use std::path::{Path, PathBuf};
use vikram_contracts::SessionId;

/// Most characters one read returns, so paging through a log cannot itself blow the budget.
pub const SPILL_READ_LIMIT: usize = 12_000;

#[derive(Clone, Debug, Default)]
pub struct Spill {
    dir: Option<PathBuf>,
}

impl Spill {
    /// Stores nothing. A tool run outside a session (a direct CLI call) has no place to spool to.
    pub fn disabled() -> Self {
        Self { dir: None }
    }

    pub fn for_session(home: &Path, session: &SessionId) -> Self {
        Self {
            dir: Some(
                home.join("sessions")
                    .join(session.as_str())
                    .join("spillover"),
            ),
        }
    }

    pub fn enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// The handle is only ever a short opaque token; anything else is refused rather than joined.
    fn file_name(id: &str) -> Option<String> {
        let clean: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if clean.is_empty() || clean.len() != id.len() || clean.len() > 80 {
            return None;
        }
        Some(format!("{clean}.txt"))
    }

    /// Write the full text and return the handle the digest should advertise.
    pub fn write(&self, id: &str, text: &str) -> Option<String> {
        let dir = self.dir.as_ref()?;
        let name = Self::file_name(id)?;
        std::fs::create_dir_all(dir).ok()?;
        std::fs::write(dir.join(&name), text).ok()?;
        Some(name.trim_end_matches(".txt").to_string())
    }

    /// A character slice of a spooled output, addressed by the handle in its digest.
    pub fn read(&self, id: &str, offset: usize, limit: usize) -> Result<String, String> {
        let dir = self
            .dir
            .as_ref()
            .ok_or("no spillover store for this session")?;
        let name = Self::file_name(id).ok_or("not a valid output handle")?;
        let text = std::fs::read_to_string(dir.join(&name))
            .map_err(|_| format!("no spooled output under `{id}`"))?;
        let total = text.chars().count();
        let take = limit.clamp(1, SPILL_READ_LIMIT);
        let window: String = text.chars().skip(offset).take(take).collect();
        let end = offset + window.chars().count();
        if window.is_empty() {
            return Ok(format!(
                "output {id}: {total} characters, nothing at offset {offset}"
            ));
        }
        Ok(format!(
            "output {id}: characters {offset}..{end} of {total}\n{window}"
        ))
    }
}

/// The in-context form of an oversized result: both ends of the text, and the handle for the middle.
pub fn pointer(id: &str, omitted_chars: usize) -> String {
    format!(
        "\n\n[{omitted_chars} characters elided from this result; full text spooled as output `{id}` \u{2014} read it with output.read(id=\"{id}\", offset, limit)]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionId {
        SessionId::generate()
    }

    #[test]
    fn a_spooled_output_is_readable_by_its_handle() {
        let home = tempfile::tempdir().unwrap();
        let id = session();
        let spill = Spill::for_session(home.path(), &id);
        let full = format!("HEAD{}TAIL", "x".repeat(5_000));
        let handle = spill.write("call-1", &full).unwrap();
        let read = spill.read(&handle, 4, 10).unwrap();
        assert!(
            read.contains(&format!("characters 4..14 of {}", full.chars().count())),
            "{read}"
        );
        assert!(read.ends_with(&full[4..14]));
        let tail = spill.read(&handle, full.chars().count() - 4, 100).unwrap();
        assert!(tail.ends_with("TAIL"));
    }

    #[test]
    fn another_session_cannot_address_the_spool() {
        let home = tempfile::tempdir().unwrap();
        let owner = session();
        let other = session();
        let handle = Spill::for_session(home.path(), &owner)
            .write("call-9", "secret output")
            .unwrap();
        let err = Spill::for_session(home.path(), &other)
            .read(&handle, 0, 100)
            .unwrap_err();
        assert!(err.contains("no spooled output"), "{err}");
    }

    #[test]
    fn a_handle_cannot_climb_out_of_the_spool() {
        let home = tempfile::tempdir().unwrap();
        let id = session();
        let spill = Spill::for_session(home.path(), &id);
        for escape in [
            "../../config",
            "../sessions",
            "/etc/passwd",
            "a/b",
            "..",
            "",
        ] {
            assert!(spill.write(escape, "x").is_none(), "{escape} was accepted");
        }
        assert!(spill.read("../../config", 0, 10).is_err());
    }

    #[test]
    fn a_disabled_spill_stores_and_serves_nothing() {
        let spill = Spill::disabled();
        assert!(!spill.enabled());
        assert!(spill.write("call-1", "text").is_none());
        assert!(spill.read("call-1", 0, 10).is_err());
    }
}
