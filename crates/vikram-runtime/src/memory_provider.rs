use crate::memory::MemoryStore;
use crate::memory_files::{MemoryFile, memory_file, user_file};
use serde_json::Value;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

pub trait MemoryProvider: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    fn initialize(&self, session_id: &str);
    fn system_prompt_block(&self) -> String {
        String::new()
    }
    fn prefetch(&self, query: &str, session_id: &str) -> String {
        let _ = (query, session_id);
        String::new()
    }
    fn queue_prefetch(&self, query: &str, session_id: &str) {
        let _ = (query, session_id);
    }
    fn sync_turn(&self, user_content: &str, assistant_content: &str, session_id: &str) {
        let _ = (user_content, assistant_content, session_id);
    }
    fn get_tool_schemas(&self) -> Vec<Value> {
        Vec::new()
    }
    fn handle_tool_call(&self, tool_name: &str, args: &Value) -> String {
        let _ = (tool_name, args);
        serde_json::json!({"error": "not handled"}).to_string()
    }
    fn shutdown(&self) {}
    fn on_turn_start(&self, turn_number: usize, message: &str) {
        let _ = (turn_number, message);
    }
    fn on_session_end(&self, messages: Vec<Value>) {
        let _ = messages;
    }
    fn on_memory_write(&self, action: &str, target: &str, content: &str) {
        let _ = (action, target, content);
    }
}

pub struct BuiltInMemoryProvider {
    store: MemoryStore,
    memory_file: Mutex<MemoryFile>,
    user_file: Mutex<MemoryFile>,
    session_id: Mutex<Option<String>>,
}

impl BuiltInMemoryProvider {
    pub fn new(store: MemoryStore) -> Self {
        let home = crate::config::vikram_home();
        Self {
            store,
            memory_file: Mutex::new(memory_file(&home)),
            user_file: Mutex::new(user_file(&home)),
            session_id: Mutex::new(None),
        }
    }

    pub fn with_files(store: MemoryStore, memory: MemoryFile, user: MemoryFile) -> Self {
        Self {
            store,
            memory_file: Mutex::new(memory),
            user_file: Mutex::new(user),
            session_id: Mutex::new(None),
        }
    }

    pub fn from_store_and_files(store: MemoryStore, memory: MemoryFile, user: MemoryFile) -> Self {
        Self::with_files(store, memory, user)
    }
}

impl MemoryProvider for BuiltInMemoryProvider {
    fn name(&self) -> &str {
        "builtin"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn initialize(&self, session_id: &str) {
        if let Ok(mut guard) = self.session_id.lock() {
            *guard = Some(session_id.to_owned());
        }
    }

    fn system_prompt_block(&self) -> String {
        let mut parts = Vec::new();
        if let Ok(mut mf) = self.memory_file.lock() {
            let snap = mf.snapshot();
            if !snap.trim().is_empty() {
                parts.push(format!("MEMORY:\n{}", snap));
            }
        }
        if let Ok(mut uf) = self.user_file.lock() {
            let snap = uf.snapshot();
            if !snap.trim().is_empty() {
                parts.push(format!("USER:\n{}", snap));
            }
        }
        parts.join("\n\n")
    }

    fn prefetch(&self, query: &str, session_id: &str) -> String {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let mut lines = Vec::new();
        if !terms.is_empty() {
            let scopes = vec!["global".to_owned(), session_id.to_owned()];
            if let Ok(entries) = self.store.recall_terms(&terms, &scopes, 8) {
                for (scope, key, value) in entries {
                    lines.push(format!("- [{scope}] {key}: {value}"));
                }
            }
        }
        if let Ok(mf) = self.memory_file.lock() {
            for entry in mf.load() {
                let lower = entry.to_lowercase();
                if terms.iter().any(|t| lower.contains(t)) {
                    lines.push(format!("- [memory] memory: {}", entry));
                }
            }
        }
        if let Ok(uf) = self.user_file.lock() {
            for entry in uf.load() {
                let lower = entry.to_lowercase();
                if terms.iter().any(|t| lower.contains(t)) {
                    lines.push(format!("- [user] user: {}", entry));
                }
            }
        }
        lines.join("\n")
    }

    fn queue_prefetch(&self, _query: &str, _session_id: &str) {}

    fn sync_turn(&self, _user_content: &str, _assistant_content: &str, _session_id: &str) {}

    fn get_tool_schemas(&self) -> Vec<Value> {
        Vec::new()
    }

    fn handle_tool_call(&self, _tool_name: &str, _args: &Value) -> String {
        serde_json::json!({"error": "builtin provider has no tools"}).to_string()
    }

    fn shutdown(&self) {}

    fn on_turn_start(&self, _turn_number: usize, _message: &str) {}

    fn on_session_end(&self, _messages: Vec<Value>) {}

    fn on_memory_write(&self, _action: &str, _target: &str, _content: &str) {}
}

#[derive(Default)]
pub struct MemoryProviderManager {
    providers: Vec<Box<dyn MemoryProvider>>,
    tool_providers: HashMap<String, String>,
    has_external: bool,
}

impl MemoryProviderManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_provider(&mut self, provider: Box<dyn MemoryProvider>) {
        let is_builtin = provider.name() == "builtin";
        let is_external = !is_builtin;
        if is_external && self.has_external {
            return;
        }
        if is_external {
            self.has_external = true;
        }
        for raw in provider.get_tool_schemas() {
            if let Some(name) = raw.get("name").and_then(Value::as_str) {
                if !self.tool_providers.contains_key(name) {
                    self.tool_providers
                        .insert(name.to_owned(), provider.name().to_owned());
                }
            } else if let Some(function) = raw.get("function")
                && let Some(name) = function.get("name").and_then(Value::as_str)
                && !self.tool_providers.contains_key(name)
            {
                self.tool_providers
                    .insert(name.to_owned(), provider.name().to_owned());
            }
        }
        if is_builtin {
            self.providers.insert(0, provider);
        } else {
            self.providers.push(provider);
        }
    }

