//! `vikram-bench` — an honest, in-process benchmark harness for VIKRAM.
//!
//! Every number this binary prints was produced by running the real code in *this* process and
//! timing it with [`std::time::Instant`]. Nothing is copied from a previous run, estimated, or
//! extrapolated, and no path that would need a model credential or the network is benchmarked at
//! all.
//!
//! # Method
//!
//! Each operation gets one untimed warm-up pass of [`WARMUP`] iterations (page cache, SQLite
//! statement cache, allocator warm), then [`RUNS`] timed runs of [`ITERATIONS`] iterations. Every
//! iteration is timed individually and the median and p95 are nearest-rank over all
//! `RUNS * ITERATIONS` per-iteration samples — so a single slow iteration is visible instead of
//! being averaged away. Timings include the cost of the `Instant` pair itself (a few tens of
//! nanoseconds), which is the floor of this method and is stated in the header rather than
//! subtracted.
//!
//! # What is deliberately not measured
//!
//! Anything whose cost is dominated by the network or by a provider credential: model completions,
//! `web.search`, gateway delivery to a real platform, MCP server handshakes, browser automation,
//! cloud providers. Also terminal rendering (needs a TTY) and process spawn / OS sandbox set-up
//! (a different order of cost, and not an in-process operation). Those paths are named in the
//! header instead of being reported as fast.
//!
//! No cross-framework performance claim is made anywhere in this harness. See `docs/comparison.md`.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use vikram_codeindex::CodeIndex;
use vikram_contracts::{
    ActionKind, Block, JournalSink, Message, NewRecord, PermissionRequest, RecordKind, Role,
    SideEffect, ToolCall, Trust, VikramError,
};
use vikram_gateway::helpers::{codepoint_len, truncate_message};
use vikram_gateway::platforms::bluebubbles::BlueBubblesAdapter;
use vikram_gateway::transport::{
    HttpRequest, HttpResponse, SharedTransport, Transport, TransportError,
};
use vikram_runtime::{Config, MemoryStore, Tool, ToolContext, ToolOutput, ToolRegistry, ToolSpec};
use vikram_store::Store;

/// Timed runs per operation.
const RUNS: usize = 5;
/// Iterations per timed run.
const ITERATIONS: usize = 1000;
/// Untimed warm-up iterations per operation (excluded from every statistic).
const WARMUP: usize = 100;

/// One measured operation.
struct Row {
    name: &'static str,
    /// The work one iteration performs, e.g. `per session insert`.
    unit: &'static str,
    /// Number of per-iteration samples the median/p95 were taken over.
    iterations: usize,
    median_ns: u128,
    p95_ns: u128,
}

fn main() -> Result<(), Box<dyn Error>> {
    print_header();

    let mut rows: Vec<Row> = Vec::new();

    // `Config::load` resolves `$VIKRAM_HOME` on every call, so the harness points it at a temp
    // directory holding a real config file. The `TempDir` stays alive for the whole run.
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("config.json"), CONFIG_JSON)?;
    // SAFETY: this happens on the main thread before anything else runs; the harness never spawns
    // a thread, so no other reader of the environment can exist while it is being mutated.
    unsafe { std::env::set_var("VIKRAM_HOME", home.path()) };

    bench_config(&mut rows)?;
    bench_store(&mut rows)?;
    bench_gateway(&mut rows)?;
    bench_runtime(&mut rows)?;
    bench_retrieval(&mut rows)?;

    print_table(&rows);
    print_footer(&rows);
    Ok(())
}

fn print_header() {
    println!("vikram-bench — in-process benchmark harness for VIKRAM");
    println!(
        "host        {}/{}   build {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug (numbers are NOT representative — run with --release)"
        } else {
            "release"
        }
    );
    println!(
        "method      {WARMUP} untimed warm-up iterations, then {RUNS} timed runs x {ITERATIONS} iterations"
    );
    println!(
        "            every iteration timed on its own with std::time::Instant; median and p95 are"
    );
    println!(
        "            nearest-rank over all {} per-iteration samples",
        RUNS * ITERATIONS
    );
    println!(
        "            a per-iteration Instant costs roughly 20-40 ns, so sub-microsecond rows are"
    );
    println!("            upper bounds, not exact costs");
    println!("not measured (need network, credentials, or a TTY):");
    println!("            model completions, web.search, gateway delivery, MCP handshakes,");
    println!("            browser automation, cloud providers, terminal rendering, sandbox spawn");
    println!(
        "            no cross-framework performance number is claimed here (docs/comparison.md)"
    );
}

