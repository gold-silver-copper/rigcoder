//! The cell harness: the real product session (`RigcoderPlugin::live`, the
//! Gemini adapter, the witness installed as the CLI installs it) pointed at
//! a `rig-cassette` server that records the real API once and replays it
//! key-free afterwards. Each cell checks an independent oracle first, then
//! what the observation trace says.
//!
//! Recording: `RIG_PROVIDER_TEST_MODE=record` with `GEMINI_API_KEY` set.
//! Replay (the default, and CI's contract): no key needed.
//!
//! The workspace path is part of the request (the preamble names it), so a
//! cell's workspace is a fixed absolute path under `/tmp`, not a tempdir,
//! and cells run one at a time (`ONE_AT_A_TIME`).
//!
//! Recording order matters: later matrices replay or derive from earlier
//! recordings, and `derive` runs before a cell takes the lock. Record with
//! one thread, matrix by matrix:
//!
//! ```sh
//! RIG_PROVIDER_TEST_MODE=record cargo test -p rigcoder --test gemini_observe -- \
//!   --test-threads=1 turns:: gates:: interruptions:: failures:: lineage:: host:: drivers::
//! ```

#![allow(dead_code)]

use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bevy_app::App;
use bevy_ecs::prelude::*;
use rig::observe::{Action, Clock, ObservationLog, ObservationTrace, Stage};
use rig_cassette::{CassetteMode, CassetteSpec, ProviderCassette};
use rigcoder::{
    Conversation, Event, ModelChoice, RigcoderPlugin, Transcript,
    approval::ApprovalMode,
    steer::{Scope, Steer},
};

pub const PROVIDER: &str = "gemini";
pub const UPSTREAM: &str = "https://generativelanguage.googleapis.com";
pub const KEY_ENV: &str = "GEMINI_API_KEY";
pub const MODEL: &str = "gemini-3.8-flash";

/// A short system prompt: the product's `--prompt-file` seam, used so every
/// recording is cheap and the model follows one instruction per cell.
pub const PROMPT: &str = "You are a test agent operating in a workspace. Follow the user's instruction literally. When asked to run a command, call the bash tool with exactly that command. When asked to read a file, call read_file. After tool results arrive, reply with one short sentence. Never ask questions.";

pub fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("the fixtures directory exists")
}

/// A cell served by a paced local server instead of a cassette: the
/// recorded frames of `source`, one per `pace`.
pub fn paced(source: (&str, &str), index: usize, pace: Duration) -> Source {
    let path = cassette_path(source.0, source.1);
    Source::Paced {
        frames: recorded_frames(&path, index),
        pace,
        workspace: (source.0.to_owned(), source.1.to_owned()),
    }
}

pub fn cassette_root() -> PathBuf {
    fixture_root().join("cassettes")
}

/// The cassette for `matrix/name`.
pub fn cassette_path(matrix: &str, name: &str) -> PathBuf {
    rig_cassette::cassette_path(&cassette_root(), PROVIDER, &format!("{matrix}/{name}"))
}

