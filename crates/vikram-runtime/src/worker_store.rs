//! Durable worker records.
//!
//! A worker is a live session plus a record of what it is doing. The session already lives in the
//! store; this module makes the record live there too, so a restart can list — and therefore
//! resume — the workers a crash left partial. The row is a projection: `session`, `parent` and
//! `status` are columns for listing, `record` is the serialized [`WorkerRecord`] the registry
//! reconstructs.

use vikram_store::{Store, WorkerRow};

use crate::error::{Result, RuntimeError};
use crate::orchestrator::WorkerRecord;
use crate::types::now_ms;

/// Write a record, replacing the row a previous version of it wrote.
pub(crate) fn save(store: &Store, record: &WorkerRecord) -> Result<()> {
    let encoded = serde_json::to_string(record)
        .map_err(|err| RuntimeError::Store(format!("worker record could not be encoded: {err}")))?;
    let now = now_ms();
    store.upsert_worker(&WorkerRow {
        id: record.id.as_str().to_owned(),
        session: record.session.as_str().to_owned(),
        parent: record.parent.as_str().to_owned(),
        status: record.status.as_str().to_owned(),
        attempts: record.attempts,
        record: encoded,
        created_ms: now,
        updated_ms: now,
    })?;
    Ok(())
}

/// Every durable worker, in spawn order.
#[allow(
    dead_code,
    reason = "reachable from RuntimeInner::load_workers, wired to boot in runtime.rs"
)]
pub(crate) fn load(store: &Store) -> Result<Vec<WorkerRecord>> {
    store
        .list_workers()?
        .into_iter()
        .map(|row| {
            serde_json::from_str(&row.record).map_err(|err| {
                RuntimeError::Store(format!("worker record `{}` is unreadable: {err}", row.id))
            })
        })
        .collect()
}

/// `CapabilityScope` lives in `vikram-policy`, which has no serde impl of its own; a worker record
/// still has to carry its envelope, so the record serializes it field for field.
pub(crate) mod scope_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use vikram_policy::CapabilityScope;

    #[derive(Serialize, Deserialize)]
    struct ScopeJson {
        tools: Option<Vec<String>>,
        file_scope: Option<Vec<String>>,
    }

    pub fn serialize<S: Serializer>(
        scope: &CapabilityScope,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        ScopeJson {
            tools: scope.tools.clone(),
            file_scope: scope.file_scope.clone(),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<CapabilityScope, D::Error> {
        let scope = ScopeJson::deserialize(deserializer)?;
        Ok(CapabilityScope {
            tools: scope.tools,
            file_scope: scope.file_scope,
        })
    }
}
