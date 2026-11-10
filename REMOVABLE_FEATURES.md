# Removable Features Audit

An audit of features in this fork that could be removed while the core editor keeps working,
ranked by estimated resource savings (largest first).

## Methodology

No binary was built for this audit, so the ranking is an estimate based on:

- Transitive dependency counts from `cargo tree -p <crate> -e normal` (the `zed` binary pulls in ~1,155 unique crates).
- Native libraries each feature drags in.
- Runtime behaviour: extra processes, network polling, file watching.

For real numbers, build with `cargo bloat` before and after each removal.

## Ranked list

| # | Feature group | Crates | Gain | Difficulty / breakage |
|---|---|---|---|---|
| 1 | **AI agent & LLM providers** | `agent`, `agent_ui`, `agent_servers`, `agent_settings`, `agent_skills`, `acp_thread`, `acp_tools`, `action_log`, `ai_onboarding`, `prompt_store`, `language_model*`, `language_models*`, `anthropic`, `open_ai`, `openai_subscribed`, `google_ai`, `bedrock` (AWS SDK), `aws_http_client`, `deepseek`, `mistral`, `ollama`, `lmstudio`, `llama_cpp`, `open_router`, `x_ai*`, `opencode`, `cloud_llm_client`, `web_search*`, `streaming_diff` | Largest code mass (~400k LOC; `agent_ui` alone has 796 transitive deps). Big binary and RAM reduction, an on-disk database, agent subprocesses | **Hard.** `git_ui` (commit message generation), `title_bar` and `workspace` depend on `agent_settings` / `language_model` and need those features cut out |
| 2 | **Collaboration / calls / screen share** | `call`, `collab_ui`, `channel`, `notifications`, `livekit_client`, `livekit_api`, `audio` (cpal/rodio), `scap`, `libwebrtc` | **libwebrtc is a huge prebuilt native library**, likely the single largest binary-size win. Also drops audio device handling and the persistent zed.dev connection while signed in | Medium. `git_ui` and `title_bar` import `call` / `livekit_client`. `collab` (the server) is not in the binary at all |
| 3 | **Edit predictions & Copilot** | `edit_prediction*`, `zeta_prompt`, `copilot`, `copilot_ui`, `copilot_chat`, `codestral` | **Largest runtime CPU/network saving**: a request per keystroke, context gathering, and Copilot runs a **separate Node.js LSP process** | Medium. `editor` only uses `edit_prediction_types`, so keep that small crate |
| 4 | **Telemetry, crash reporting, auto-update** | `telemetry*`, `crashes` + minidumper, `hang_telemetry`, `input_latency_ui`, `crates/zed/src/reliability`, `auto_update`, `auto_update_ui`, `auto_update_helper`, `feedback`, `system_specs` | `crashes` **starts a second copy of the zed process** as a crash handler, so removing it saves a whole process's RAM. Auto-update polls every hour (15 min on nightly), and on release builds of a personal fork it could replace your build with an upstream one | Easy to medium. `telemetry` calls are spread across `editor`, `workspace`, etc., but turning them into no-ops is mechanical |
| 5 | **Extension system** | `extension_host` (wasmtime + cranelift JIT), `extensions_ui`, `extension_suggest`, `theme_extension`, `language_extension`, `debug_adapter_extension` | Large binary size, plus a WASM runtime and memory for each loaded extension | **High cost:** every language that isn't built in (~20 are) and every extension theme comes from extensions. `recent_projects` also depends on it. Only remove it if the built-in languages are enough |
| 6 | **Debugger** | `dap`, `dap_adapters`, `debugger_ui`, `debugger_tools` | ~35k LOC (642 deps), plus downloaded adapters | Medium. `project` and `editor` depend on `dap` (breakpoints) |
| 7 | **REPL / Jupyter** | `repl` (runtimelib, ZeroMQ, Jupyter websockets) | 653 deps, kernel processes | Easy. Self-contained |
| 8 | **Dev containers / sandbox** | `dev_container`, `sandbox` | ~26k LOC | Easy to medium (`recent_projects` uses it) |
| 9 | **Remote development** | `remote`, `remote_connection`, `remote_server`, `proxy_handshake`, `http_proxy` | Moderate | **Hard.** Built deep into `project` and `workspace`. Not recommended |
| 10 | **Unused built-in grammars / language servers** | Entries in `crates/grammars` and `crates/languages` (cpp, go/gomod/gowork, python, ruby, elixir/heex, tailwind, eslint, …) | 0.5–3 MB of binary per grammar, plus language server downloads | Easy, one language at a time |
| 11 | **Small UI extras** | `journal`, `onboarding`, `language_onboarding`, `which_key`, `miniprofiler_ui`, `inspector_ui`, `component_preview`, `svg_preview`, `tabular_data_preview`, `markdown_preview` + `mermaid_render`, `settings_profile_selector`, `encoding_selector`, `line_ending_selector`, `call_hierarchy`, `snippets_ui`, `vim` (if vim mode is unused) | Small each; mostly smaller binary and faster startup | Easy. Mostly just removing an `init` call in `crates/zed/src/main.rs` (around lines 723–791) |
| 12 | **Workspace-only tooling** (not part of the zed binary) | `collab` (server, 47k LOC), `edit_prediction_cli`, `edit_prediction_metrics`, `eval_cli`, `eval_utils`, `extension_cli`, `theme_importer`, `schema_generator`, `docs_preprocessor`, `benchmarks`, `*_benchmarks`, `bench_metrics`, `agent_ui_benchmarks`, `gpui_ios`, `gpui_web`, plus Windows/macOS-only crates if building only on Linux (`gpui_windows`, `etw_tracing`, `windows_resources`, `explorer_command_injector`) | **No runtime gain.** Only faster `cargo` commands and less disk and repo clutter | Easy. Remove them from `members` in the root `Cargo.toml` |

## Core (keep)

`editor`, `project`, `workspace`, `gpui*`, `language`, `languages`, `lsp`, `worktree`, `fs`, `git`,
`git_ui` / `git_ui_core` (VS Code layout), `terminal` / `terminal_view`, `search`, `file_finder`,
`project_panel`, `outline*`, `settings*`, `theme*`, `ui`, `client` (imported by many crates).

## Suggested removal order

For the most savings with the least breakage:

1. Telemetry, crash handler and auto-update (#4)
2. Edit predictions and Copilot (#3)
3. Collaboration and calls (#2)
4. REPL (#7)
5. The AI agent (#1), which saves the most but means untangling `git_ui` and `title_bar`