/// A fixed workspace: the path is embedded in every recorded request.
pub fn workspace(matrix: &str, name: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp/rigcoder-observe")
        .join(matrix)
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A counting clock: monotonic, deterministic, host-owned.
#[derive(Default)]
pub struct Ticks(AtomicU64);

impl Clock for Ticks {
    fn elapsed(&self) -> Duration {
        Duration::from_millis(self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// How a cell is configured before its run starts.
pub struct Config {
    pub stream: bool,
    pub retries: u8,
    pub max_turns: usize,
    pub max_tokens: u64,
    pub approval: ApprovalMode,
    pub prompt: &'static str,
    pub concurrency: Option<usize>,
    pub steer: Option<fn(&mut Steer)>,
    pub additional_params: Option<serde_json::Value>,
    /// `None`: no witness at all (the disabled path).
    pub witness: Option<WitnessConfig>,
    /// Where the model's answers come from.
    pub source: Source,
    /// The model name on the wire.
    pub model: &'static str,
    /// Send a deliberately invalid key (recorded as a real auth failure).
    pub bogus_key: bool,
    /// Wrap the model handler in serving layers before registering it.
    pub layers: Option<fn(rig::serve::ErasedHandler) -> rig::serve::ErasedHandler>,
    /// File-tool write scope (configuring it disables bash).
    pub scope: Option<fn(&Path) -> Scope>,
    /// Record streamed dispatches' events verbatim in the effect log.
    pub keep_stream_events: bool,
    /// The cell's evidence packet is not compared on replay (its story has
    /// a timing in it: a cancellation racing a handler); it is still
    /// written whenever packets are written.
    pub volatile: bool,
}

#[derive(Clone, Default)]
pub struct WitnessConfig {
    pub capacity: Option<usize>,
    pub clock: bool,
    pub session: Option<String>,
}

#[derive(Clone)]
pub enum Source {
    /// Record when the ambient mode says so, replay otherwise.
    Ambient,
    /// Always replay this cassette file, under the workspace of the cell
    /// that recorded it (`matrix`, `name`): the path is in the request.
    Replay {
        path: PathBuf,
        workspace: (String, String),
    },
    /// No provider wire at all: the cell never reaches a handler.
    None,
    /// A paced local server replaying recorded frames (see [`paced`]).
    Paced {
        frames: Vec<String>,
        pace: Duration,
        workspace: (String, String),
    },
}

impl Source {
    /// Replay another cell's recording.
    pub fn of(matrix: &str, name: &str) -> Self {
        Self::Replay {
            path: cassette_path(matrix, name),
            workspace: (matrix.to_owned(), name.to_owned()),
        }
    }

    /// Replay a derived fixture recorded under another cell's workspace.
    pub fn derived(path: PathBuf, matrix: &str, name: &str) -> Self {
        Self::Replay {
            path,
            workspace: (matrix.to_owned(), name.to_owned()),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            stream: true,
            retries: 0,
            max_turns: 4,
            max_tokens: 512,
            approval: ApprovalMode::Auto,
            prompt: PROMPT,
            concurrency: None,
            steer: None,
            additional_params: None,
            witness: Some(WitnessConfig::default()),
            source: Source::Ambient,
            model: MODEL,
            bogus_key: false,
            layers: None,
            scope: None,
            keep_stream_events: false,
            volatile: false,
        }
    }
}

/// A handler built before startup, registered under the model key by
/// [`register_prebuilt`] so `setup` finds it bound.
#[derive(Resource)]
pub struct Prebuilt(pub Mutex<Option<rig::serve::ErasedHandler>>);

pub fn register_prebuilt(mut handlers: rig_ecs::bus::Handlers, prebuilt: Res<Prebuilt>) {
    if let Some(handler) = lock(&prebuilt.0).take() {
        handlers
            .register_erased(rigcoder::model::MODEL_KEY, handler)
            .expect("the layered model registers");
    }
}

impl Config {
    pub fn unary() -> Self {
        Self {
            stream: false,
            ..Self::default()
        }
    }

    pub fn streamed() -> Self {
        Self::default()
    }

    pub fn delivery(stream: bool) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

pub struct Cell {
    pub matrix: String,
    pub name: String,
    pub dir: PathBuf,
    pub app: App,
    pub mode: CassetteMode,
    pub ticks: Arc<Ticks>,
    /// The cell's configuration, as the evidence packet records it.
    pub described: serde_json::Value,
    volatile: bool,
    rt: tokio::runtime::Runtime,
    cassette: Option<ProviderCassette>,
}

fn http() -> rig::http_client::BoxedHttpClient {
    rig::http_client::ReqwestClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap(),
    )
    .boxed()
}

impl Cell {
    /// Build the cell: start the cassette, build the product app, apply the
    /// configuration, run startup.
    pub fn new(matrix: &str, name: &str, config: Config) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mode, path, ws) = match &config.source {
            Source::Ambient => (
                Some(CassetteMode::current()),
                cassette_path(matrix, name),
                (matrix.to_owned(), name.to_owned()),
            ),
            Source::Replay { path, workspace } => {
                (Some(CassetteMode::Replay), path.clone(), workspace.clone())
            }
            Source::None => (None, PathBuf::new(), (matrix.to_owned(), name.to_owned())),
            Source::Paced { workspace, .. } => (None, PathBuf::new(), workspace.clone()),
        };
        let described = serde_json::json!({
            "matrix": matrix,
            "cell": name,
            "source": match &config.source {
                Source::Ambient => serde_json::json!({"kind": "cassette", "scenario": format!("{matrix}/{name}")}),
                Source::Replay { path, workspace } => serde_json::json!({"kind": "replay", "path": path.strip_prefix(fixture_root()).map(|p| p.display().to_string()).unwrap_or_else(|_| path.display().to_string()), "workspace_of": format!("{}/{}", workspace.0, workspace.1)}),
                Source::None => serde_json::json!({"kind": "none"}),
                Source::Paced { frames, pace, workspace } => serde_json::json!({"kind": "paced", "frames": frames.len(), "pace_ms": pace.as_millis(), "workspace_of": format!("{}/{}", workspace.0, workspace.1)}),
            },
            "model": config.model,
            "stream": config.stream,
            "max_tokens": config.max_tokens,
            "provider_retries": config.retries,
            "max_turns": config.max_turns,
            "approval": format!("{:?}", config.approval).to_ascii_lowercase(),
            "prompt": config.prompt,
            "concurrency": config.concurrency,
            "additional_params": config.additional_params,
            "bogus_key": config.bogus_key,
            "layers": config.layers.is_some(),
            "scope": config.scope.is_some(),
            "steer_overridden": config.steer.is_some(),
            "keep_stream_events": config.keep_stream_events,
            "witness": config.witness.as_ref().map(|w| serde_json::json!({"capacity": w.capacity, "clock": w.clock, "session": w.session})),
            "volatile": config.volatile,
            "rig": &RIG_REV[..12],
            "rigcoder": env!("CARGO_PKG_VERSION"),
        });
        let volatile = config.volatile;
        let cassette = mode.map(|mode| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let scenario: &'static str = Box::leak(format!("{matrix}/{name}").into_boxed_str());
            rt.block_on(ProviderCassette::start_at(
                PROVIDER,
                CassetteSpec::new(scenario),
                UPSTREAM,
                mode,
                path,
            ))
        });
        let dir = workspace(&ws.0, &ws.1);
        let (base, key) = match (&cassette, &config.source) {
            (Some(cassette), _) => (
                cassette.base_url(),
                if config.bogus_key {
                    cassette.bogus_api_key()
                } else {
                    cassette.api_key(KEY_ENV)
                },
            ),
            (None, Source::Paced { frames, pace, .. }) => {
                (paced_server(frames.clone(), *pace).0, "paced".to_owned())
            }
            (None, _) => ("http://127.0.0.1:9".to_owned(), "no-wire".to_owned()),
        };
        let mut app = App::new();
        app.add_plugins(RigcoderPlugin {
            workspace: dir.clone(),
            model: ModelChoice::parse(PROVIDER, Some(config.model.to_owned())).unwrap(),
            max_turns: config.max_turns,
            mode: rigcoder::Mode::Live,
            prompt_override: Some(config.prompt.to_owned()),
            keep_stream_events: config.keep_stream_events,
        });
        app.insert_resource(rigcoder::model::ModelConnection::new(
            base.clone(),
            key.clone(),
            http(),
        ));
        if let Some(layers) = config.layers {
            use rig::client::CompletionClient;
            let client = rig::providers::gemini::Client::builder()
                .api_key(key)
                .base_url(&base)
                .http_client(http())
                .build()
                .unwrap();
            let adapter = rig::serve::adapters::CompletionAdapter::new(
                config.model,
                client.completion_model(config.model),
            );
            let handler = layers(rig::serve::ErasedHandler::new(adapter));
            app.insert_resource(Prebuilt(Mutex::new(Some(handler))))
                .add_systems(bevy_app::PreStartup, register_prebuilt);
        }
        app.insert_resource(rigcoder::RunSettings {
            stream: config.stream,
            max_tokens: config.max_tokens,
            provider_retries: config.retries,
        });
        app.insert_resource(config.approval);
        if let Some(steer) = config.steer {
            let mut rules = app.world().resource::<Steer>().clone();
            steer(&mut rules);
            app.insert_resource(rules);
        }
        if let Some(scope) = config.scope {
            app.insert_resource(scope(&dir));
        }
        let ticks = Arc::new(Ticks::default());
        match &config.witness {
            None => {
                app.world_mut()
                    .remove_resource::<rig_ecs::bus::Witnessing>();
                app.world_mut()
                    .remove_resource::<rigcoder::observe::Observations>();
            }
            Some(witness) => {
                let mut log = match witness.capacity {
                    Some(capacity) => ObservationLog::with_capacity(capacity),
                    None => ObservationLog::default(),
                };
                if witness.clock {
                    log = log.with_clock(ticks.clone());
                }
                if let Some(session) = &witness.session {
                    log = log.with_session(session.clone());
                }
                let log = Arc::new(log);
                rig_ecs::bus::Witnessing::install(app.world_mut(), log.clone());
                app.insert_resource(rigcoder::observe::Observations(log));
            }
        }
        // Startup: the model and the tools register, the agent spawns.
        app.update();
        if let Some(concurrency) = config.concurrency {
            let agent = app.world().resource::<rigcoder::AgentHandle>().agent;
            app.world_mut()
                .entity_mut(agent)
                .insert(rig_ecs::agent::ToolPolicy { concurrency });
        }
        if let Some(params) = config.additional_params {
            let agent = app.world().resource::<rigcoder::AgentHandle>().agent;
            app.world_mut()
                .entity_mut(agent)
                .insert(rig_ecs::agent::AdditionalParams(Some(params)));
        }
        Self {
            matrix: matrix.to_owned(),
            name: name.to_owned(),
            dir,
            app,
            mode: mode.unwrap_or(CassetteMode::Replay),
            ticks,
            described,
            volatile,
            rt,
            cassette,
        }
    }

    /// Everything the world holds about the run, as files: the evidence
    /// packet. Written to `fixtures/evidence/gemini/<matrix>/<cell>/` when
    /// packets are being written (`RIGCODER_EVIDENCE=write`, or cassette
    /// record mode), otherwise regenerated in scratch and compared with the
    /// committed packet after stripping measurements (delivery pass
    /// numbers, clock stamps), unless the cell is `volatile`.
    pub fn evidence(&self) {
        let world = self.app.world();
        let mut files: Vec<(String, String)> = Vec::new();
        files.push(("cell.json".into(), pretty(&self.described)));
        let log = rigcoder::effect_log(world);
        files.push(("effects.json".into(), pretty(&log)));
        if let Some(trace) = rigcoder::observations(world) {
            files.push(("observations.json".into(), pretty(&trace)));
        }
        let transcript: String = world
            .resource::<Transcript>()
            .events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect();
        files.push(("transcript.jsonl".into(), transcript));
        let conversation = world.resource::<Conversation>();
        files.push(("history.json".into(), pretty(&conversation.history)));
        files.push((
            "run.json".into(),
            pretty(&serde_json::json!({
                "runs": conversation.runs,
                "ending": self.ending(),
                "answer": self.answer(),
                "provider_retries": conversation.provider_retries,
                "records": log.records.len(),
                "observations": rigcoder::observations(world).map(|t| t.observations.len()),
            })),
        ));
        let mut workspace: Vec<(String, String)> = Vec::new();
        snapshot(&self.dir, &self.dir, &mut workspace);
        workspace.sort();
        for (path, content) in workspace {
            files.push((format!("workspace/{path}"), content));
        }
        let packet = fixture_root()
            .join("evidence")
            .join(PROVIDER)
            .join(&self.matrix)
            .join(&self.name);
        if writing_evidence() {
            let _ = std::fs::remove_dir_all(&packet);
            for (name, content) in &files {
                let path = packet.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, content).unwrap();
            }
            return;
        }
        assert!(
            packet.is_dir(),
            "{}: no evidence packet; write them with RIGCODER_EVIDENCE=write",
            packet.display()
        );
        for (name, content) in &files {
            let committed = std::fs::read_to_string(packet.join(name)).unwrap_or_else(|e| {
                panic!(
                    "{}/{name}: {e}; regenerate with RIGCODER_EVIDENCE=write",
                    packet.display()
                )
            });
            if self.volatile {
                continue;
            }
            let (a, b) = (normalized(name, &committed), normalized(name, content));
            assert!(
                a == b,
                "{}/{name} drifted from the committed evidence; regenerate with RIGCODER_EVIDENCE=write if the change is intended\n--- committed\n{}\n--- now\n{}",
                packet.display(),
                &a[..a.len().min(2000)],
                &b[..b.len().min(2000)]
            );
        }
        let mut listed = Vec::new();
        list(&packet, &packet, &mut listed);
        let mut produced: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        listed.sort();
        produced.sort();
        assert_eq!(
            listed, produced,
            "the packet holds exactly what the run produced"
        );
    }

    pub fn recording(&self) -> bool {
        self.mode == CassetteMode::Record
    }

    pub fn submit(&mut self, prompt: &str) -> Entity {
        rigcoder::submit(self.app.world_mut(), prompt).expect("a run starts")
    }

    pub fn cancel(&mut self, reason: &str) {
        rigcoder::cancel(self.app.world_mut(), reason);
    }

    /// Drive to quiescence. Provider backoff is a live-provider courtesy; a
    /// cell asserts what is retried, not how long the wait is.
    pub fn drive(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            self.app.update();
            self.app
                .world_mut()
                .resource_mut::<Conversation>()
                .expire_backoff();
            if !self.app.world().resource::<Conversation>().is_busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!(
            "{}/{}: the run did not end: {:?}",
            self.matrix,
            self.name,
            self.events()
        );
    }

    /// Keep the world turning for `duration` after the run ended, so what
    /// the bus does with an effect the run left in flight is observed.
    pub fn drive_more(&mut self, duration: Duration, mut done: impl FnMut(&World) -> bool) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.app.update();
            if done(self.app.world()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn drive_until(&mut self, what: &str, mut done: impl FnMut(&World) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            self.app.update();
            if done(self.app.world()) {
                return;
            }
            if !self.app.world().resource::<Conversation>().is_busy() {
                panic!("{what}: the run ended first; {:?}", self.events());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("{what}: not reached; {:?}", self.events());
    }

    pub fn events(&self) -> Vec<Event> {
        self.app.world().resource::<Transcript>().events.clone()
    }

    pub fn ending(&self) -> String {
        self.events()
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Settled { .. } => Some("settled".to_owned()),
                Event::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "none".into())
    }

    pub fn answer(&self) -> String {
        self.events()
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Settled { answer } => Some(answer.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn retries(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::Retrying { .. }))
            .count()
    }

    pub fn tool_results(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::ToolResult { ok: true, .. }))
            .count()
    }

    pub fn log(&self) -> rigcoder::EffectLog {
        rigcoder::effect_log(self.app.world())
    }

    /// The trace, after a serde round trip.
    pub fn trace(&self) -> ObservationTrace {
        let trace = rigcoder::observations(self.app.world()).expect("a witness is installed");
        let json = serde_json::to_string(&trace).unwrap();
        let back: ObservationTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, trace, "the trace round-trips");
        trace
    }

    pub fn facts(&self) -> Vec<String> {
        facts(&self.trace())
    }

    pub fn count(&self, fact: &str) -> usize {
        self.facts().iter().filter(|f| f.as_str() == fact).count()
    }

    pub fn find(&self, pred: impl Fn(&Action) -> bool) -> Option<rig::observe::Observation> {
        self.trace()
            .observations
            .into_iter()
            .find(|o| pred(&o.action))
    }

    /// How many requests the cassette holds (in replay, exactly the requests
    /// the session made: `finish` asserts full consumption).
    pub fn recorded_requests(&self) -> usize {
        match &self.cassette {
            Some(cassette) if !self.recording() => rig_cassette::recorded_request_paths(
                &cassette_root(),
                PROVIDER,
                &format!("{}/{}", self.matrix, self.name),
            )
            .len(),
            _ => usize::MAX,
        }
    }

    /// Finish the cassette: write the recording, or assert replay consumed
    /// every interaction.
    pub fn finish(&mut self) {
        if let Some(cassette) = self.cassette.take() {
            self.rt.block_on(cassette.finish());
        }
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        // A cell that panicked before `finish` still writes its recording
        // so the failure can be inspected; replay consumption is not
        // asserted on that path (the panic is the report).
        if let Some(cassette) = self.cassette.take()
            && self.recording()
        {
            let _ = catch_unwind(AssertUnwindSafe(|| self.rt.block_on(cassette.finish())));
        }
    }
}

