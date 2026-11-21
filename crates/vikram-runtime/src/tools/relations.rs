use serde_json::{Value, json};
use vikram_contracts::{
    ActionKind, PermissionRequest, ProfileId, SideEffect, Trust, UserId, VikramError,
};
use vikram_store::{RelationRow, Store};

use crate::config::database_path;

use crate::tools::*;

fn open_store() -> Result<Store, VikramError> {
    Store::open(&database_path()).map_err(|e| VikramError::Storage(e.to_string()))
}

/// The (user, profile) every relation call is confined to. There is deliberately no `user` or
/// `profile` argument: a call can only ever name the active identity, so a session cannot reach
/// another user's table by asking for it. Missing identity denies the call outright.
fn scope(store: &Store) -> Result<(UserId, ProfileId), VikramError> {
    let identity = store
        .get_active_identity()
        .map_err(|e| VikramError::Storage(e.to_string()))?;
    match (identity.user, identity.profile) {
        (Some(user), Some(profile)) => Ok((user, profile)),
        _ => Err(VikramError::PolicyDenied {
            reason: "relations need an active user and profile".into(),
        }),
    }
}

fn render(row: &RelationRow) -> String {
    let mut line = format!("{} ({}) — closeness {}", row.name, row.kind, row.closeness);
    if !row.aliases.is_empty() {
        line.push_str(&format!(", aka {}", row.aliases.join(", ")));
    }
    if !row.notes.is_empty() {
        line.push_str(&format!(", notes: {}", row.notes));
    }
    line
}

fn aliases_arg(args: &Value) -> Option<Vec<String>> {
    args.get("aliases").and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

fn closeness_arg(args: &Value) -> Option<i32> {
    args.get("closeness")
        .and_then(Value::as_i64)
        .map(|v| v.clamp(-100, 100) as i32)
}

pub(crate) struct Relation;

impl Tool for Relation {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "relation".into(),
            description: "Manage the people, projects, places and pets the user has a standing relationship with. Actions: list (all known relations), get (one by kind+name), save (create or refresh; optional aliases and closeness -100..100), delete (by kind+name).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "save", "delete"], "description": "default list"},
                    "kind": {"type": "string", "description": "person, project, place, org, pet, or any short label (get/save/delete)"},
                    "name": {"type": "string", "description": "canonical name; matched case-insensitively (get/save/delete)"},
                    "aliases": {"type": "array", "items": {"type": "string"}, "description": "other names this relation answers to (save)"},
                    "closeness": {"type": "integer", "description": "-100 (adversarial) to 100 (immediate family); clamped (save)"}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let (kind, side_effect) = match action {
            "save" => (ActionKind::Write, SideEffect::LocalMutation),
            "delete" => (ActionKind::Write, SideEffect::Destructive),
            _ => (ActionKind::Read, SideEffect::None),
        };
        let target = match (
            args.get("kind").and_then(Value::as_str),
            args.get("name").and_then(Value::as_str),
        ) {
            (Some(kind), Some(name)) => format!("relation:{kind}:{name}"),
            _ => "relations".to_owned(),
        };
        Ok(PermissionRequest::new(
            kind,
            "relation",
            target,
            side_effect,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let store = open_store()?;
        let (user, profile) = scope(&store)?;
        let now = crate::types::now_ms();

        match action {
            "list" => {
                let rows = store
                    .list_relations(&user, &profile)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                if rows.is_empty() {
                    return Ok(ToolOutput {
                        content: "no known relations".into(),
                        ok: true,
                    });
                }
                let body = rows.iter().map(render).collect::<Vec<_>>().join("\n");
                Ok(ToolOutput {
                    content: truncated(body),
                    ok: true,
                })
            }
            "get" => {
                let kind = arg_str(args, "kind")?;
                let name = arg_str(args, "name")?;
                let row = store
                    .get_relation(&user, &profile, &kind, &name)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                match row {
                    Some(row) => Ok(ToolOutput {
                        content: format!(
                            "{}\nid: {}\nlast seen: {}",
                            render(&row),
                            row.id.as_str(),
                            row.last_seen_ms
                                .map(|ts| ts.to_string())
                                .unwrap_or_else(|| "never".into())
                        ),
                        ok: true,
                    }),
                    None => Ok(ToolOutput {
                        content: format!("no relation `{kind}/{name}`"),
                        ok: false,
                    }),
                }
            }
            "save" => {
                let kind = arg_str(args, "kind")?;
                let name = arg_str(args, "name")?;
                let row = store
                    .upsert_relation(&user, &profile, &kind, &name, now)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                if let Some(aliases) = aliases_arg(args) {
                    store
                        .set_relation_aliases(&row.id, &aliases, now)
                        .map_err(|e| VikramError::Storage(e.to_string()))?;
                }
                if let Some(closeness) = closeness_arg(args) {
                    store
                        .set_relation_closeness(&row.id, closeness, now)
                        .map_err(|e| VikramError::Storage(e.to_string()))?;
                }
                let row = store
                    .get_relation(&user, &profile, &kind, &name)
                    .map_err(|e| VikramError::Storage(e.to_string()))?
                    .unwrap_or(row);
                Ok(ToolOutput {
                    content: format!("saved relation `{}`", render(&row)),
                    ok: true,
                })
            }
            "delete" => {
                let kind = arg_str(args, "kind")?;
                let name = arg_str(args, "name")?;
                let row = store
                    .get_relation(&user, &profile, &kind, &name)
                    .map_err(|e| VikramError::Storage(e.to_string()))?;
                match row {
                    Some(row) => {
                        store
                            .delete_relation(&row.id)
                            .map_err(|e| VikramError::Storage(e.to_string()))?;
                        Ok(ToolOutput {
                            content: format!("deleted relation `{}`", render(&row)),
                            ok: true,
                        })
                    }
                    None => Ok(ToolOutput {
                        content: format!("no relation `{kind}/{name}`"),
                        ok: false,
                    }),
                }
            }
            other => Err(VikramError::ProviderRejected(format!(
                "unknown relation action `{other}`"
            ))),
        }
    }
}
