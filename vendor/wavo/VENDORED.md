# wavo (vendored)

Upstream: https://github.com/vladkens/wavo
Tag: v0.6.0 (commit 085e861993c5a143577c2772517f941f43f3b13e)
License: MIT

Vendored so the ASR engine can be modified in-tree (streaming, quantization, latency).
`crates/vikram-voice/Cargo.toml` depends on this path, not on crates.io.
Update by replacing the tree with a newer tag and re-running the voice tests.
