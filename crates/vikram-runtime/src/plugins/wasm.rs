//! The WebAssembly plugin tier.
//!
//! A `.wasm` module declared under `components.wasm` (or as the manifest `entry` when it carries
//! the `.wasm` extension) runs inside the same permission envelope as a process plugin: the
//! manifest must declare an execution permission (`process` or `network`) and the entry must
//! resolve strictly inside the plugin directory. Execution is in-process through wasmi — no
//! subprocess, no WASI. The only host imports a module may use are `vikram.log` and
//! `vikram.read_resource`; any other import fails to instantiate.
//!
//! ABI (v2, JSON-over-memory):
//! - the module exports `memory`, `alloc(size: i32) -> i32`, `dealloc(ptr: i32, size: i32)` and
//!   `handle(ptr: i32, len: i32) -> i64`;
//! - the host allocates `len` bytes via `alloc`, writes the request JSON, calls `handle` and
//!   unpacks the returned `i64` as `(out_ptr << 32) | out_len` pointing at the response JSON in
//!   linear memory; both buffers are released through `dealloc`;
//! - the response is a [`crate::hooks::HookEffect`]-shaped JSON object — the same reply a
//!   process plugin writes to stdout;
//! - the module may import `(import "vikram" "log" (func (param i32 i32) (result i32)))`:
//!   `log(ptr, len)` reads `len` bytes at `ptr` from guest linear memory and captures them as
//!   plugin diagnostics, returning the number of bytes accepted. It returns `-1` when `ptr`
//!   or `len` is negative or `ptr` lies outside memory, and `0` once the per-request log
//!   budget ([`MAX_WASM_LOG`]) is spent. Captured lines are merged into the reply object
//!   under the reserved `_logs` key (overwriting any guest-supplied value); effect parsing
//!   ignores unknown keys, so v1 guests and replies interoperate unchanged;
//! - the module may also import
//!   `(import "vikram" "read_resource" (func (param i32 i32 i32 i32) (result i32)))`:
//!   `read_resource(uri_ptr, uri_len, out_ptr, out_cap)` reads a `uri_len`-byte URI at
//!   `uri_ptr` from guest linear memory, resolves it strictly inside the plugin's own
//!   directory (`plugin://<relpath>` or a bare relative path; absolute paths, `..` and
//!   symlink escapes are refused), and copies up to `out_cap` bytes of the file — capped
//!   at [`MAX_WASM_RESOURCE`] — into guest memory at `out_ptr`. It returns the number of
//!   bytes written, `0` for an empty file, or a negative code: `-1` not found or not a
//!   regular file, `-2` path refused (outside the plugin directory), `-3` file or
//!   `out_cap` exceeds the cap, `-4` host I/O failure, `-5` invalid guest pointer,
//!   length or URI.
//!
//! Execution is bounded: the module is compiled once per plugin and every call runs under a
//! fresh store with a fuel budget ([`WASM_FUEL_LIMIT`]) plus the recursion and stack limits
//! wasmi enforces by default.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use wasmi::{Caller, Engine, Extern, Linker, Module, Store};

use crate::plugins::{PluginManifest, ensure_execution_permitted, entry_path};

/// The largest serialized request a wasm plugin is sent — the same ceiling process plugins get.
const MAX_WASM_PAYLOAD: usize = 256 * 1024;

/// Fuel budget for one `handle` invocation. A module that loops forever traps with
/// `OutOfFuel` instead of hanging the turn.
const WASM_FUEL_LIMIT: u64 = 50_000_000;

/// Total bytes `vikram.log` may capture per `handle` call. Host-side log reads are not
/// fuel-metered, so the budget bounds what a guest can make the host copy.
const MAX_WASM_LOG: usize = 64 * 1024;

/// Largest file `vikram.read_resource` will copy into guest memory per call.
const MAX_WASM_RESOURCE: usize = 64 * 1024;

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

