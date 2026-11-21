//! The WebAssembly plugin tier.
//!
//! A `.wasm` module declared under `components.wasm` (or as the manifest `entry` when it carries
//! the `.wasm` extension) runs inside the same permission envelope as a process plugin: the
//! manifest must declare an execution permission (`process` or `network`) and the entry must
//! resolve strictly inside the plugin directory. Execution is in-process through wasmi — no
//! subprocess, no WASI, no host imports: a module that imports anything fails to instantiate.
//!
//! ABI (v1, JSON-over-memory):
//! - the module exports `memory`, `alloc(size: i32) -> i32`, `dealloc(ptr: i32, size: i32)` and
//!   `handle(ptr: i32, len: i32) -> i64`;
//! - the host allocates `len` bytes via `alloc`, writes the request JSON, calls `handle` and
//!   unpacks the returned `i64` as `(out_ptr << 32) | out_len` pointing at the response JSON in
//!   linear memory; both buffers are released through `dealloc`;
//! - the response is a [`crate::hooks::HookEffect`]-shaped JSON object — the same reply a
//!   process plugin writes to stdout.
//!
//! Execution is bounded: the module is compiled once per plugin and every call runs under a
//! fresh store with a fuel budget ([`WASM_FUEL_LIMIT`]) plus the recursion and stack limits
//! wasmi enforces by default.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use wasmi::{Engine, Linker, Module, Store};

use crate::plugins::{PluginManifest, ensure_execution_permitted, entry_path};

/// The largest serialized request a wasm plugin is sent — the same ceiling process plugins get.
const MAX_WASM_PAYLOAD: usize = 256 * 1024;

/// Fuel budget for one `handle` invocation. A module that loops forever traps with
/// `OutOfFuel` instead of hanging the turn.
const WASM_FUEL_LIMIT: u64 = 50_000_000;

/// Errors a wasm plugin call can produce. Every variant names the plugin so registry and stage
/// diagnostics attribute the failure correctly.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("plugin `{plugin}` wasm entry is not a .wasm module")]
    NotWasm { plugin: String },
    #[error("plugin `{plugin}` wasm module failed to compile: {reason}")]
    Compile { plugin: String, reason: String },
    #[error("plugin `{plugin}` wasm module does not export `{export}`")]
    MissingExport {
        plugin: String,
        export: &'static str,
    },
    #[error("plugin `{plugin}` wasm module returned an invalid pointer/length pair")]
    BadReply { plugin: String },
    #[error(
        "plugin `{plugin}` wasm payload is {size} bytes, over the {MAX_WASM_PAYLOAD}-byte limit"
    )]
    PayloadTooLarge { plugin: String, size: usize },
    #[error("plugin `{plugin}` wasm execution failed: {reason}")]
    Execution { plugin: String, reason: String },
    #[error("plugin `{plugin}` wasm reply is not valid JSON: {reason}")]
    Reply { plugin: String, reason: String },
}

/// A compiled wasm module plus the engine that produced it.
struct CompiledModule {
    engine: Engine,
    module: Module,
}

/// A wasm plugin: its manifest-gated entry, compiled lazily on first call.
///
/// `WasmPlugin` is `Send + Sync`: the compiled module is immutable and every call instantiates a
/// fresh store, so concurrent dispatches cannot share mutable wasm state.
pub struct WasmPlugin {
    plugin: String,
    path: PathBuf,
    compiled: OnceLock<Result<CompiledModule, String>>,
}

impl WasmPlugin {
    /// Build a wasm plugin handle for `entry` inside `dir`, gated by `manifest` permissions.
    ///
    /// The entry must resolve inside the plugin directory like any other entry point, the
    /// manifest must grant an execution permission, and the file must carry the wasm magic.
    pub fn new(
        plugin: impl Into<String>,
        dir: impl Into<PathBuf>,
        entry: &str,
        manifest: &PluginManifest,
    ) -> Result<Self, String> {
        let plugin = plugin.into();
        ensure_execution_permitted(&plugin, manifest)?;
        let dir = dir.into();
        let path =
            entry_path(&dir, entry).map_err(|reason| format!("plugin `{plugin}`: {reason}"))?;
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
        {
            return Err(format!(
                "plugin `{plugin}`: wasm entry `{entry}` must carry the .wasm extension"
            ));
        }
        Ok(Self {
            plugin,
            path,
            compiled: OnceLock::new(),
        })
    }