/// Cells run one at a time: a cell's workspace path is part of its recorded
/// request, so cells that replay one recording share one directory.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Run one cell: build, execute `body`, finish the cassette; a body panic
/// is re-raised after the recording is written.
pub fn run(matrix: &str, name: &str, config: Config, body: impl FnOnce(&mut Cell)) {
    let _serial = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cell = Cell::new(matrix, name, config);
    let result = catch_unwind(AssertUnwindSafe(|| body(&mut cell)));
    match result {
        Ok(()) => {
            cell.evidence();
            cell.finish()
        }
        Err(payload) => {
            drop(cell);
            resume_unwind(payload);
        }
    }
}

/// The facts, named: library actions by their variant, host facts by kind.
pub fn facts(trace: &ObservationTrace) -> Vec<String> {
    trace
        .observations
        .iter()
        .map(|o| fact(&o.action, o.stage))
        .collect()
}

pub fn fact(action: &Action, stage: Stage) -> String {
    match action {
        Action::Host { kind, payload } => match payload.get("decision") {
            Some(decision) => format!("{kind}:{}", decision.as_str().unwrap_or("")),
            None => kind.clone(),
        },
        Action::Ended { ending } => format!("ended:{}", ending.code),
        Action::Denied { reason } => format!("denied@{stage:?}:{}", reason.code),
        Action::Refused { reason } => format!("refused:{}", reason.code),
        Action::Deferred { reason } => format!("deferred:{}", reason.code),
        Action::Cancelled { .. } => format!("cancelled@{stage:?}"),
        Action::Held { .. } => "held".into(),
        Action::Released => "released".into(),
        Action::Issued => "issued".into(),
        Action::Landed { .. } => "landed".into(),
        Action::StreamTruncated { .. } => "stream_truncated".into(),
        Action::Replaced { .. } => "replaced".into(),
        Action::CancelRequested { .. } => "cancel_requested".into(),
        Action::Retry { .. } => "retry".into(),
        Action::InvalidCall { .. } => "invalid_call".into(),
        Action::Approved { .. } => "approved".into(),
        Action::Patched { .. } => "patched".into(),
    }
}