/// Per-call host state: the raw bytes `vikram.log` has captured so far, plus the plugin
/// directory `vikram.read_resource` confines reads to.
#[derive(Default)]
struct CallState {
    log: Vec<u8>,
    root: PathBuf,
}

/// A wasm plugin: its manifest-gated entry, compiled lazily on first call.
///
/// `WasmPlugin` is `Send + Sync`: the compiled module is immutable and every call instantiates a
/// fresh store, so concurrent dispatches cannot share mutable wasm state.
pub struct WasmPlugin {
    plugin: String,
    dir: PathBuf,
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
            dir,
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
        let (reply, logs) = self.call(&payload)?;
        let mut value: Value =
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
        if let Value::Object(map) = &mut value {
            map.remove("_logs");
            if !logs.is_empty() {
                map.insert(
                    "_logs".to_owned(),
                    Value::Array(logs.into_iter().map(Value::String).collect()),
                );
            }
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
    ///
    /// Returns the reply JSON plus the diagnostics lines the guest emitted through
    /// `vikram.log` during the call.
    fn call(&self, payload: &str) -> Result<(String, Vec<String>), WasmError> {
        let compiled = self
            .compiled
            .get_or_init(|| self.compile())
            .as_ref()
            .map_err(|reason| WasmError::Compile {
                plugin: self.plugin.clone(),
                reason: reason.clone(),
            })?;
        let mut store = Store::new(
            &compiled.engine,
            CallState {
                log: Vec::new(),
                root: self.dir.clone(),
            },
        );
        store
            .set_fuel(WASM_FUEL_LIMIT)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: format!("cannot set fuel budget: {reason}"),
            })?;
        let mut linker = Linker::<CallState>::new(&compiled.engine);
        linker
            .func_wrap("vikram", "log", wasm_log)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: format!("cannot register `vikram.log` host import: {reason}"),
            })?;
        linker
            .func_wrap("vikram", "read_resource", wasm_read_resource)
            .map_err(|reason| WasmError::Execution {
                plugin: self.plugin.clone(),
                reason: format!("cannot register `vikram.read_resource` host import: {reason}"),
            })?;
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
        let log_bytes = std::mem::take(&mut store.data_mut().log);
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
        let logs = String::from_utf8_lossy(&log_bytes)
            .lines()
            .map(|line| line.trim_end().to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        let reply = String::from_utf8(output).map_err(|reason| WasmError::Reply {
            plugin: self.plugin.clone(),
            reason: format!("reply is not UTF-8: {reason}"),
        })?;
        Ok((reply, logs))
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

/// The `vikram.log` host import: copy `len` bytes at `ptr` out of guest linear memory into
/// the per-call log budget and return how many were accepted.
///
/// Every guest-supplied value is validated before it touches host memory: negative
/// `ptr`/`len` and a `ptr` outside the exported `memory` return `-1`, a spent
/// [`MAX_WASM_LOG`] budget returns `0`, and the copy itself is capped by both the
/// remaining memory and the remaining budget so a hostile `len` cannot make the host
/// read or allocate beyond [`MAX_WASM_LOG`] bytes.
fn wasm_log(mut caller: Caller<'_, CallState>, ptr: i32, len: i32) -> i32 {
    if ptr < 0 || len < 0 {
        return -1;
    }
    let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
        return -1;
    };
    let start = ptr as usize;
    let requested = len as usize;
    let data_size = memory.data_size(&caller);
    if start >= data_size {
        return if requested == 0 { 0 } else { -1 };
    }
    let budget = MAX_WASM_LOG.saturating_sub(caller.data().log.len());
    let take = requested.min(data_size - start).min(budget);
    if take == 0 {
        return 0;
    }
    let mut bytes = vec![0u8; take];
    if memory.read(&caller, start, &mut bytes).is_err() {
        return -1;
    }
    caller.data_mut().log.extend_from_slice(&bytes);
    take as i32
}