    /// The wasm entries a manifest declares: `components.wasm` items, or `entry` itself when it
    /// carries the `.wasm` extension. Process entries are never treated as wasm.
    pub fn declared_entries(manifest: &PluginManifest) -> Vec<String> {
        if let Some(entries) = manifest
            .components
            .as_ref()
            .and_then(|components| components.get("wasm"))
        {
            return entries.clone();
        }
        let entry = manifest.entry.trim();
        if entry.to_ascii_lowercase().ends_with(".wasm") {
            vec![entry.to_owned()]
        } else {
            Vec::new()
        }
    }

    /// Send `request` through the ABI and return the parsed JSON reply.
    pub fn request(&self, request: &Value) -> Result<Value, WasmError> {
        let payload = request.to_string();
        if payload.len() > MAX_WASM_PAYLOAD {
            return Err(WasmError::PayloadTooLarge {
                plugin: self.plugin.clone(),
                size: payload.len(),
            });
        }
        let reply = self.call(&payload)?;
        let value: Value =
            serde_json::from_str(reply.trim()).map_err(|reason| WasmError::Reply {
                plugin: self.plugin.clone(),
                reason: reason.to_string(),
            })?;
        if !value.is_object() {
            return Err(WasmError::Reply {
                plugin: self.plugin.clone(),
                reason: "reply is not a JSON object".to_owned(),
            });
        }
        Ok(value)
    }

    /// Compile the module eagerly so a broken guest is refused at load time. The result is
    /// cached in the same slot [`Self::request`] uses, so the first call does not compile twice.
    pub fn validate(&self) -> Result<(), WasmError> {
        self.compiled
            .get_or_init(|| self.compile())
            .as_ref()
            .map(|_| ())
            .map_err(|reason| WasmError::Compile {
                plugin: self.plugin.clone(),
                reason: reason.clone(),
            })
    }

    /// Validate a wasm file without executing it: magic bytes plus a full wasmi compile.
    pub fn validate_file(path: &Path) -> Result<(), WasmError> {
        let bytes = std::fs::read(path).map_err(|reason| WasmError::Execution {
            plugin: path.display().to_string(),
            reason: format!("cannot read module: {reason}"),
        })?;
        Self::validate_bytes(&bytes).map_err(|reason| WasmError::Compile {
            plugin: path.display().to_string(),
            reason,
        })
    }