/// The semantic facts of a trace for a unary/streamed parity claim: every
/// fact but a delivery-only truncation. Holds and releases are decisions
/// too: since Rig #2479 the runtime lifts only the holds it placed, so a
/// hold stands exactly as long as the policy holds it.
pub fn semantic_facts(trace: &ObservationTrace) -> Vec<String> {
    facts(trace)
        .into_iter()
        .filter(|f| f != "stream_truncated")
        .collect()
}

/// Read a cassette as YAML documents (one per interaction).
pub fn read_cassette(path: &Path) -> Vec<serde_yaml::Value> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_yaml::Deserializer::from_str(&text)
        .map(|doc| serde_yaml::Value::deserialize(doc).unwrap())
        .collect()
}

/// YAML documents as cassette text.
pub fn cassette_text(docs: &[serde_yaml::Value]) -> String {
    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        out.push_str(&serde_yaml::to_string(doc).unwrap());
    }
    out
}

/// The SSE frames of a recorded streaming response body, and a way to put
/// them back.
pub fn sse_frames(body: &str) -> Vec<String> {
    body.split("\r\n\r\n")
        .flat_map(|chunk| chunk.split("\n\n"))
        .filter(|chunk| !chunk.trim().is_empty())
        .map(|chunk| chunk.trim_matches(['\r', '\n']).to_owned())
        .collect()
}