// ---------------------------------------------------------------- measurement core

fn measure(
    rows: &mut Vec<Row>,
    name: &'static str,
    unit: &'static str,
    step: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..WARMUP {
        step().map_err(Box::<dyn Error>::from)?;
    }
    let mut samples: Vec<u128> = Vec::with_capacity(RUNS * ITERATIONS);
    for _ in 0..RUNS {
        for _ in 0..ITERATIONS {
            let started = Instant::now();
            step().map_err(Box::<dyn Error>::from)?;
            samples.push(started.elapsed().as_nanos());
        }
    }
    samples.sort_unstable();
    rows.push(Row {
        name,
        unit,
        iterations: samples.len(),
        median_ns: nearest_rank(&samples, 50),
        p95_ns: nearest_rank(&samples, 95),
    });
    Ok(())
}

/// Nearest-rank percentile of an ascending slice. `percent` is 1..=100.
fn nearest_rank(sorted: &[u128], percent: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn fmt_time(ns: u128) -> String {
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.2} us", ns as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    }
}

fn print_table(rows: &[Row]) {
    println!();
    println!(
        "{:<46} | {:>10} | {:>10} | {:>10} | unit",
        "operation", "iterations", "median", "p95"
    );
    println!(
        "{}-+-{}-+-{}-+-{}-+-{}",
        "-".repeat(46),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(54)
    );
    for row in rows {
        println!(
            "{:<46} | {:>10} | {:>10} | {:>10} | {}",
            row.name,
            row.iterations,
            fmt_time(row.median_ns),
            fmt_time(row.p95_ns),
            row.unit
        );
    }
}

fn print_footer(rows: &[Row]) {
    println!();
    println!("notes");
    println!(
        "  * VIKRAM's operator config is JSON (`$VIKRAM_HOME/config.json`), not TOML: there is no TOML"
    );
    println!(
        "    parser in the workspace, so the config rows measure the real JSON load and parse path."
    );
    println!(
        "  * store rows use a file-backed SQLite database (`Store::open`) in a temp directory with the"
    );
    println!("    journal's WAL / synchronous=NORMAL pragmas, i.e. the production settings.");
    println!(
        "  * `store.journal.append` appends to a freshly created empty session each iteration so the"
    );
    println!("    per-append cost stays constant instead of drifting as one session's log grows.");
    println!(
        "  * gateway rows run the real `truncate_message` chunker and the real BlueBubbles webhook"
    );
    println!(
        "    parser; the adapter is built over a transport that refuses to make requests, so no"
    );
    println!("    network call can happen inside a timed iteration.");
    println!(
        "  * `runtime.tool_dispatch` goes through the real `ToolRegistry` (lookup by name, then the"
    );
    println!(
        "    `Tool::run` vtable) with a trivial tool registered, so no filesystem or sandbox cost is"
    );
    println!("    hidden inside the number.");
    println!(
        "  * `codeindex.ranked_context` and `memory.recall` query an index/memory store seeded at"
    );
    println!("    start-up; the seed sizes are printed in the unit column.");
    println!(
        "  * {} rows, {} samples per row, {:?} total timed iterations.",
        rows.len(),
        RUNS * ITERATIONS,
        rows.len() * RUNS * ITERATIONS
    );
}

// ---------------------------------------------------------------- 1. config

/// A realistic operator config: every field the runtime reads on a normal start.
const CONFIG_JSON: &str = r#"{
  "base_url": "https://api.openai.com/v1",
  "model": "gpt-5-codex",
  "api_key_env": "OPENAI_API_KEY",
  "context_window": 400000,
  "workspace_root": "/home/operator/project",
  "max_steps": 40,
  "max_output_tokens": 16000,
  "learning_review_every": 0,
  "memory_nudge_interval": 10,
  "title_model": "gpt-5-mini",
  "ascii_fallback": false,
  "thinking": true,
  "mode": "ask",
  "search": { "backend": "auto", "api_key_env": "VIKRAM_SEARCH_API_KEY" },
  "context": {
    "auto_compact": true,
    "trigger_percent": 72,
    "keep_recent": 12,
    "tool_output_ceiling": 6000,
    "memory_retrieval": true,
    "memory_budget_tokens": 1200,
    "project_budget_tokens": 4000
  },
  "security": {
    "disable_yolo": false,
    "disable_auto_approve": false,
    "disable_always_allow": false,
    "sandbox": { "enabled": true, "allow_network": false }
  }
}"#;

