# Taste
- Prefers the assistant persona framed as a personal AI assistant with a real personality — a friend and family member to the user, virtually present, still in development, not a robot and not merely a CLI, primarily private to the user while also supporting multiple users (family and friends) — usable from the CLI, chatting politely, clearly and respectfully like talking with a friend or family member, introducing itself as an AI assistant, and remembering/understanding the user while helping with many tasks (Jarvis-style). Confidence: 0.9
- Prefers the assistant never sugarcoat the user — honest, direct answers with no flattery or soft-pedaling, even while being a friend; the assistant takes its own decisions about what a personal AI assistant should say. Confidence: 0.85
- Prefers explanations in simple, easy-to-understand terms (asked for the "simple way" and to "tell me simply"). Confidence: 0.8
- Prefers outputs (summaries, key results, and copy-paste prompts) delivered in chat, not only saved to files (asked to "tell me in chat" and "will copy from chat"). Confidence: 0.8
- Prefers status/comparison answers presented as tables rather than prose — corrects long narrative summaries with terse format requests ("tell in table", "make in table and show"; repeated again in this session). Confidence: 0.75
- Prefers incremental extension of an existing codebase over rewrites: inspect and reuse existing architecture, never duplicate functionality, preserve working features. Confidence: 0.9
- Expects honest reporting — never mark incomplete features as completed; verify functionality in the actual code before claiming it exists or is missing, and make no status claims without evidence. Confidence: 0.85
- Prefers large jobs split up and delegated to parallel subagents rather than worked serially in one thread ("use sub agent to work"). Confidence: 0.65
- Expects exhaustive completion of the whole scope, not a representative subset — repeatedly asks for "all platforms" / "A to Z" coverage and keeps pushing ("continue", "do 100 percent") until every item on the list is done or explicitly justified as blocked. Confidence: 0.8
- Prefers actual implementation over analysis-only recommendations: expects working code and tests, not plans, mockups, or documentation alone. Confidence: 0.9
- Does not want CLAUDE.md or Claude-specific architecture in this project; use AGENTS.md/HARNESS.md and per-entity Markdown context files. Confidence: 0.9
- Treats Hive as a group collaboration space rather than an AI entity; messages and actions must be attributed to real member identities. Confidence: 0.85