    /// Compile-check raw module bytes (magic plus wasmi validation) without instantiating.
    pub fn validate_bytes(bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
            return Err("not a WebAssembly module (bad magic)".to_owned());
        }
        let engine = Engine::default();
        Module::new(&engine, bytes)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn compile(&self) -> Result<CompiledModule, String> {
        let bytes =
            std::fs::read(&self.path).map_err(|reason| format!("cannot read module: {reason}"))?;
        Self::validate_bytes(&bytes)?;
        let mut config = wasmi::Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &bytes).map_err(|err| err.to_string())?;
        Ok(CompiledModule { engine, module })
    }

    /// Instantiate the module in a fresh fuel-metered store and run one `handle` round-trip.
    fn call(&self, payload: &str) -> Result<String, WasmError> {
        let compiled = self
            .compiled
            .get_or_init(|| self.compile())
            .as_ref()
            .map_err(|reason| WasmError::Compile {
                plugin: self.plugin.clone(),
                reason: reason.clone(),
            })?;
        let mut store = Store::new(&compiled.engine, ());
        store
            .set_fuel(WASM_FUEL_LIMIT)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: format!("cannot set fuel budget: {reason}"),
            })?;
        let linker = Linker::new(&compiled.engine);
        let instance = linker
            .instantiate_and_start(&mut store, &compiled.module)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: reason.to_string(),
            })?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(WasmError::MissingExport {
                plugin: self.plugin.clone(),
                export: "memory",
            })?;
        for export in ["alloc", "dealloc", "handle"] {
            if instance.get_func(&store, export).is_none() {
                return Err(WasmError::MissingExport {
                    plugin: self.plugin.clone(),
                    export,
                });
            }
        }
        let alloc = instance
            .get_typed_func::<(i32,), (i32,)>(&store, "alloc")
            .map_err(|reason| self.signature("alloc", reason))?;
        let dealloc = instance
            .get_typed_func::<(i32, i32), ()>(&store, "dealloc")
            .map_err(|reason| self.signature("dealloc", reason))?;
        let handle = instance
            .get_typed_func::<(i32, i32), (i64,)>(&store, "handle")
            .map_err(|reason| self.signature("handle", reason))?;

        let input = payload.as_bytes();
        let input_len = i32::try_from(input.len()).map_err(|_| WasmError::PayloadTooLarge {
            plugin: self.plugin.clone(),
            size: input.len(),
        })?;
        let (input_ptr,) = alloc
            .call(&mut store, (input_len,))
            .map_err(|reason| self.execution(reason))?;
        memory
            .write(&mut store, input_ptr as usize, input)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: format!("input does not fit guest memory: {reason}"),
            })?;
        let (packed,) = handle
            .call(&mut store, (input_ptr, input_len))
            .map_err(|reason| self.execution(reason))?;
        let out_ptr = (packed >> 32) as u32;
        let out_len = packed as u32;
        if out_ptr == 0 || out_len == 0 || out_len as usize > MAX_WASM_PAYLOAD {
            return Err(WasmError::BadReply {
                plugin: self.plugin.clone(),
            });
        }
        let mut output = vec![0u8; out_len as usize];
        memory
            .read(&store, out_ptr as usize, &mut output)
            .map_err(|_| WasmError::BadReply {
                plugin: self.plugin.clone(),
            })?;
        let _ = dealloc.call(&mut store, (out_ptr as i32, out_len as i32));
        let _ = dealloc.call(&mut store, (input_ptr, input_len));
        String::from_utf8(output).map_err(|reason| WasmError::Reply {
            plugin: self.plugin.clone(),
            reason: format!("reply is not UTF-8: {reason}"),
        })
    }

    fn signature(&self, export: &'static str, reason: wasmi::Error) -> WasmError {
        WasmError::Execution {
            plugin: self.plugin.clone(),
            reason: format!("export `{export}` has the wrong signature: {reason}"),
        }
    }

    fn execution(&self, reason: wasmi::Error) -> WasmError {
        WasmError::Execution {
            plugin: self.plugin.clone(),
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ECHO_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (data (i32.const 512) "{\"deny\":\"wasm says no\"}")
  (data (i32.const 600) "{}")
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param $ptr i32) (param $len i32) (result i64)
    (if (result i64)
      (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 123))
      (then
        (i64.add
          (i64.shl (i64.extend_i32_u (i32.const 512)) (i64.const 32))
          (i64.extend_i32_u (i32.const 23))))
      (else
        (i64.add
          (i64.shl (i64.extend_i32_u (i32.const 600)) (i64.const 32))
          (i64.extend_i32_u (i32.const 2)))))))
"#;

    const LOOP_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param i32 i32) (result i64)
    (loop (br 0))
    unreachable))