fn bench_config(rows: &mut Vec<Row>) -> Result<(), Box<dyn Error>> {
    measure(
        rows,
        "config.load",
        "per load (fs read + JSON parse)",
        &mut || {
            let config = Config::load().map_err(|e| e.to_string())?;
            // Prove the file was actually read and parsed, not defaulted.
            if config.max_steps != 40 || config.model != "gpt-5-codex" {
                return Err(format!(
                    "config did not parse the seeded file: max_steps={} model={:?}",
                    config.max_steps, config.model
                ));
            }
            Ok(())
        },
    )?;

    measure(
        rows,
        "config.parse_json",
        "per parse (no file read)",
        &mut || {
            let config: Config = serde_json::from_str(CONFIG_JSON).map_err(|e| e.to_string())?;
            if config.context.trigger_percent != 72 {
                return Err("nested context config not parsed".to_string());
            }
            Ok(())
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------- 2. store

fn bench_store(rows: &mut Vec<Row>) -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;

    // (a) session insert
    let insert_store = Store::open(&dir.path().join("insert.db"))?;
    let mut seq: u64 = 0;
    measure(
        rows,
        "store.session.insert",
        "per session insert",
        &mut || {
            seq += 1;
            insert_store
                .create_session(
                    &format!("bench session {seq}"),
                    Path::new("/work/bench"),
                    Some("gpt-5-codex"),
                    1_700_000_000_000 + seq,
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        },
    )?;

    // (b) journal append, one empty session per iteration so the cost stays constant
    let journal_store = Store::open(&dir.path().join("journal.db"))?;
    let sessions_needed = WARMUP + RUNS * ITERATIONS;
    let mut journal_sessions = Vec::with_capacity(sessions_needed);
    for i in 0..sessions_needed {
        let row = journal_store.create_session(
            &format!("journal {i}"),
            Path::new("/work/bench"),
            None,
            1_700_000_000_000 + i as u64,
        )?;
        journal_sessions.push(row.id);
    }
    let journal = journal_store.journal();
    let mut cursor = 0usize;
    measure(
        rows,
        "store.journal.append",
        "per journaled message record",
        &mut || {
            let session = journal_sessions
                .get(cursor)
                .ok_or_else(|| "ran out of seeded sessions".to_string())?
                .clone();
            cursor += 1;
            let record = NewRecord::new(
                session,
                RecordKind::Message,
                json!({
                    "role": "user",
                    "blocks": [{ "type": "text", "text": "benchmark message body", "trust": "user" }]
                }),
            );
            journal.append(&record).map_err(|e| e.to_string())?;
            Ok(())
        },
    )?;

    // (c) listing a store that holds 250 active sessions
    let list_store = Store::open(&dir.path().join("list.db"))?;
    for i in 0..250u64 {
        list_store.create_session(
            &format!("listed {i}"),
            Path::new("/work/bench"),
            Some("gpt-5-codex"),
            1_700_000_000_000 + i,
        )?;
    }
    measure(
        rows,
        "store.list_sessions",
        "per list of 250 active sessions",
        &mut || {
            let listed = list_store.list_sessions().map_err(|e| e.to_string())?.len();
            if listed != 250 {
                return Err(format!("expected 250 sessions, got {listed}"));
            }
            Ok(())
        },
    )?;

    // (d) replaying a 250-record conversation, the restart path
    let replay_store = Store::open(&dir.path().join("replay.db"))?;
    let replay_session = replay_store
        .create_session("replay", Path::new("/work/bench"), None, 1_700_000_000_000)?
        .id;
    let replay_journal = replay_store.journal();
    for i in 0..250u64 {
        let record = NewRecord::new(
            replay_session.clone(),
            RecordKind::Message,
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "blocks": [{ "type": "text", "text": "replayed message body", "trust": "user" }]
            }),
        );
        replay_journal.append(&record).map_err(|e| e.to_string())?;
    }
    measure(
        rows,
        "store.replay_messages",
        "per replay of 250 journal records",
        &mut || {
            let replayed = replay_store
                .replay_messages(&replay_session)
                .map_err(|e| e.to_string())?
                .len();
            if replayed != 250 {
                return Err(format!("expected 250 records, got {replayed}"));
            }
            Ok(())
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------- 3. gateway

/// A transport that cannot reach anything: the gateway's parse and chunking paths are pure, so a
/// timed iteration never depends on the network.
struct NoNetworkTransport;

#[async_trait::async_trait]
impl Transport for NoNetworkTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        Err(TransportError::Request(format!(
            "vikram-bench makes no network calls (would have sent {})",
            request.url
        )))
    }
}

/// Roughly 64 KiB of markdown, including fenced blocks, so the chunker has fences to re-open.
fn markdown_64k() -> String {
    let mut text = String::with_capacity(64 * 1024 + 256);
    let mut section = 0;
    while text.len() < 64 * 1024 {
        section += 1;
        text.push_str(&format!("## section {section}\n"));
        text.push_str("A gateway chunk must keep a fenced block whole whenever it fits.\n");
        if section % 5 == 0 {
            text.push_str("```rust\nfn chunk_limit() -> usize { 4096 }\n```\n");
        }
        text.push('\n');
    }
    text
}

/// One BlueBubbles `new-message` delivery carrying text and two attachments.
const WEBHOOK_BODY: &str = r#"{
  "type": "new-message",
  "data": {
    "guid": "6F1E2A3B-1111-2222-3333-444455556666",
    "text": "can you look at this screenshot and the log file?",
    "handle": { "address": "+15551234567", "firstName": "Operator" },
    "chats": [{ "guid": "iMessage;-;+15551234567", "style": 45 }],
    "isFromMe": false,
    "dateCreated": 1700000000000,
    "attachments": [
      { "guid": "att-1", "mimeType": "image/jpeg", "transferName": "screenshot.jpg", "totalBytes": 184320 },
      { "guid": "att-2", "mimeType": "text/plain", "transferName": "build.log", "totalBytes": 4096 }
    ]
  }
}"#;

fn bench_gateway(rows: &mut Vec<Row>) -> Result<(), Box<dyn Error>> {
    let text = markdown_64k();
    if text.len() < 64 * 1024 {
        return Err(format!("seeded text is only {} bytes", text.len()).into());
    }

    measure(
        rows,
        "gateway.truncate_message",
        "per 64 KiB chunking (max_len=4096)",
        &mut || {
            let chunks = truncate_message(&text, 4096, codepoint_len);
            if chunks.len() < 2 {
                return Err(format!("expected several chunks, got {}", chunks.len()));
            }
            // Chunking must keep the content; a chunker that dropped it would look fast.
            let kept: usize = chunks.iter().map(String::len).sum();
            if kept < text.len() / 2 {
                return Err(format!("chunking kept only {kept} of {} bytes", text.len()));
            }
            Ok(())
        },
    )?;

    let transport: SharedTransport = Arc::new(NoNetworkTransport);
    let adapter = BlueBubblesAdapter::new(transport, "127.0.0.1:1234", "bench-password");
    measure(
        rows,
        "gateway.webhook_parse",
        "per webhook parse (1 event, 2 attachments)",
        &mut || {
            let events = adapter.parse_webhook(WEBHOOK_BODY)?;
            if events.len() != 1 {
                return Err(format!("expected 1 event, got {}", events.len()));
            }
            if events[0].media_urls.len() != 2 {
                return Err(format!(
                    "expected 2 attachments, got {}",
                    events[0].media_urls.len()
                ));
            }
            Ok(())
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------- 4. runtime

/// A trivial tool, so the registry dispatch is measured without filesystem or sandbox cost hiding
/// inside it.
struct EchoTool;

impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bench.echo".to_string(),
            description: "Echo the arguments back. Used by vikram-bench only.".to_string(),
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }
    }

    fn permission(&self, _args: &serde_json::Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "bench.echo",
            "bench",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        Ok(ToolOutput {
            content: args.to_string(),
            ok: true,
        })
    }
}

fn sample_message() -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![
            Block::Text {
                text: "Reading the file and summarising the three failures.".to_string(),
                trust: Trust::Model,
            },
            Block::ToolCall {
                call: ToolCall::new(
                    "fs.read",
                    json!({ "path": "crates/vikram-store/src/lib.rs" }),
                ),
            },
        ],
    }
}