    pub fn add_external(&mut self, provider: Box<dyn MemoryProvider>) -> crate::Result<()> {
        if provider.name() == "builtin" {
            self.add_provider(provider);
            return Ok(());
        }
        if self.has_external {
            return Err(crate::RuntimeError::InvalidState(
                "external memory provider already registered".into(),
            ));
        }
        self.add_provider(provider);
        Ok(())
    }

    pub fn register(&mut self, provider: Box<dyn MemoryProvider>) {
        self.add_provider(provider);
    }

    pub fn providers(&self) -> &[Box<dyn MemoryProvider>] {
        &self.providers
    }

    pub fn get_provider(&self, name: &str) -> Option<&dyn MemoryProvider> {
        self.providers
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.as_ref())
    }

    pub fn initialize_all(&self, session_id: &str) {
        for p in &self.providers {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| p.initialize(session_id)));
        }
    }

    pub fn system_prompt_block(&self) -> String {
        self.build_system_prompt()
    }

    pub fn build_system_prompt(&self) -> String {
        let mut blocks = Vec::new();
        for p in &self.providers {
            let p = p.as_ref();
            if let Ok(block) = catch_unwind(AssertUnwindSafe(|| p.system_prompt_block()))
                && !block.trim().is_empty()
            {
                blocks.push(block);
            }
        }
        blocks.join("\n\n")
    }

    pub fn prefetch_all(&self, query: &str, session_id: &str) -> String {
        self.prefetch(query, session_id)
    }

    pub fn prefetch(&self, query: &str, session_id: &str) -> String {
        let mut parts = Vec::new();
        for p in &self.providers {
            let p = p.as_ref();
            if let Ok(result) = catch_unwind(AssertUnwindSafe(|| p.prefetch(query, session_id)))
                && !result.trim().is_empty()
            {
                parts.push(result);
            }
        }
        parts.join("\n\n")
    }

    pub fn queue_prefetch_all(&self, query: &str, session_id: &str) {
        self.queue_prefetch(query, session_id);
    }

    pub fn queue_prefetch(&self, query: &str, session_id: &str) {
        for p in &self.providers {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| p.queue_prefetch(query, session_id)));
        }
    }

    pub fn sync_all(&self, user_content: &str, assistant_content: &str, session_id: &str) {
        for p in &self.providers {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| {
                p.sync_turn(user_content, assistant_content, session_id)
            }));
        }
    }

    pub fn get_all_tool_schemas(&self) -> Vec<Value> {
        self.tool_schemas()
    }

    pub fn tool_schemas(&self) -> Vec<Value> {
        let mut schemas = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            let p = p.as_ref();
            if let Ok(raw_schemas) = catch_unwind(AssertUnwindSafe(|| p.get_tool_schemas())) {
                for raw in raw_schemas {
                    let schema = if raw.get("type").and_then(Value::as_str) == Some("function")
                        && raw.get("function").is_some()
                    {
                        raw.get("function").cloned().unwrap_or(raw)
                    } else {
                        raw
                    };
                    if let Some(name) = schema.get("name").and_then(Value::as_str)
                        && !seen.contains(name)
                    {
                        seen.insert(name.to_owned());
                        schemas.push(schema);
                    }
                }
            }
        }
        schemas
    }

    pub fn handle_tool_call(&self, tool_name: &str, args: &Value) -> String {
        let provider_name = match self.tool_providers.get(tool_name) {
            Some(name) => name.clone(),
            None => {
                return serde_json::json!({"error": format!("no memory provider handles tool '{}'", tool_name)})
                    .to_string()
            }
        };
        for p in &self.providers {
            if p.name() == provider_name {
                let p = p.as_ref();
                return catch_unwind(AssertUnwindSafe(|| p.handle_tool_call(tool_name, args)))
                    .unwrap_or_else(|_| {
                        serde_json::json!({"error": "memory tool failed"}).to_string()
                    });
            }
        }
        serde_json::json!({"error": format!("no memory provider handles tool '{}'", tool_name)})
            .to_string()
    }

    pub fn on_turn_start(&self, turn_number: usize, message: &str) {
        for p in &self.providers {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| p.on_turn_start(turn_number, message)));
        }
    }

    pub fn on_session_end(&self, messages: Vec<Value>) {
        for p in &self.providers {
            let p = p.as_ref();
            let msgs = messages.clone();
            let _ = catch_unwind(AssertUnwindSafe(|| p.on_session_end(msgs)));
        }
    }

    pub fn on_memory_write(&self, action: &str, target: &str, content: &str) {
        let external: Vec<&Box<dyn MemoryProvider>> = self
            .providers
            .iter()
            .filter(|p| p.name() != "builtin")
            .collect();
        for p in external {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| {
                p.on_memory_write(action, target, content)
            }));
        }
    }

    pub fn shutdown_all(&self) {
        for p in self.providers.iter().rev() {
            let p = p.as_ref();
            let _ = catch_unwind(AssertUnwindSafe(|| p.shutdown()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProvider {
        name: String,
        available: bool,
        sp_calls: Arc<AtomicUsize>,
        prefetch_calls: Arc<AtomicUsize>,
        sync_calls: Arc<AtomicUsize>,
        turn_calls: Arc<AtomicUsize>,
        session_calls: Arc<AtomicUsize>,
        write_calls: Arc<AtomicUsize>,
        shutdown_calls: Arc<AtomicUsize>,
        panic_sp: bool,
    }

    impl MockProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                available: true,
                sp_calls: Arc::new(AtomicUsize::new(0)),
                prefetch_calls: Arc::new(AtomicUsize::new(0)),
                sync_calls: Arc::new(AtomicUsize::new(0)),
                turn_calls: Arc::new(AtomicUsize::new(0)),
                session_calls: Arc::new(AtomicUsize::new(0)),
                write_calls: Arc::new(AtomicUsize::new(0)),
                shutdown_calls: Arc::new(AtomicUsize::new(0)),
                panic_sp: false,
            }
        }
    }

    impl MemoryProvider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn initialize(&self, _session_id: &str) {}
        fn system_prompt_block(&self) -> String {
            if self.panic_sp {
                panic!("boom");
            }
            self.sp_calls.fetch_add(1, Ordering::SeqCst);
            format!("block:{}", self.name)
        }
        fn prefetch(&self, _query: &str, _session_id: &str) -> String {
            self.prefetch_calls.fetch_add(1, Ordering::SeqCst);
            format!("prefetch:{}", self.name)
        }
        fn queue_prefetch(&self, _query: &str, _session_id: &str) {}
        fn sync_turn(&self, _u: &str, _a: &str, _s: &str) {
            self.sync_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn get_tool_schemas(&self) -> Vec<Value> {
            if self.name == "builtin" {
                Vec::new()
            } else {
                vec![
                    serde_json::json!({"name": format!("{}_tool", self.name), "description": "d", "parameters": {}}),
                ]
            }
        }
        fn handle_tool_call(&self, tool_name: &str, _args: &Value) -> String {
            serde_json::json!({"ok": true, "tool": tool_name, "provider": self.name}).to_string()
        }
        fn shutdown(&self) {
            self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn on_turn_start(&self, _n: usize, _m: &str) {
            self.turn_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn on_session_end(&self, _messages: Vec<Value>) {
            self.session_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn on_memory_write(&self, _a: &str, _t: &str, _c: &str) {
            self.write_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn builtin_provider_basics() {
        let store = MemoryStore::open_in_memory().unwrap();
        let p = BuiltInMemoryProvider::new(store);
        assert_eq!(p.name(), "builtin");
        assert!(p.is_available());
        p.initialize("s1");
    }

    #[test]
    fn builtin_with_files() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut mf = memory_file(home);
        let mut uf = user_file(home);
        mf.add("I like Rust").unwrap();
        uf.add("user pref").unwrap();
        let store = MemoryStore::open_in_memory().unwrap();
        let p = BuiltInMemoryProvider::with_files(store, mf, uf);
        let block = p.system_prompt_block();
        assert!(block.contains("Rust"));
    }

    #[test]
    fn manager_builtin_first_and_one_external() {
        let mut mgr = MemoryProviderManager::new();
        let ext1 = MockProvider::new("ext1");
        let ext2 = MockProvider::new("ext2");
        let builtin = BuiltInMemoryProvider::new(MemoryStore::open_in_memory().unwrap());
        mgr.add_provider(Box::new(ext1));
        mgr.add_provider(Box::new(builtin));
        mgr.add_provider(Box::new(ext2));
        let names: Vec<&str> = mgr.providers().iter().map(|p| p.name()).collect();
        assert_eq!(names, vec!["builtin", "ext1"]);
        assert_eq!(mgr.providers().len(), 2);
    }

    #[test]
    fn manager_fan_out_and_exception_isolation() {
        // The manager admits one external provider (see `manager_builtin_first_and_one_external`),
        // so isolation is proven with one panicking provider, then with one healthy provider.
        let mut mgr = MemoryProviderManager::new();
        let mut p1 = MockProvider::new("p1");
        p1.panic_sp = true;
        let p1_sp = p1.sp_calls.clone();
        let builtin = BuiltInMemoryProvider::new(MemoryStore::open_in_memory().unwrap());
        mgr.add_provider(Box::new(builtin));
        mgr.add_provider(Box::new(p1));
        let block = mgr.system_prompt_block();
        assert!(
            block.is_empty(),
            "a panicking provider contributes no block"
        );
        assert_eq!(p1_sp.load(Ordering::SeqCst), 0);

        let mut healthy = MemoryProviderManager::new();
        let p2 = MockProvider::new("p2");
        let p2_sp = p2.sp_calls.clone();
        healthy.add_provider(Box::new(BuiltInMemoryProvider::new(
            MemoryStore::open_in_memory().unwrap(),
        )));
        healthy.add_provider(Box::new(p2));
        let block = healthy.system_prompt_block();
        assert!(block.contains("block:p2"));
        assert_eq!(p2_sp.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manager_tool_routing() {
        let mut mgr = MemoryProviderManager::new();
        let ext = MockProvider::new("ext");
        mgr.add_provider(Box::new(ext));
        let builtin = BuiltInMemoryProvider::new(MemoryStore::open_in_memory().unwrap());
        mgr.add_provider(Box::new(builtin));
        let schemas = mgr.get_all_tool_schemas();
        assert_eq!(schemas.len(), 1);
        let res = mgr.handle_tool_call("ext_tool", &serde_json::json!({}));
        assert!(res.contains("ext"));
    }

    #[test]
    fn manager_hooks_fan_out() {
        let mut mgr = MemoryProviderManager::new();
        let ext = MockProvider::new("ext");
        let turn = ext.turn_calls.clone();
        let session = ext.session_calls.clone();
        let write = ext.write_calls.clone();
        let sync = ext.sync_calls.clone();
        let shutdown = ext.shutdown_calls.clone();
        mgr.add_provider(Box::new(ext));
        let builtin = BuiltInMemoryProvider::new(MemoryStore::open_in_memory().unwrap());
        mgr.add_provider(Box::new(builtin));
        mgr.sync_all("u", "a", "s");
        mgr.on_turn_start(1, "hi");
        mgr.on_session_end(vec![serde_json::json!({"role":"user"})]);
        mgr.on_memory_write("add", "memory", "content");
        mgr.shutdown_all();
        assert_eq!(sync.load(Ordering::SeqCst), 1);
        assert_eq!(turn.load(Ordering::SeqCst), 1);
        assert_eq!(session.load(Ordering::SeqCst), 1);
        assert_eq!(write.load(Ordering::SeqCst), 1);
        assert_eq!(shutdown.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manager_prefetch() {
        let mut mgr = MemoryProviderManager::new();
        let ext = MockProvider::new("ext");
        mgr.add_provider(Box::new(ext));
        let out = mgr.prefetch_all("hello", "s");
        assert!(out.contains("prefetch:ext"));
    }

    #[test]
    fn add_external_error() {
        let mut mgr = MemoryProviderManager::new();
        let e1 = MockProvider::new("e1");
        let e2 = MockProvider::new("e2");
        mgr.add_external(Box::new(e1)).unwrap();
        assert!(mgr.add_external(Box::new(e2)).is_err());
    }
}