/// The `vikram.read_resource` host import: serve a file from inside the plugin's own
/// directory into a guest-provided out buffer.
///
/// The URI is read from guest memory, trimmed, stripped of an optional `plugin://`
/// prefix and resolved through [`entry_path`] against the plugin directory, so absolute
/// paths, `..` components and symlinks pointing outside are all refused before any host
/// filesystem access. Only regular files up to [`MAX_WASM_RESOURCE`] bytes are served,
/// and only when they fit in the guest's `out_cap` buffer; the copy itself is validated
/// against guest memory bounds like every other pointer. Returns bytes written, `0` for
/// an empty file, or a negative code documented in the module docs.
fn wasm_read_resource(
    mut caller: Caller<'_, CallState>,
    uri_ptr: i32,
    uri_len: i32,
    out_ptr: i32,
    out_cap: i32,
) -> i32 {
    const BAD_ARGS: i32 = -5;
    if uri_ptr < 0 || uri_len < 0 || out_ptr < 0 || out_cap < 0 {
        return BAD_ARGS;
    }
    let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
        return BAD_ARGS;
    };
    let data_size = memory.data_size(&caller);
    let uri_start = uri_ptr as usize;
    let uri_len = uri_len as usize;
    let uri_end = uri_start.saturating_add(uri_len);
    if uri_len == 0 || uri_end > data_size {
        return BAD_ARGS;
    }
    let mut uri_bytes = vec![0u8; uri_len];
    if memory.read(&caller, uri_start, &mut uri_bytes).is_err() {
        return BAD_ARGS;
    }
    let Ok(uri) = std::str::from_utf8(&uri_bytes) else {
        return BAD_ARGS;
    };
    let uri = uri.trim();
    let rel = uri.strip_prefix("plugin://").unwrap_or(uri);
    let path = match entry_path(&caller.data().root, rel) {
        Ok(path) => path,
        Err(_) => return -2,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(reason) => {
            return if reason.kind() == std::io::ErrorKind::NotFound {
                -1
            } else {
                -4
            };
        }
    };
    if !meta.is_file() {
        return -1;
    }
    let size = meta.len() as usize;
    if size > MAX_WASM_RESOURCE || size > out_cap as usize {
        return -3;
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return if reason.kind() == std::io::ErrorKind::NotFound {
                -1
            } else {
                -4
            };
        }
    };
    let len = bytes.len();
    if len > out_cap as usize {
        return -3;
    }
    if len == 0 {
        return 0;
    }
    let out_start = out_ptr as usize;
    let out_end = out_start.saturating_add(len);
    if out_end > data_size || memory.write(&mut caller, out_start, &bytes).is_err() {
        return BAD_ARGS;
    }
    len as i32
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
        assert_eq!(plugin.call("[]").unwrap().0.trim(), "{}");
        assert!(plugin.call("{\"a\":1}").unwrap().0.contains("wasm says no"));
    }

    const LOG_WAT: &str = r#"(module
  (import "vikram" "log" (func $log (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 512) "hello from wasm\nsecond line")
  (data (i32.const 600) "{\"ok\":true}")
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param $ptr i32) (param $len i32) (result i64)
    (drop (call $log (i32.const 512) (i32.const 27)))
    (i64.add
      (i64.shl (i64.extend_i32_u (i32.const 600)) (i64.const 32))
      (i64.extend_i32_u (i32.const 11)))))
"#;

    #[test]
    fn guest_log_lines_are_captured_into_reply() {
        let (_dir, plugin) = plugin_dir(LOG_WAT);
        let reply = plugin.request(&json!({"point": "user_input"})).unwrap();
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(
            reply["_logs"],
            json!(["hello from wasm", "second line"]),
            "{reply}"
        );
    }

    #[test]
    fn guest_without_log_import_has_no_logs_key() {
        let (_dir, plugin) = plugin_dir(ECHO_WAT);
        let reply = plugin.request(&json!({})).unwrap();
        assert!(reply.get("_logs").is_none(), "{reply}");
    }

    const BAD_LOG_WAT: &str = r#"(module
  (import "vikram" "log" (func $log (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 600) "{\"ok\":true}")
  (data (i32.const 620) "{\"ok\":false}")
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param $ptr i32) (param $len i32) (result i64)
    (if (result i64)
      (i32.eq (call $log (i32.const 999999) (i32.const 10)) (i32.const -1))
      (then
        (i64.add
          (i64.shl (i64.extend_i32_u (i32.const 620)) (i64.const 32))
          (i64.extend_i32_u (i32.const 12))))
      (else
        (i64.add
          (i64.shl (i64.extend_i32_u (i32.const 600)) (i64.const 32))
          (i64.extend_i32_u (i32.const 11)))))))