fn bench_runtime(rows: &mut Vec<Row>) -> Result<(), Box<dyn Error>> {
    let message = sample_message();
    measure(
        rows,
        "contracts.message.to_json",
        "per Message serialisation",
        &mut || {
            let encoded = serde_json::to_string(&message).map_err(|e| e.to_string())?;
            if !encoded.contains("fs.read") {
                return Err("serialised message lost the tool call".to_string());
            }
            Ok(())
        },
    )?;

    measure(
        rows,
        "contracts.message.from_json",
        "per Message parse",
        &mut || {
            let raw = serde_json::to_string(&message).map_err(|e| e.to_string())?;
            let decoded: Message = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if decoded.blocks.len() != message.blocks.len() {
                return Err("decoded message lost blocks".to_string());
            }
            Ok(())
        },
    )?;

    let registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool));
    let ctx = ToolContext::default();
    measure(
        rows,
        "runtime.tool_dispatch",
        "per registry lookup + Tool::run",
        &mut || {
            let tool = registry
                .get("bench.echo")
                .ok_or_else(|| "bench.echo not registered".to_string())?;
            let output = tool
                .run(&json!({ "text": "bench" }), &ctx)
                .map_err(|e| e.to_string())?;
            if !output.ok || !output.content.contains("bench") {
                return Err("tool dispatch returned the wrong result".to_string());
            }
            Ok(())
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------- 5. retrieval

/// 60 source files x 40 functions, enough that symbol lookup has to do real work.
fn seed_code_tree(root: &Path) -> Result<usize, Box<dyn Error>> {
    let mut written = 0;
    for file in 0..60 {
        let mut body = String::new();
        for function in 0..20 {
            body.push_str(&format!(
                "pub fn compute_total_{file}_{function}(input: u32) -> u32 {{ input + {function} }}\n"
            ));
            body.push_str(&format!(
                "pub fn helper_{function}() -> &'static str {{ \"helper {file}/{function}\" }}\n"
            ));
        }
        std::fs::write(root.join(format!("mod_{file}.rs")), body)?;
        written += 1;
    }
    Ok(written)
}

fn bench_retrieval(rows: &mut Vec<Row>) -> Result<(), Box<dyn Error>> {
    // (a) code index: build a real index once, then measure the query path the tool uses.
    let code_dir = tempfile::tempdir()?;
    let files = seed_code_tree(code_dir.path())?;
    let mut index = CodeIndex::open(code_dir.path(), &code_dir.path().join("index.db"))?;
    let stats = index.index(&[])?;
    if stats.symbols == 0 {
        return Err(format!("indexed {files} files but found no symbols").into());
    }
    measure(
        rows,
        "codeindex.ranked_context",
        "per query over a 60-file / 1200-symbol index",
        &mut || {
            let context = index
                .ranked_context("compute_total", 20)
                .map_err(|e| e.to_string())?;
            if context.is_empty() {
                return Err("ranked_context returned nothing; the index was not consulted".into());
            }
            Ok(())
        },
    )?;

    // (b) memory retrieval: the hybrid recall the context engine runs each turn.
    let memory = MemoryStore::open_in_memory()?;
    for i in 0..200 {
        memory
            .save(
                format!("note-{i}"),
                format!(
                    "The gateway retries delivery when the platform reports a transient failure ({i})."
                ),
                "project",
            )
            .map_err(|e| e.to_string())?;
    }
    let terms: Vec<String> = ["deploy", "config", "gateway"]
        .iter()
        .map(|term| (*term).to_string())
        .collect();
    let scopes: Vec<String> = vec!["project".to_string()];
    measure(
        rows,
        "memory.recall",
        "per hybrid recall over 200 memories",
        &mut || {
            let hits = memory
                .recall_terms(&terms, &scopes, 5)
                .map_err(|e| e.to_string())?;
            if hits.is_empty() {
                return Err("recall returned nothing; the store was not consulted".into());
            }
            Ok(())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_picks_the_documented_sample() {
        let sorted = [10, 20, 30, 40];
        assert_eq!(nearest_rank(&sorted, 50), 20, "rank 2 of 4");
        assert_eq!(
            nearest_rank(&sorted, 95),
            40,
            "p95 of four samples is the last"
        );
        assert_eq!(nearest_rank(&[], 50), 0);
    }

    #[test]
    fn time_formatting_switches_unit_once() {
        assert_eq!(fmt_time(999), "999 ns");
        assert_eq!(fmt_time(1_500), "1.50 us");
        assert_eq!(fmt_time(2_500_000), "2.50 ms");
    }

    #[test]
    fn the_seeded_markdown_is_at_least_64_kib() {
        assert!(markdown_64k().len() >= 64 * 1024);
    }

    #[test]
    fn the_seeded_config_is_the_real_config_shape() {
        let config: Config = serde_json::from_str(CONFIG_JSON).expect("seeded config parses");
        assert_eq!(config.max_steps, 40);
        assert_eq!(config.model, "gpt-5-codex");
    }

    #[test]
    fn the_sample_message_round_trips() {
        let message = sample_message();
        let raw = serde_json::to_string(&message).expect("serialises");
        let decoded: Message = serde_json::from_str(&raw).expect("parses");
        assert_eq!(decoded, message);
    }
}