pub fn join_frames(frames: &[String]) -> String {
    frames
        .iter()
        .map(|f| format!("{f}\r\n\r\n"))
        .collect::<String>()
}

/// A derived fixture: `source` (a recorded cassette) rewritten by `edit`
/// into `derived`. The edit is the documentation of what the live API
/// cannot produce. Always computed; in record mode the result is written,
/// otherwise it must equal the committed file byte for byte, so a derived
/// fixture can never drift from its source or its edit unnoticed.
pub fn derive(
    source: &Path,
    derived: &Path,
    edit: impl FnOnce(&mut Vec<serde_yaml::Value>),
) -> PathBuf {
    assert!(
        source.is_file(),
        "derived fixtures need their recorded source: {}",
        source.display()
    );
    let mut docs = read_cassette(source);
    edit(&mut docs);
    let text = cassette_text(&docs);
    if CassetteMode::current() == CassetteMode::Record {
        if let Some(parent) = derived.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(derived, &text).unwrap();
    } else {
        let committed = std::fs::read_to_string(derived).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; derived fixtures are written in record mode",
                derived.display()
            )
        });
        assert!(
            committed == text,
            "{} is out of date with its source {} and its edit; regenerate in record mode",
            derived.display(),
            source.display()
        );
    }
    derived.to_owned()
}