"#;

    #[test]
    fn out_of_bounds_log_pointer_is_rejected_not_captured() {
        let (_dir, plugin) = plugin_dir(BAD_LOG_WAT);
        let reply = plugin.request(&json!({})).unwrap();
        assert_eq!(reply["ok"], json!(false), "{reply}");
        assert!(reply.get("_logs").is_none(), "{reply}");
    }

    const READ_WAT: &str = r#"(module
  (import "vikram" "log" (func $log (param i32 i32) (result i32)))
  (import "vikram" "read_resource" (func $read (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 512) "plugin://data.txt   ")
  (data (i32.const 536) "data.txt            ")
  (data (i32.const 560) "../escape.txt       ")
  (data (i32.const 584) "missing.txt         ")
  (data (i32.const 608) "big.txt             ")
  (data (i32.const 632) "unused              ")
  (data (i32.const 656) "data.txt            ")
  (data (i32.const 680) "link.txt            ")
  (data (i32.const 720) "{\"ok\":true}")
  (data (i32.const 740) "{\"code\":-1}")
  (data (i32.const 760) "{\"code\":-2}")
  (data (i32.const 780) "{\"code\":-3}")
  (data (i32.const 800) "{\"code\":-4}")
  (data (i32.const 820) "{\"code\":-5}")
  (data (i32.const 840) "{\"ok\":false}")
  (global $heap (mut i32) (i32.const 4096))
  (func $uri_ptr (param $sel i32) (result i32)
    (if (result i32)
      (i32.eq (local.get $sel) (i32.const 102))
      (then (i32.const 999999))
      (else
        (i32.add (i32.const 512)
          (i32.mul (i32.sub (local.get $sel) (i32.const 97)) (i32.const 24))))))
  (func $out_cap (param $sel i32) (result i32)
    (if (result i32)
      (i32.eq (local.get $sel) (i32.const 103))
      (then (i32.const 4))
      (else (i32.const 8192))))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.add
      (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
      (i64.extend_i32_u (local.get $l))))
  (func $pack_reply (param $n i32) (result i64)
    (if (result i64) (i32.eq (local.get $n) (i32.const -1))
      (then (call $pack (i32.const 740) (i32.const 11)))
      (else
        (if (result i64) (i32.eq (local.get $n) (i32.const -2))
          (then (call $pack (i32.const 760) (i32.const 11)))
          (else
            (if (result i64) (i32.eq (local.get $n) (i32.const -3))
              (then (call $pack (i32.const 780) (i32.const 11)))
              (else
                (if (result i64) (i32.eq (local.get $n) (i32.const -4))
                  (then (call $pack (i32.const 800) (i32.const 11)))
                  (else
                    (if (result i64) (i32.eq (local.get $n) (i32.const -5))
                      (then (call $pack (i32.const 820) (i32.const 11)))
                      (else (call $pack (i32.const 840) (i32.const 12)))))))))))))
  (func (export "alloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param $ptr i32) (param $len i32) (result i64)
    (local $sel i32) (local $n i32)
    (local.set $sel (i32.load8_u (i32.add (local.get $ptr) (i32.const 1))))
    (local.set $n (call $read
      (call $uri_ptr (local.get $sel))
      (i32.const 20)
      (i32.const 8192)
      (call $out_cap (local.get $sel))))
    (if (result i64)
      (i32.gt_s (local.get $n) (i32.const 0))
      (then
        (drop (call $log (i32.const 8192) (local.get $n)))
        (call $pack (i32.const 720) (i32.const 11)))
      (else (call $pack_reply (local.get $n))))))
