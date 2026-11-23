# JARVIS capability ledger

The operator's 85-row capability table, answered against the code that exists on
2026-11-23. Every `have` and `partial` row carries a `path:line` that was read, not
remembered. Rows 71-85 are the film list; they are answered too, because "we cannot do
this and here is why" is a real answer.

Status values:

| status | meaning |
|---|---|
| `have` | wired and reachable in production |
| `partial` | works, but a named piece is missing or unreachable |
| `declared` | types, tests or a crate exist; nothing calls them |
| `missing` | nothing in the tree |
| `never` | no data source or hardware exists, and adding one is out of scope |

## 1. Intelligence, memory, self-knowledge

| # | capability | status | evidence | gap |
|---|---|---|---|---|
| 1 | Intelligence | have | `crates/vikram-runtime/src/turn.rs:2884` builds the completion request, `:755` streams it | — |
| 2 | Decision Making | partial | `turn.rs:2292` `policy().evaluate_scoped` returns a `Decision` | only permission verdicts; the task-level choice is the model's |
| 3 | Problem Solving | partial | `turn.rs:1060` loops provider rounds until no tool calls; `verify` now feeds the repair loop a real instruction — failing step, command, `FailureKind`, the files the error points at, a bounded excerpt — and a returned path list buys the next attempt (`tools/verify.rs`, tests `a_failed_verify_attempts_a_repair_and_reports_each_attempt`, `a_timed_out_attempt_does_not_retry`) | the tool cannot edit: it hands the paths back and the model must act on them, so a deterministic failure repeats the same way until the next turn |
| 4 | Planning | have | `tools/process.rs:459` update_plan; `turn.rs:192` work contract, `:586` PlanUpdated | plan is advisory, not step-enforced |
| 5 | Long-Term Memory | have | `memory.rs:161`; recall `runtime.rs:1397`; injected `turn.rs:2694`; files `memory_files.rs` | — |
| 6 | Short-Term Memory | have | `turn.rs:624`; `runtime.rs:1719` should_compact, `:1883` model-written fold | — |
| 7 | Episodic Memory | have | `learning.rs:817` records an `Experience`; `runtime.rs:1510` injects recent ones | — |
| 8 | Learning | have | `learning.rs:770` on_turn_end; `runtime.rs:2798` model review; `learning.rs:1574` apply_or_stage; an applied write records key, scope and the value it replaced, so `vikram learning undo <id>` restores or removes it (tests `an_auto_applied_memory_write_can_be_undone`, `undoing_a_write_with_no_previous_value_removes_the_entry`) | auto-apply only under policy `Auto`; undo covers memory writes, not skill or instruction changes |
| 9 | Personality | have | `turn.rs:163` preamble, `:223` bot soul; `runtime.rs:1086` persona skills | — |
| 10 | Emotional Intelligence | partial | tone is read from explicit first-person words only: `Tone { Frustrated, Tired, Pleased, Urgent }` + `explicit_tone` in `dialectic.rs`, stored as `InsightKind::Tone` and decayed like any other insight; tests `explicit_tone_phrases_are_recognised`, `punctuation_does_not_create_a_tone_signal`, `a_negated_feeling_is_not_a_tone` | no affect model: nothing infers a mood, nothing reads tone from voice, and no behaviour changes beyond the supplement line |
| 11 | Natural Conversation | have | `turn.rs:755` stream, `:848` TextDelta | — |
| 41 | Self-Verification | partial | `tools/core.rs:450` registers VerifyRun; `tools/verify.rs` build+test with git evidence and a repair hook that parses the failing paths so the loop can retry; a turn that edits and never verifies carries an `unverified edits:` line and one notice (`turn.rs`); a reply claiming a path the diff never touched gets one advisory warning | still opt-in: nothing *forces* a verify after an edit, and the claim check reads paths, not prose |
| 42 | Self-Correction | have | `turn.rs:1507` hands a failed `verify`'s own repair instruction back as one `Trust::Host` message in the same turn: the step, command, `FailureKind`, named files and bounded excerpt are read from the tool result's report (`:1750`), and `AutoRepair` (`:1785`) allows one such attempt per turn — skipped on `timeout`, with no named file, once the step budget is spent, or when the turn is cancelled or out of worker rounds; a later failure says the repair did not hold instead of retrying (tests `a_failed_verify_is_handed_back_to_the_model_once`, `a_second_failing_verify_in_the_same_turn_is_not_retried`, `a_timed_out_verify_is_not_retried_automatically`, `a_repair_with_no_named_file_is_not_handed_back`). `RepeatGuard` (`:1657`) still advises after three failures in a row, and `:912` drops the oldest turn on a context-limit rejection | the automatic fix covers a failed `verify` only: other tool failures still get advice, and a failure that survives the one handed-back attempt is reported, not repaired further |
| 62 | Model Routing | partial | `cross_check` (`crates/vikram-runtime/src/tools/cross_check.rs`) compares two models a caller names, and `self_trial` measures a settings change and rolls it back when the benchmark got worse | routing is still the operator's or the persona's choice: nothing picks a model automatically, because `select_model_for_task` only substring-matches a caller-supplied list |
| 63 | Multi-Model Intelligence | partial | `orchestrator.rs:583` per-worker model; `cross_check` asks two models one question and shows both answers with lexical divergence evidence; `self_trial` keeps a change only when the benchmark agrees | no synthesis and no judge: two answers are shown side by side, never merged into one verdict |
| 66 | Self-Diagnostics | have | `runtime.rs:2391` hourly self-check; `:2584` MAPE-K; `:2737` issues name their component | store-derived only |
| 67 | Self-Optimization | partial | the host attaches a `ConfigSurface` with exactly three writable keys (`context:memory_budget_tokens`, `runtime:learning_memory_policy`, `runtime:learning_skill_policy`); `self_trial::trial_change` writes one, re-measures the benchmark and rolls it back when any metric got worse (`crates/vikram-evolution/src/self_trial.rs`); `vikram evolution trial <key> <value> <benchmark>` drives it. Tests `a_self_change_that_worsens_the_benchmark_is_rolled_back`, `a_self_change_that_improves_the_benchmark_is_kept` | nothing proposes a change on its own: a trial needs an operator-supplied key, value and benchmark, and only the config file's memory budget is writable from the CLI |
| 68 | Digital Twin | partial | two models exist. The person: `entities.profile_id` written and read (`vikram-store` `list_entities_for_profile`), widened `entities_slug` index, `dialectic.rs` preferences; tests `an_entity_created_for_a_profile_is_listed_only_there`, `two_profiles_can_each_own_an_entity_with_the_same_slug`. The machine: `HostProfile` in `crates/vikram-evolution/src/mapek.rs` (`cpus`, `memory_total_bytes`, one filesystem's total/free, `failed_services`, `default_route`, `captured_ms`), captured by the existing probes, persisted once at `mapek.host` / `profile`, sent to the model as the `machine` line of every prompt (`RuntimeInner::machine_line`, `Trust::Host`, frozen per process), rendered by `describe_host_profile` as `machine: 8 CPUs, 15.2 GiB RAM, 19.7 GiB free of 474.9 GiB, 0 failed services, network up`; unknown facts are omitted, never printed as zero; tests `a_host_profile_names_what_the_probes_saw`, `unknown_facts_are_left_out_of_the_line`, `a_host_profile_survives_a_store_reopen`, `the_machine_line_reaches_the_prompt` | the profile is captured once and never refreshed, one filesystem only, and nothing models the environment beyond this host |

## 2. Voice and perception

| # | capability | status | evidence | gap |
|---|---|---|---|---|
| 12 | Voice Recognition | have | `crates/vikram-voice/src/asr.rs:241` wavo GGUF; `session.rs:840` listen_once | default model `whisper-base.en` — English only |
| 13 | Voice Synthesis | have | `session.rs:658` piper, espeak-ng fallback; `terminal.rs:574` speaks replies | espeak-ng is the fresh-machine default (robotic) |
| 14 | Wake Word | missing | `voice/src/lib.rs:66` has no wake field; activation is `Ctrl+G` (`tui/src/input.rs:156`) or `/voice` | needs an ONNX wake-word engine that is not in the tree |
| 15 | Real-Time Voice | partial | live captions `session.rs:857`; replies stop mid-sentence: `VoiceEvent::Ready` hands the host a `CancelToken`, `hush` cancels it on a new turn (`crates/vikram-cli/src/terminal.rs`), test `an_interrupted_reply_stops_the_token_that_is_speaking` | barge-in by voice is not there: the mic is still closed while speaking, because without echo cancellation an open mic transcribes the assistant |
| 16 | Multilingual | have | `asr.rs:231` accepts `language`; `lib.rs` `asr_language`; `lib.rs` `tts_language` → espeak `-v` and the piper voice whose file name carries the tag (`session.rs` `pick_voice_for`); tests `a_language_tag_picks_the_voice_that_speaks_it` | a model per language is still a download, not a switch |
| 17 | Computer Vision | have | `--image` (`main.rs:81`), `chat.rs:82`, `openai.rs:903`, `anthropic.rs:586` | only for vision-capable models; no capability check |
| 18 | Facial Recognition | missing | no face or embedding code | needs a face model plus a camera source |
| 19 | Object Recognition | missing | only tesseract text (`computer.rs:515`) | no detector |
| 20 | Screen Understanding | have | a screenshot taken by a tool reaches the next request as a real image block (`turn.rs` `remember_screenshot` / `take_pending_screenshot`, `Block::Image` + `ImageSource::Data`), attached once, dropped after 3 turns, capped at 1.5 MiB and labelled with the tool and path; OpenAI and Anthropic both translate it; tests `a_screenshot_from_the_previous_step_reaches_the_next_request`, `a_screenshot_is_attached_once_and_then_forgotten`, `an_oversized_image_is_not_attached`, `a_turn_without_a_screenshot_sends_no_image`, `a_model_that_cannot_see_images_is_told_about_the_screenshot` | one image at a time and only from a screenshot tool; vision support is a model-name allowlist, so an unlisted vision model gets a text note instead of pixels |
| 21 | Live Camera | missing | no camera code; `capture.rs:155` opens microphones | no v4l path |
| 22 | Video Understanding | missing | `gateway/src/media_fetch.rs:36` skips video and audio | never read |
| 23 | Audio Perception | missing | `voice/src/vad.rs:5` is an energy gate, not a classifier | no sound classification |
| 24 | Spatial Awareness | missing | no sensor or position code | no data source exists |

## 3. Web, documents, automation, development

| # | capability | status | evidence | gap |
|---|---|---|---|---|
| 25 | Internet Access | have | `tools/web.rs:70` WebSearch → `search.rs:71`; `tools/browser.rs:45` GET + strip; registered `tools/core.rs:461` | no generic POST; search scrapes Bing HTML |
| 26 | Deep Research | have | `tools/research.rs` fans out up to three query variants, fetches at most five pages, dedupes by URL and returns one source-tagged digest; results are ranked (variants that reached a page, then term hits, then fetch order) and a marker clash between two pages is reported as `possible disagreement:` with both URLs; tests `a_source_reached_by_two_queries_ranks_above_one_reached_by_one`, `two_sources_that_disagree_are_reported_as_a_conflict`, `sources_that_agree_produce_no_conflict_line` | the clash detector is lexical marker matching, not a proven contradiction, and there is no page-level credibility ranking |
| 27 | Knowledge Base | have | `memory.rs:409` hybrid recall, `:1113` FTS5, `:661` consolidate; relation graph `lib.rs:4612` | the cosine quarter of recall is structurally zero — no embedder |
| 28 | Document Intelligence | have | `tools/doc.rs` `doc.read`: PDF through `pdftotext -layout`, DOCX through unzip + XML, XLSX through shared strings and sheet names; workspace-confined; tests `a_path_outside_the_workspace_is_refused`, `a_missing_helper_names_the_package_to_install` | XLSX cell addressing is not implemented; scanned PDFs need OCR first |
| 29 | Browser Automation | have | `browser/cdp.rs:201` open, `:324` mouse, `:565` insertText; 12 tools | fresh empty profile per session (`cdp.rs:666`) — no logged-in browsing |
| 30 | Computer Automation | have | `computer.rs:588` click, `:640` type, `:1009` screenshot, `:1456` register | Linux only; Wayland partial; no element tree |
| 31 | Terminal Control | have | `tools/shell.rs:128` run, `:191` persistent PTY; sandbox wrap `:42`; `terminal.exec` (`tools/terminal.rs`) reaches the ssh and docker backends of `vikram-terminals` | no file transfer; no interactive docker shell |
| 32 | File Management | have | `tools/fs.rs:92,134,209,305,427` read/list/write/edit/delete | no move/rename/copy tool |
| 33 | Coding | have | `tools/lsp.rs:355` real rust-analyzer; `codeindex.rs:508` tree-sitter; `tools/verify.rs` | LSP limited to its server table |
| 34 | Software Development | have | `shell.rs:415` build, `:463` test; `verification.rs:762` strict verify | build detection covers four ecosystems |
| 59 | IoT Integration | missing | no device module; no mqtt/serial/gpio code | nothing |
| 60 | Robotics | missing | no hits outside prose | nothing |
| 61 | Navigation | partial | web navigation `cdp.rs:865`, `tools/browser.rs:242` | no maps, routes or physical navigation |
| 64 | Dynamic Tool Creation | have | a manifest declares its tools (`PluginManifest.tools: Vec<PluginTool>`, `plugins/manifest.rs`); each becomes `plugin.<name>` in the next turn's schema via `ToolRegistry::with_plugins` (`tools/core.rs`), tests `a_declared_plugin_tool_is_advertised_under_its_own_name`, `a_declared_plugin_tool_is_hidden_in_plan_and_offered_unattended` | nothing creates a tool from model output: a plugin is installed by the operator or by `plugin.install` |
| 65 | Skill Acquisition | have | `skills/discovery.rs:536`; `install.rs:59`; proposals write skills `learning.rs:1799` | produces procedure documents, not executable tools |
| 69 | Multi-Device Presence | partial | gateway host loop over 28 adapters; daemon user service `service.rs:251` | one session per chat key (`gateway.rs:1214`); no device registry or hand-off |
| 70 | Emergency Response | partial | three actuators: re-queue reflection (`mapek.rs:1234`); the maintenance job puts the daemon to doze on a >=95% disk and wakes it below 90% only if that actuator set the doze (`presence.reason = "disk"`, `crates/vikram-daemon/src/runner.rs` `react_to_disk`), tests `a_full_disk_puts_the_daemon_to_doze_once`, `a_hand_set_doze_is_not_reverted_by_a_recovered_disk`, `an_unreadable_disk_changes_nothing`; heartbeat flags stuck jobs (`heartbeat.rs:315`), urgent bypass `triggers.rs:23`, webhook `notify.rs:148` | the only self-protective action is to slow down and speak: nothing restarts a failed service, frees disk, or rolls back a bad deploy without a declared `deploy.run` plan |

## 4. Agents, autonomy, operations, security

| # | capability | status | evidence | gap |
|---|---|---|---|---|
| 35 | Autonomous Agents | have | `daemon/runner.rs:112` run_agent, `:88` `unattended: true` | — |
| 36 | Multi-Agent System | have | `runner.rs:413` hive roster; `runtime.rs:5842` hive_run | — |
| 37 | Dynamic Agent Creation | have | `orchestrator.rs:540` spawn_worker; `tools/hive_roster.rs:412` bot.create | — |
| 38 | Agent Delegation | have | `orchestrator.rs:251,675`; registered `tools/core.rs:467`; hive.delegate `hive_roster.rs:166` | — |
| 39 | Parallel Execution | have | `runtime.rs:6077` one spawn per participant; `runner.rs:568` | — |
| 40 | Agent Communication | have | `orchestrator.rs:502` notify_parent into the parent transcript; shared hive transcript `runtime.rs:6100`; workers now exchange notes: `WorkerRegistry::post_note`/`take_notes` over the durable `workers.record`, exposed as `worker.note` / `worker.notes` (Delegate gate), tests `a_worker_reads_a_note_from_a_sibling_once`, `a_note_posted_by_one_worker_is_read_once_by_another` | notes are asynchronous and take-once: no live channel, no broadcast |
| 43 | Background Operations | have | `daemon.rs:749` worker pool (`worker.rs:269`); loop `daemon.rs:993` | — |
| 44 | Persistent Autonomy | partial | jobs, runs, events and audit live in SQLite (`daemon/src/store.rs`), the worker team is durable too (`workers` table in `worker_store.rs`, boot restore in `RuntimeInner::load_workers`), and a stalled worker can be resumed from the model (`worker.resume` -> `RuntimeHandle::resume_worker`, refusing a non-partial worker with its status and listing the resumable ids when none is named); tests `a_worker_record_survives_a_reopen_and_loads_back`, `loading_workers_twice_does_not_duplicate`, `a_partial_worker_can_be_resumed_through_the_tool`, `resuming_a_finished_worker_is_refused_with_its_status` | a restored worker comes back as a record, not a running turn: in-flight model turns are abandoned, and only a `Partial` worker can be resumed |
| 45 | Task Scheduling | have | `cron.rs:106` parse, `:184` next; `jobs.rs:231`; `triggers.rs:140` EventBus | — |
| 46 | Proactive Assistance | partial | `heartbeat.rs:315` late/stuck, `:357` failure streaks, delivered `daemon.rs:957`; the hourly self-check now also watches the host: real mounted-filesystem fill (`df -Pk`, Medium at 90%, High at 95%), failed systemd services (`systemctl --failed`), and default-route loss, each reported once per crossing from a durable `mapek.*` baseline; `next scheduled: <job> in N hours` is named when a session opens (`RuntimeInner::next_scheduled_job`) | nothing anticipates: every signal comes from its own scheduler or the host's present state, and there is no model of what the operator is about to do |
| 47 | Predictive Assistance | partial | `likely_next_step` (`crates/vikram-runtime/src/learning.rs`) counts success -> next-first-tool pairs inside one agent/workspace sequence and answers only at 3+ occasions; `RuntimeHandle::create_session` prints `likely next step: ... after N of the last M ...`; tests `a_repeated_pattern_produces_a_likely_next_step`, `fewer_than_three_occasions_say_nothing`, `failed_outcomes_do_not_build_a_pattern`, `a_session_start_names_the_likely_next_step` | a pattern in history is evidence of what followed, not a prediction of what the operator will do; reads the last 50 experiences only |
| 48 | Notifications | have | `notify.rs:130` local/session/webhook, `:153` notified.log; `daemon.rs:808` | — |
| 49 | System Monitoring | have | `tools/system.rs` `system.status`: `/proc/loadavg`, `/proc/meminfo`, a real `df -Pk` per mounted path, the last stored verify verdict, the busiest five processes by resident bytes (`/proc/<pid>/statm` x 4096, `comm`), and an `nvidia-smi` GPU section capped at 8 rows; the whole report is bounded by the shared 20 KB `truncated()`; `mapek.rs:1039` keeps the hourly probes; tests `a_missing_disk_path_degrades_instead_of_failing`, `the_busiest_processes_are_listed_by_resident_memory`, `a_missing_gpu_tool_says_so_instead_of_failing`, `the_tool_is_registered_and_advertises_its_schema` | one reading, no history: no per-process CPU share, no process tree, no GPU on a machine without `nvidia-smi` |
| 50 | Network Monitoring | have | `mapek` reads the kernel routing table (`probe_default_route`) and then actually asks the host that routes it: an injectable `endpoint_probe` (default `TcpStream::connect_timeout(<gateway>:53, 1500 ms)`) treats `Ok` **and** `ConnectionRefused` as reachable, a timeout or unreachable as lost, and an unreadable table as unknown with no claim; both the route and the reachability state persist as durable transitions (`mapek.network`, `mapek.endpoint`, ids `network.default_route.lost`/`.restored`, `network.endpoint.lost`/`.restored`), reported once per crossing; tests `an_endpoint_timeout_is_reported_once`, `a_refused_connection_still_counts_as_reachable`, `an_unreadable_routing_table_is_not_a_lost_endpoint` | one gateway, one port: a reachable gateway says the local network works, not that the wider internet does, and `network_active` still reads `false` for an older test |
| 51 | Server Management | have | `terminal.exec` (`tools/terminal.rs`) with `backend=ssh` reaches `vikram-terminals/src/lib.rs:297`; a target cannot smuggle an ssh option (`a_target_cannot_smuggle_an_ssh_option`) | no file transfer, no persistent shell |
| 52 | Docker Management | have | `terminal.exec` with `backend=docker` reaches `vikram-terminals/src/lib.rs:211`; refusal under a denying policy is tested (`a_policy_that_denies_process_execution_refuses_the_call`) | one command per call — no ps/stop/logs wrapper |
| 53 | Database Management | have | `db.backup` in `crates/vikram-runtime/src/tools/db.rs`: `PRAGMA integrity_check` on the source, copy, then integrity again on the copy; refuses a directory source and an existing destination. Tests `a_backup_is_readable_and_passes_integrity`, `a_destination_inside_the_source_is_refused` | sqlite only: postgres/mysql logical dumps are not implemented, and the tool says so |
| 54 | Deployment | have | `deploy.run` (`crates/vikram-runtime/src/tools/deploy.rs`) runs a declared step list and refuses without a `rollback`, refuses a step that `looks_destructive`, and runs the rollback when a step fails; tests `a_deploy_without_a_rollback_command_is_refused`, `a_failed_deploy_runs_the_rollback`, `a_step_outside_the_workspace_cwd_is_refused` | deploys what the operator declares; no registry/blue-green awareness |
| 55 | Cybersecurity | partial | `policy/src/lib.rs:365` floor veto; `shell.rs:508` looks_destructive; sandbox `core.rs:83`; two observation sources: `system.threats` counts failed-auth lines over a window from journald or a log and groups them by source, and `assess` names the shape it sees (repeated address, coordinated scan, automated scanning, or typos); one thing does act on a host signal: the maintenance job dozes the daemon when the disk passes 95% (`runner.rs` `react_to_disk`) | prevention plus two reads and one slow-down: no file-integrity watch, no process anomaly, no IOC feed, and nothing blocks or blocks-and-reports on a login burst |
| 56 | Threat Detection | partial | `system.threats` (`crates/vikram-runtime/src/tools/threat.rs`) counts failed-auth lines over a window from journald or a named log, grouped by source; tests `a_failed_login_burst_is_reported_with_its_source`, `an_unreadable_log_is_reported_as_no_claim` | failed logins only: no IOC feeds, no file-integrity watch, no process anomaly |
| 57 | Access Control | have | `policy/src/lib.rs:365` veto, `:358` depth limit; per-tool `permission()` | — |
| 58 | Smart Home | missing | zero hits for mqtt/home-assistant/hue | nothing |

## 5. The film list (71-85)

These are answered, not promised. Four are reachable, the rest need hardware.

| # | capability | status | answer |
|---|---|---|---|
| 71 | Holographic Interface | never | no display surface but the terminal; a GPU overlay is a separate product |
| 72 | Stark Lab Management | never | no lab hardware to control |
| 73 | Suit Diagnostics | never | no wearable |
| 74 | Suit Control | never | no exoskeleton, and this is a safety line, not a feature gap |
| 75 | Tactical Analysis | have | a prompt plus a skill: plans, trade-offs, ranked options. `turn.rs:192` contract already asks for it |
| 76 | Threat Assessment | partial | `assess` in `crates/vikram-runtime/src/tools/threat.rs` labels what the count shape means: one address repeating (>=5) is called a likely brute-force attempt, >=3 addresses with >=2 tries each "looks like a coordinated scan", many single failures "looks like automated scanning", a clean spread is called what typos and stale credentials look like, and an unattributable log says the cause cannot be told; every branch hedges with "looks like" and never claims an attack or safety. Tests `one_source_repeating_is_named_a_brute_force_attempt`, `many_addresses_trying_twice_is_named_a_scan`, `failures_with_no_address_are_said_to_be_undecidable`, `a_clean_log_has_no_assessment` | labels a count shape only: fixed thresholds, no time window for a file source, no correlation across logs, and nothing acts on the verdict |
| 77 | Real-Time Intelligence | partial | daemon triggers and file watches are live feeds; no sensor fusion |
| 78 | Autonomous Defense | never | offensive or defensive action without a human is out of scope by policy |
| 79 | Environmental Analysis | never | no sensors |
| 80 | Mission Coordination | have | hive + delegation + plan tracking (rows 36-39, 4) |
| 81 | 3D Modeling | never | out of scope; no geometry kernel |
| 82 | Engineering Simulation | never | out of scope; needs a solver |
| 83 | Predictive Maintenance | partial | disk fill is now projected, not just measured: `mapek` keeps a 24-sample history (`mapek.disk_history`) and a pure `project_disk_fill` requires >=3 samples over >=1 h, positive growth and a current share under 95%, then reports the hours until 95% (Medium under 24 h); `heartbeat.rs` separately predicts its own job failures | one number on one filesystem, and only from readings this process took: no SMART, no inode or IO-error signal, no service or hardware failure prediction |
| 84 | Automated Workshop | never | no machines |
| 85 | Contextual Awareness | have | a new session opens knowing three things about its moment: the operator's memory files (`memory_snapshot`), the next scheduled job (`next scheduled:`), and where the last work item stopped (`where_we_left_off` in `learning.rs` -> `last time: `topic` (edit) ended with an error — that is where the work stopped`, wired in `RuntimeHandle::create_session`); ordering is explicit by `(timestamp_ms, id)`, a chat-only turn is skipped, absolute paths never appear in the line; tests `a_failed_last_session_is_named_as_unfinished`, `a_successful_verified_session_says_nothing`, `no_history_says_nothing`, `a_session_start_names_where_the_last_work_stopped` | the newest 10 experiences only, and it names the newest work item, not the session that is open |

## 6. Score

| group | have | partial | declared | missing | never |
|---|---|---|---|---|---|
| intelligence, memory, self (18 rows) | 10 | 4 | 2 | 2 | 0 |
| voice, perception (13) | 3 | 3 | 0 | 7 | 0 |
| web, docs, dev (17) | 9 | 5 | 0 | 3 | 0 |
| agents, autonomy, ops (22) | 9 | 5 | 2 | 6 | 0 |
| film list (15) | 2 | 4 | 0 | 0 | 9 |
| **total (85)** | **33** | **21** | **4** | **18** | **9** |

## 7. Build queue, in value order

| # | change | turns | into | state |
|---|---|---|---|---|
| 1 | `doc.read` tool over `pdftotext` / unzip+XML, following the `tesseract` shell-out at `computer.rs:515` | 28 | have | done — `tools/doc.rs` |
| 2 | `research` tool: one question → N bounded searches + fetches → source-tagged digest | 26 | have | done — `tools/research.rs` |
| 3 | terminal tool over `vikram-terminals` (ssh, docker) through the existing approval path | 51, 52 | have | done — `tools/terminal.rs` |
| 4 | persist `WorkerRegistry`; load on boot so partial workers are resumable | 44 | have | done — `worker_store.rs` + boot load |
| 5 | `system.status` tool: load, memory, real disk figure, self-check verdict | 49 | have | done — `tools/system.rs` |
| 6 | installed plugins become callable as `plugin.<id>` in the next turn's schema | 64 | have | done — manifest declares `tools`, `with_plugins` registers each as `plugin.<name>` gate Execute |
| 7 | reply stops on a new turn (`CancelToken` + `hush`); mic open during playback needs echo cancellation | 15 | partial | half done — `crates/vikram-cli/src/terminal.rs` |
| 8 | `tts_language` field → `espeak -v`, piper voice by language | 16 | have | done — `crates/vikram-voice` |

Not queued, with reasons: model routing and self-optimization (row 62, 67) need a real
criteria source before wiring them would beat the fixed behaviour; wake word (14) needs
an ONNX engine that is not in the tree; face, object, camera, video, audio-class and
spatial work (18-24) need a model and a sensor we do not have; IoT, robotics, smart home
(58-60) need devices; every `never` row needs hardware or crosses a safety line.

## 8. Rule for this file

A row moves to `have` only when a production caller exists and a test names the
behaviour. A row that regresses goes back down in the same change that breaks it.