pub fn body_of(doc: &mut serde_yaml::Value) -> &mut String {
    match doc.get_mut("then").and_then(|t| t.get_mut("body")) {
        Some(serde_yaml::Value::String(s)) => s,
        other => panic!("a string response body: {other:?}"),
    }
}

pub fn set_status(doc: &mut serde_yaml::Value, status: u16) {
    doc["then"]["status"] = serde_yaml::Value::Number(status.into());
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub use serde::Deserialize;

/// A one-thread HTTP/1.1 server that answers every request with the frames
/// of a recorded streaming response, one frame per `pace`, then closes: the
/// recorded bytes, delivered slowly enough for a host to act mid-stream.
pub fn paced_server(frames: Vec<String>, pace: Duration) -> (String, Arc<AtomicU64>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicU64::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let counter = counter.clone();
            let frames = frames.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut head_end = None;
                loop {
                    let n = match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if head_end.is_none()
                        && let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        head_end = Some(at + 4);
                    }
                    if let Some(at) = head_end {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= at + length {
                            break;
                        }
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
                for frame in frames {
                    std::thread::sleep(pace);
                    if stream
                        .write_all(format!("{frame}\r\n\r\n").as_bytes())
                        .and_then(|()| stream.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    (base, requests)
}

/// The recorded SSE frames of the `index`-th interaction of a cassette.
pub fn recorded_frames(path: &Path, index: usize) -> Vec<String> {
    let mut docs = read_cassette(path);
    sse_frames(body_of(&mut docs[index]))
}

/// A unary/streamed parity pair: `body` runs once per wire under
/// `config(stream)`, and the cells' semantic facts must agree.
pub fn pair(matrix: &str, name: &str, config: fn(bool) -> Config, body: impl Fn(&mut Cell, bool)) {
    let mut traces = Vec::new();
    for stream in [false, true] {
        let cell = format!("{name}_{}", if stream { "stream" } else { "unary" });
        run(matrix, &cell, config(stream), |cell| {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(cell, stream)));
            if cell
                .app
                .world()
                .get_resource::<rig_ecs::bus::Witnessing>()
                .is_some()
            {
                eprintln!("[{}/{}] facts: {:?}", matrix, cell.name, cell.facts());
            }
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
            traces.push(semantic_facts(&cell.trace()));
        });
    }
    assert_eq!(
        traces[0], traces[1],
        "unary and streamed agree on the facts"
    );
}

/// The Rig revision every cell runs against (the workspace pin).
pub const RIG_REV: &str = "a219d2b0c73d87bd8d1fe080c0cbdf95e0b35fe4";

fn pretty<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap() + "\n"
}

/// Whether evidence packets are being written rather than compared.
pub fn writing_evidence() -> bool {
    std::env::var("RIGCODER_EVIDENCE").is_ok_and(|v| v == "write")
        || CassetteMode::current() == CassetteMode::Record
}

/// The workspace's files, relative path and content (binary as base64).
fn snapshot(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            snapshot(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let content = match std::fs::read(&path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(e) => format!("base64:{}", base64_encode(e.as_bytes())),
                },
                Err(e) => format!("unreadable: {e}"),
            };
            out.push((rel.display().to_string(), content));
        }
    }
}

