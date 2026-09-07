# rigcoder

A coding agent built on `rig-ecs`, the Bevy-world runtime from
[rig PR #2443](https://github.com/0xPlaygrounds/rig/pull/2443). The model is a
handler entity, every tool is a handler entity, the agent is an entity that
`Grant`s them, and each prompt is a run entity whose turns, tool calls and
outcomes are components the UI reads. Nothing in this repository awaits or
polls the model: the bus drives it as a Bevy schedule.

Workspace crates:

| crate | what |
|---|---|
| `crates/rigcoder` | the agent: `RigcoderPlugin`, the system prompt (`prompt.md`), six tools (`read_file`, `write_file`, `edit_file`, `list_files`, `grep`, `bash`), and the transcript systems |
| `crates/rigcoder-cli` | `rigcoder`: headless, one task in, transcript out; what the benchmark harness runs inside task containers |
| `crates/rigcoder-ui` | `rigcoder-ui`: a terminal UI, Bevy driving ratatui over crossterm via `bevy_ratatui` (Bevy 0.19 needs its `main` branch, pinned by commit) |
| `crates/rigcoder-bench` | `rigcoder-bench`: the Terminal-Bench runner (Docker directly, no framework) and the self-improvement loop |
| `crates/rigcoder-verify` | the migrated 42-case consumer verifier; integration with the product tools and stronger session contracts is in progress |

Plus `harness/`: the task slices, the Linux build script and the ledger of the
self-improvement loop (see `harness/README.md`).

## Run

Needs Rust 1.95 (pinned in `rust-toolchain.toml`) and an API key:

```sh
export ANTHROPIC_API_KEY=...           # or OPENAI_API_KEY with RIGCODER_PROVIDER=openai
cargo run -p rigcoder-ui -- /path/to/repo
cargo run -p rigcoder-cli -- --cwd /path/to/repo "Fix the failing test in src/lib.rs"
```

In the TUI: Enter sends, Alt+Enter or Ctrl+J inserts a newline, Esc stops the
run in flight, arrows and PgUp/PgDn scroll the transcript, End follows the
stream again, Ctrl+C quits. Set `RIGCODER_LOG=path` to get tracing output in a
file (the terminal is the screen).

Model selection: `RIGCODER_PROVIDER` (`anthropic` default, or `openai`) and
`RIGCODER_MODEL` (defaults `claude-opus-5` / `gpt-5.6-sol`); the CLI also takes
`--provider` and `--model`. Other CLI flags: `--max-turns`, `--timeout-secs`,
`--transcript out.jsonl`, `--verbose`.

## How a run works

1. `setup` (Startup) registers the provider model under `rigcoder/model` and
   each tool under `tool:<name>` with `Handlers::register`, spawns the agent
   entity (`Preamble`, `MaxTokens`, `MaxTurns`, `ToolPolicy`, `UsesModel`) and
   one `Grant` link entity per tool.
2. `submit` spawns a run over that agent with the conversation so far
   as history (`rig_ecs::systems::spawn_run`).
   `RunSettings` controls streaming, output tokens and provider retries; its
   defaults preserve streaming and each run freezes its own settings.
3. The bus folds the graph into a request, dispatches the completion, and
   materialises the model's tool calls as child effects; the handlers run on
   Bevy's IO task pool.
4. Three small systems read it back: streamed text after `RigSet::Fold`,
   tool calls in `BusSet::Gate`, tool outcomes and run endings from observers.
   They append to the `Transcript` resource; the UI and CLI render that.
5. When the run settles, its utterances are read off the graph in `Order` and
   become the next run's history.

Because tool calls are effect entities, everything rig-ecs offers applies
unchanged: a system in `BusSet::Gate` can hold or deny a `bash` call for
approval, a `Judge` system can rewrite a result, a `Scene` can checkpoint a
run mid-task, and an `EffectLog` can replay one.

File writes and edits prepare their contents without changing the workspace,
then recheck the original file before an atomic replacement. On macOS and Linux,
new files use private Unix permission bits; replacements preserve ordinary
permission bits and ownership. Read-only, hard-linked, set-ID, ACL-bearing and
extended-attribute-bearing targets are refused, as are replacements that would
change ownership or inherit unsupported metadata. This does not yet provide
file approval, a mutation ledger, or exclusion of concurrent external writers.

## Dependency pin

`Cargo.toml` pins all five direct Rig dependencies (`rig`, `rig-core`,
`rig-ecs`, `rig-effect-log` and `rig-cassette`) to extraction commit
`36bb89956a790367be8eaa88958d055bcce67718` in
[Rig PR #2474](https://github.com/0xPlaygrounds/rig/pull/2474), stacked on #2443.
To move the pin, update every Rig revision and `Cargo.lock`, then run the
workspace tests and `cargo run --locked -p rigcoder-verify -- verify`.
The Rig PR remains open while this migration is integrated and verified.