"#;

    const READ_OK_WAT: &str = r#"(module
  (import "vikram" "log" (func $log (param i32 i32) (result i32)))
  (import "vikram" "read_resource" (func $read (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 512) "plugin://data.txt")
  (data (i32.const 560) "data.txt")
  (data (i32.const 600) "{\"ok\":true}")
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $size)))
    (local.get $ptr))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param $ptr i32) (param $len i32) (result i64)
    (local $n i32)
    (local.set $n (call $read (i32.const 512) (i32.const 17) (i32.const 8192) (i32.const 8192)))
    (if (i32.gt_s (local.get $n) (i32.const 0))
      (then (drop (call $log (i32.const 8192) (local.get $n)))))
    (local.set $n (call $read (i32.const 560) (i32.const 8) (i32.const 8192) (i32.const 8192)))
    (if (i32.gt_s (local.get $n) (i32.const 0))
      (then (drop (call $log (i32.const 8192) (local.get $n)))))
    (i64.add
      (i64.shl (i64.extend_i32_u (i32.const 600)) (i64.const 32))
      (i64.extend_i32_u (i32.const 11)))))
"#;

    fn resource_plugin(wat: &str) -> (tempfile::TempDir, WasmPlugin) {
        let (dir, plugin) = plugin_dir(wat);
        std::fs::write(dir.path().join("data.txt"), b"res\xffource\n").unwrap();
        std::fs::write(dir.path().join("big.txt"), vec![b'x'; 128 * 1024]).unwrap();
        (dir, plugin)
    }

    #[test]
    fn read_resource_serves_files_inside_plugin_dir() {
        let (_dir, plugin) = resource_plugin(READ_OK_WAT);
        let reply = plugin.request(&json!("a")).unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(
            reply["_logs"],
            json!(["res\u{fffd}ource", "res\u{fffd}ource"]),
            "{reply}"
        );
    }

    #[test]
    fn read_resource_accepts_bare_relative_paths() {
        let (_dir, plugin) = resource_plugin(READ_WAT);
        let reply = plugin.request(&json!("b")).unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["_logs"], json!(["res\u{fffd}ource"]), "{reply}");
    }

    #[test]
    fn read_resource_refuses_paths_outside_plugin_dir() {
        let (_dir, plugin) = resource_plugin(READ_WAT);
        assert_eq!(plugin.request(&json!("c")).unwrap(), json!({"code": -2}));
    }

    #[test]
    fn read_resource_reports_not_found_too_big_and_bad_args() {
        let (_dir, plugin) = resource_plugin(READ_WAT);
        assert_eq!(plugin.request(&json!("d")).unwrap(), json!({"code": -1}));
        assert_eq!(plugin.request(&json!("e")).unwrap(), json!({"code": -3}));
        assert_eq!(plugin.request(&json!("f")).unwrap(), json!({"code": -5}));
        assert_eq!(plugin.request(&json!("g")).unwrap(), json!({"code": -3}));
    }

    #[cfg(unix)]
    #[test]
    fn read_resource_refuses_symlink_escape() {
        let (dir, plugin) = resource_plugin(READ_WAT);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        assert_eq!(plugin.request(&json!("h")).unwrap(), json!({"code": -2}));
    }

    const FOREIGN_IMPORT_WAT: &str = r#"(module
  (import "env" "nope" (func))
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "handle") (param i32 i32) (result i64) (i64.const 0))
)"#;

    #[test]
    fn imports_other_than_vikram_log_still_fail() {
        let (_dir, plugin) = plugin_dir(FOREIGN_IMPORT_WAT);
        let err = plugin.request(&json!({})).err().unwrap();
        assert!(matches!(err, WasmError::Execution { .. }), "{err}");
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