"#;

    const NO_MEMORY_WAT: &str = r#"(module
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param i32 i32) (result i64) (i64.const 0))
)"#;

    fn test_manifest(permissions: &[&str]) -> PluginManifest {
        PluginManifest {
            name: "wasm-test".to_owned(),
            version: "0.1.0".to_owned(),
            entry: "main.wasm".to_owned(),
            permissions: permissions.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn plugin_dir(wat: &str) -> (tempfile::TempDir, WasmPlugin) {
        let dir = tempfile::tempdir().unwrap();
        let bytes = wat::parse_str(wat).unwrap();
        std::fs::write(dir.path().join("main.wasm"), bytes).unwrap();
        let plugin = WasmPlugin::new(
            "wasm-test",
            dir.path(),
            "main.wasm",
            &test_manifest(&["process"]),
        )
        .unwrap();
        (dir, plugin)
    }

    #[test]
    fn request_round_trips_through_guest_memory() {
        let (_dir, plugin) = plugin_dir(ECHO_WAT);
        let reply = plugin.request(&json!({"point": "user_input"})).unwrap();
        assert_eq!(reply, json!({"deny": "wasm says no"}));
    }

    #[test]
    fn handle_sees_the_request_bytes() {
        let (_dir, plugin) = plugin_dir(ECHO_WAT);
        // `call` is the raw ABI boundary: a payload that does not start with '{' takes the
        // module's else branch and replies with the empty object at offset 600.
        assert_eq!(plugin.call("[]").unwrap().trim(), "{}");
        assert!(plugin.call("{\"a\":1}").unwrap().contains("wasm says no"));
    }

    #[test]
    fn infinite_loop_traps_on_fuel() {
        let (_dir, plugin) = plugin_dir(LOOP_WAT);
        let err = plugin.request(&json!({})).err().unwrap();
        assert!(matches!(err, WasmError::Execution { .. }), "{err}");
    }

    #[test]
    fn missing_memory_export_is_refused() {
        let (_dir, plugin) = plugin_dir(NO_MEMORY_WAT);
        let err = plugin.request(&json!({})).err().unwrap();
        assert!(
            matches!(
                err,
                WasmError::MissingExport {
                    export: "memory",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn invalid_wasm_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.wasm"), b"not wasm at all").unwrap();
        let plugin = WasmPlugin::new(
            "wasm-test",
            dir.path(),
            "main.wasm",
            &test_manifest(&["process"]),
        )
        .unwrap();
        let err = plugin.request(&json!({})).err().unwrap();
        assert!(matches!(err, WasmError::Compile { .. }), "{err}");
        assert!(WasmPlugin::validate_bytes(b"garbage").is_err());
    }

    #[test]
    fn execution_requires_a_permission() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.wasm"),
            wat::parse_str(ECHO_WAT).unwrap(),
        )
        .unwrap();
        let err = WasmPlugin::new("wasm-test", dir.path(), "main.wasm", &test_manifest(&[]))
            .err()
            .unwrap();
        assert!(err.contains("declares no permissions"), "{err}");
        let err = WasmPlugin::new(
            "wasm-test",
            dir.path(),
            "main.wasm",
            &test_manifest(&["fs.read"]),
        )
        .err()
        .unwrap();
        assert!(err.contains("no execution permission"), "{err}");
    }

    #[test]
    fn entry_must_stay_inside_and_carry_wasm_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.wasm"),
            wat::parse_str(ECHO_WAT).unwrap(),
        )
        .unwrap();
        let err = WasmPlugin::new(
            "wasm-test",
            dir.path(),
            "../main.wasm",
            &test_manifest(&["process"]),
        )
        .err()
        .unwrap();
        assert!(err.contains("inside the plugin directory"), "{err}");
        let err = WasmPlugin::new(
            "wasm-test",
            dir.path(),
            "main.sh",
            &test_manifest(&["process"]),
        )
        .err()
        .unwrap();
        assert!(err.contains(".wasm extension"), "{err}");
    }

    #[test]
    fn declared_entries_prefers_components_wasm() {
        let mut manifest = test_manifest(&["process"]);
        manifest.components = Some(
            [(
                "wasm".to_owned(),
                vec!["a.wasm".to_owned(), "b.wasm".to_owned()],
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            WasmPlugin::declared_entries(&manifest),
            vec!["a.wasm".to_owned(), "b.wasm".to_owned()]
        );
        let manifest = test_manifest(&["process"]);
        assert_eq!(WasmPlugin::declared_entries(&manifest), vec!["main.wasm"]);
        let mut manifest = test_manifest(&["process"]);
        manifest.entry = "run.sh".to_owned();
        assert!(WasmPlugin::declared_entries(&manifest).is_empty());
    }
}
