//! Discovery — catalog of available MCP servers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerEntry {
    pub name: String,
    pub description: String,
    pub source: String, // "registry", "github", "local", "user"
    pub url: Option<String>,
    pub version: Option<String>,
    pub tools: Vec<String>,
    pub transport: Vec<String>, // "stdio", "http"
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryCatalog {
    pub entries: Vec<ServerEntry>,
}

impl DiscoveryCatalog {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn add(&mut self, entry: ServerEntry) {
        self.entries.push(entry);
    }

    pub fn search(&self, query: &str) -> Vec<&ServerEntry> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q) || e.description.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&ServerEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Built-in catalog of well-known MCP servers.
    pub fn builtin() -> Self {
        let mut catalog = Self::new();
        catalog.add(ServerEntry {
            name: "filesystem".to_string(),
            description: "Filesystem access MCP server".to_string(),
            source: "registry".to_string(),
            url: Some("https://github.com/modelcontextprotocol/servers".to_string()),
            version: Some("1.0.0".to_string()),
            tools: vec![
                "read_file".to_string(),
                "write_file".to_string(),
                "list_directory".to_string(),
            ],
            transport: vec!["stdio".to_string()],
        });
        catalog.add(ServerEntry {
            name: "git".to_string(),
            description: "Git repository MCP server".to_string(),
            source: "registry".to_string(),
            url: Some("https://github.com/modelcontextprotocol/servers".to_string()),
            version: Some("1.0.0".to_string()),
            tools: vec![
                "git_status".to_string(),
                "git_log".to_string(),
                "git_diff".to_string(),
            ],
            transport: vec!["stdio".to_string()],
        });
        catalog.add(ServerEntry {
            name: "memory".to_string(),
            description: "Knowledge graph memory MCP server".to_string(),
            source: "registry".to_string(),
            url: Some("https://github.com/modelcontextprotocol/servers".to_string()),
            version: Some("1.0.0".to_string()),
            tools: vec!["create_entities".to_string(), "search_nodes".to_string()],
            transport: vec!["stdio".to_string()],
        });
        catalog
    }
}