fn list(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            list(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A packet file with its measurements stripped: delivery pass numbers in
/// the effect log, clock stamps in the trace.
fn normalized(name: &str, content: &str) -> String {
    match name {
        "effects.json" => {
            let mut v: serde_json::Value = serde_json::from_str(content).unwrap();
            collapse_deliveries(&mut v);
            pretty(&v)
        }
        "observations.json" => {
            let mut v: serde_json::Value = serde_json::from_str(content).unwrap();
            if let Some(observations) = v
                .pointer_mut("/observations")
                .and_then(|d| d.as_array_mut())
            {
                for o in observations {
                    o.as_object_mut().map(|o| o.remove("at"));
                }
            }
            pretty(&v)
        }
        // A failure's Debug form carries the response headers the report
        // kept, and the replay server stamps `date` at replay time.
        "transcript.jsonl" | "run.json" => DATE_HEADER
            .replace_all(content, "date: <replayed>")
            .into_owned(),
        _ => content.to_owned(),
    }
}

static DATE_HEADER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r#"date\\?": \\?"[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT\\?""#,
    )
    .unwrap()
});

/// An effect log's delivery partitions with their measurements removed:
/// the schedule pass (`batch`) a delivery landed in, and how a stream was
/// split across passes (consecutive stream deliveries of one effect become
/// one, their items summed) — both depend on when bytes arrived, not on
/// what the program decided.
pub fn collapse_deliveries(log: &mut serde_json::Value) {
    let Some(deliveries) = log
        .pointer_mut("/header/deliveries")
        .and_then(|d| d.as_array_mut())
    else {
        return;
    };
    let mut collapsed: Vec<serde_json::Value> = Vec::new();
    for mut d in deliveries.drain(..) {
        if let Some(o) = d.as_object_mut() {
            o.remove("batch");
        }
        let is_stream = d["kind"]["delivery"] == "stream";
        if is_stream
            && let Some(last) = collapsed.last_mut()
            && last["kind"]["delivery"] == "stream"
            && last["id"] == d["id"]
        {
            let items = last["kind"]["items"].as_u64().unwrap_or(0)
                + d["kind"]["items"].as_u64().unwrap_or(0);
            last["kind"]["items"] = serde_json::Value::from(items);
            continue;
        }
        collapsed.push(d);
    }
    *deliveries = collapsed;
}
