//! **M5 acceptance scenarios** (F5.8, docs/08 §1 M5 row, docs/07 §2/§3).
//!
//! One named scenario per M5 exit-evidence bullet, each driving the **real**
//! machinery — the real orchestrator state machine, the real `policy::evaluate`,
//! the real `JarvisApprovalGate`, the real `PgGrantStore` and the real
//! hash-chained audit log over live Postgres — with doubles only at the
//! outermost hop, where the thing on the other side is a device or a
//! third-party network this host does not have: the Wyoming speech engines, the
//! Home Assistant REST API, the Spotify Web API and the MPRIS session bus.
//! `cargo xtask golden` (scenario 9) runs each of them by name and fails if one
//! stops existing.
//!
//! Fixture-driven throughout, per CLAUDE.md ("fixture-driven tests over
//! live-provider calls, always"): **no live Wyoming, no live Home Assistant, no
//! live Spotify**. Every adapter here is entered through its production client
//! and its production tool executor; only the transport trait underneath is
//! scripted, so the framing, allowlist, policy tier, grant binding, error
//! classification and result text under test are production code.
//!
//! What this file deliberately does **not** claim: the NFR-04 *number* for
//! evidence #1. A latency budget measured against fixture speech engines is not
//! the budget — see `evidence1_…`'s own note, `cargo xtask perf --voice` (which
//! labels itself "MODEL TIME EXCLUDED"), and
//! `docs/milestones/M5-acceptance.md` §3.

mod voice_fixture;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jarvis_adapters::home_assistant::{
    EntityAllowlist, HomeAssistantClient, HomeAssistantError, HomeAssistantTransport, HomeRequest,
};
use jarvis_adapters::spotify::{
    AccessToken, ApiRequest, ApiResponse, HttpMethod, SpotifyClient, SpotifyConfig, SpotifyError,
    SpotifyTransport, TokenResponse,
};
use jarvis_application::deterministic::DeterministicFirstProvider;
use jarvis_application::model::{FinishReason, ModelEvent, ModelProvider};
use jarvis_application::orchestrator::{Orchestrator, RunInput, ToolStack};
use jarvis_application::policy::{ApprovalGate, AuditSink, PolicyContext, ToolRegistry};
use jarvis_application::ports::{MediaController, MediaError};
use jarvis_application::testing::{
    EchoAssembler, FakeModel, ManualClock, RecordingCheckpointer, RecordingSink,
};
use jarvis_contracts::approvals::{ApprovalDecision, ApprovalDecisionDto};
use jarvis_domain::ids::{ApprovalId, RunId};
use jarvis_domain::media::{
    MediaSnapshot, PlaybackStatus, PlayerId, PlayerState, TrackMetadata, TransportCommand,
    VolumePct,
};
use jarvis_domain::policy::Scope;
use jarvis_domain::run::{Run, RunBudget, RunState};
use jarvis_domain::tools::{CanonicalValue as V, ToolId, ToolProposal};
use jarvis_infra::audit_sink::PgAuditSink;
use jarvis_infra::grants::PgGrantStore;
use jarvisd::approvals::JarvisApprovalGate;
use jarvisd::tools::{register_home_assistant_tools, register_media_tools, register_spotify_tools};
use sqlx::{PgPool, Row};
use tokio_util::sync::CancellationToken;

use voice_fixture::{
    Harness, Received, SESSION as VOICE_SESSION, VoiceSocket, VoiceWiring, addr_of,
    audio_frame_count, events_of, payload_of,
};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB1";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const USER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB2";
const DEVICE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB3";

/// Wall-clock ceiling for the fixture-driven voice exchange. Generous relative
/// to what the fixtures take: the point of the bound is that a regression fails
/// instead of hanging the suite.
const BUDGET: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// shared harness
// ---------------------------------------------------------------------------

fn ctx(scopes: &[&str]) -> PolicyContext {
    PolicyContext {
        user_id: USER.parse().expect("valid test ulid"),
        device_id: DEVICE.parse().expect("valid test ulid"),
        granted_scopes: scopes
            .iter()
            .map(|s| Scope::new(*s).expect("static scope is valid"))
            .collect::<BTreeSet<_>>(),
    }
}

fn new_run() -> Run {
    Run::new(
        RUN.parse().expect("valid test ulid"),
        SESSION.parse().expect("valid test ulid"),
        RunBudget::default_interactive(),
    )
}

/// The orchestrator, wired exactly as `jarvisd::runs::RunEngine` wires it for a
/// run with tool authority: the caller's model + registry, and the **real**
/// audit sink, approval gate and grant store over live Postgres.
#[allow(clippy::too_many_arguments)]
async fn drive(
    model: &dyn ModelProvider,
    registry: &ToolRegistry,
    audit: &dyn AuditSink,
    gate: &dyn ApprovalGate,
    grants: &PgGrantStore,
    scopes: &[&str],
    input: &str,
    sink: &RecordingSink,
) -> Run {
    let assembler = EchoAssembler;
    let checkpointer = RecordingCheckpointer::default();
    let clock = ManualClock::at_unix(1_700_000_000);
    let orchestrator = Orchestrator {
        model,
        context: &assembler,
        checkpointer: &checkpointer,
        sink,
        clock: &clock,
        user_id: None,
        tools: Some(ToolStack {
            registry,
            audit,
            context: ctx(scopes),
            approval_gate: gate,
            grant_minter: grants,
            grant_validator: grants,
        }),
    };
    orchestrator
        .drive(
            new_run(),
            RunInput {
                text: input.to_owned(),
            },
            CancellationToken::new(),
        )
        .await
}

/// Runtime-checked (not `query!`) on purpose: an acceptance scenario should not
/// be able to go stale against the offline sqlx cache, and these reads own no
/// production SQL.
async fn audit_types(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT event_type FROM audit.audit_events ORDER BY seq ASC")
        .fetch_all(pool)
        .await
        .expect("audit rows")
}

/// Every minted grant with its single-use consumption marker.
async fn grant_rows(pool: &PgPool) -> Vec<(String, bool)> {
    sqlx::query("SELECT tool_id, consumed_at FROM tooling.grants ORDER BY minted_at")
        .fetch_all(pool)
        .await
        .expect("grant rows")
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("tool_id"),
                row.get::<Option<time::OffsetDateTime>, _>("consumed_at")
                    .is_some(),
            )
        })
        .collect()
}

/// A model that proposes one tool call, then answers on the next turn. This is
/// the shape a reasoning provider really produces, and it is what lets the run
/// reach `Completed` instead of proposing forever.
fn propose_then_answer(proposal: ToolProposal, answer: &str) -> FakeModel {
    FakeModel::scripted_turns([
        vec![ModelEvent::ToolProposal(proposal)],
        vec![
            ModelEvent::TextDelta(answer.to_owned()),
            ModelEvent::Done(FinishReason::Stop),
        ],
    ])
}

fn proposal(tool: &str, args: impl IntoIterator<Item = (&'static str, V)>) -> ToolProposal {
    ToolProposal {
        tool_id: tool.parse::<ToolId>().expect("static tool id is valid"),
        arguments: V::obj(args),
    }
}

/// The client learns the minted approval id from the persisted (and, in
/// production, WS-published) `approval.requested` card — read it back from the
/// outbox rather than reaching into the gate, exactly as `approvals.rs` does.
async fn approve_when_requested(pool: PgPool, gate: Arc<JarvisApprovalGate>) {
    let run_id: RunId = RUN.parse().expect("valid test ulid");
    for _ in 0..500 {
        let row = sqlx::query(
            "SELECT payload FROM outbox.outbox_events \
             WHERE event_type = 'approval.requested' ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .expect("read outbox");
        if let Some(row) = row {
            let payload: serde_json::Value = row.get("payload");
            let id: ApprovalId = payload["card"]["approvalId"]
                .as_str()
                .expect("card carries approvalId")
                .parse()
                .expect("valid ULID");
            gate.resolve(
                &run_id,
                &id,
                ApprovalDecisionDto {
                    decision: ApprovalDecision::Approve,
                    edited_arguments: None,
                },
                "user:acceptance",
            )
            .await
            .expect("resolve the parked approval");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("approval.requested was never persisted");
}

// ---------------------------------------------------------------------------
// fixture: Home Assistant
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeEntity {
    state: String,
    name: String,
    area: Option<String>,
}

/// A scripted HA transport. Its very existence is the assertion that no
/// scenario reaches the network; `service_failures` is what lets a scenario
/// seed a light that does not respond.
#[derive(Default)]
struct FakeHome {
    calls: Mutex<Vec<HomeRequest>>,
    entities: Mutex<std::collections::BTreeMap<String, FakeEntity>>,
    service_failures: Mutex<BTreeSet<String>>,
}

impl FakeHome {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn add(
        self: &Arc<Self>,
        entity: &str,
        state: &str,
        name: &str,
        area: Option<&str>,
    ) -> Arc<Self> {
        self.entities.lock().unwrap().insert(
            entity.to_owned(),
            FakeEntity {
                state: state.to_owned(),
                name: name.to_owned(),
                area: area.map(str::to_owned),
            },
        );
        Arc::clone(self)
    }

    fn fail_service(self: &Arc<Self>, entity: &str) -> Arc<Self> {
        self.service_failures
            .lock()
            .unwrap()
            .insert(entity.to_owned());
        Arc::clone(self)
    }

    fn render(id: &str, entity: &FakeEntity) -> String {
        match &entity.area {
            Some(area) => format!(
                r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}","area_id":"{area}"}}}}"#,
                entity.state, entity.name
            ),
            None => format!(
                r#"{{"entity_id":"{id}","state":"{}","attributes":{{"friendly_name":"{}"}}}}"#,
                entity.state, entity.name
            ),
        }
    }

    /// Entities the transport was asked to *drive* (not merely read).
    fn driven(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                HomeRequest::Service { entity, .. } => Some(entity.as_str().to_owned()),
                _ => None,
            })
            .collect()
    }

    /// True if the transport ever saw a request *naming* this entity. The
    /// `/api/states` index names nobody, so this is exactly "was this entity
    /// read or driven".
    fn touched(&self, entity: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|call| match call {
            HomeRequest::State(id) => id.as_str() == entity,
            HomeRequest::Service { entity: id, .. } => id.as_str() == entity,
            HomeRequest::AllStates => false,
        })
    }
}

#[async_trait::async_trait]
impl HomeAssistantTransport for FakeHome {
    async fn send(
        &self,
        request: HomeRequest,
        cancel: CancellationToken,
    ) -> Result<String, HomeAssistantError> {
        self.calls.lock().unwrap().push(request.clone());
        if cancel.is_cancelled() {
            return Err(HomeAssistantError::Cancelled);
        }
        let entities = self.entities.lock().unwrap();
        match request {
            HomeRequest::AllStates => Ok(format!(
                "[{}]",
                entities
                    .iter()
                    .map(|(id, entity)| Self::render(id, entity))
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            HomeRequest::State(id) => entities
                .get(id.as_str())
                .map(|entity| Self::render(id.as_str(), entity))
                .ok_or(HomeAssistantError::UnknownEntity),
            HomeRequest::Service { entity, .. } => {
                if self
                    .service_failures
                    .lock()
                    .unwrap()
                    .contains(entity.as_str())
                {
                    return Err(HomeAssistantError::Rejected);
                }
                Ok("[]".to_owned())
            }
        }
    }
}

fn home_registry(transport: Arc<FakeHome>, allowlist: EntityAllowlist) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_home_assistant_tools(
        &mut registry,
        Arc::new(HomeAssistantClient::with_transport(transport)),
        Arc::new(allowlist),
    )
    .expect("home tools register");
    registry
}

fn allowlist(lights: &[&str], scenes: &[&str]) -> EntityAllowlist {
    let owned = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    EntityAllowlist::new(&[], &owned(lights), &owned(scenes), &[]).expect("valid allowlist")
}

// ---------------------------------------------------------------------------
// fixture: Spotify
// ---------------------------------------------------------------------------

const ACCESS_TOKEN: &str = "BQD-acceptance-access-token";
const REFRESH_TOKEN: &str = "AQC-acceptance-refresh-token";

#[derive(Debug, Clone)]
struct RecordedCall {
    method: HttpMethod,
    path: &'static str,
    query: Vec<(String, String)>,
    body: Option<String>,
}

impl RecordedCall {
    fn key(&self) -> String {
        let method = match self.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
        };
        format!("{method} {}", self.path)
    }
    fn q(&self, key: &str) -> Option<String> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    fn body(&self) -> String {
        self.body.clone().unwrap_or_default()
    }
}

#[derive(Default)]
struct FakeSpotify {
    routes: Mutex<std::collections::BTreeMap<String, std::collections::VecDeque<ApiResponse>>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl FakeSpotify {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue a body for `"<METHOD> <path>"`. Unrouted calls answer `204 No
    /// Content` — what Spotify's player endpoints really return.
    fn json(self: &Arc<Self>, key: &str, body: &str) -> Arc<Self> {
        self.routes
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_default()
            .push_back(ApiResponse::new(200, body));
        Arc::clone(self)
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn keys(&self) -> Vec<String> {
        self.calls().iter().map(RecordedCall::key).collect()
    }

    fn call(&self, key: &str) -> Option<RecordedCall> {
        self.calls().into_iter().find(|c| c.key() == key)
    }
}

#[async_trait::async_trait]
impl SpotifyTransport for FakeSpotify {
    async fn refresh_access_token(
        &self,
        _client_id: &str,
        refresh_token: &str,
        _cancel: CancellationToken,
    ) -> Result<TokenResponse, SpotifyError> {
        assert_eq!(
            refresh_token, REFRESH_TOKEN,
            "the host-resolved refresh token must reach the transport unchanged"
        );
        Ok(TokenResponse {
            access_token: AccessToken::new(ACCESS_TOKEN),
            expires_in_secs: 3600,
            rotated_refresh_token: None,
        })
    }

    async fn call(
        &self,
        token: &AccessToken,
        request: ApiRequest,
        _cancel: CancellationToken,
    ) -> Result<ApiResponse, SpotifyError> {
        assert_eq!(token.expose(), ACCESS_TOKEN);
        let recorded = RecordedCall {
            method: request.method,
            path: request.path,
            query: request.query.clone(),
            body: request.body.clone(),
        };
        let key = recorded.key();
        self.calls.lock().unwrap().push(recorded);
        let queued = self
            .routes
            .lock()
            .unwrap()
            .get_mut(&key)
            .and_then(std::collections::VecDeque::pop_front);
        Ok(queued.unwrap_or_else(|| ApiResponse::new(204, "")))
    }
}

fn spotify_registry(transport: Arc<FakeSpotify>) -> ToolRegistry {
    let config = SpotifyConfig::new(
        "acceptance-client-id",
        REFRESH_TOKEN,
        VolumePct::new(70).expect("valid cap"),
    )
    .with_device_aliases([("Kitchen".to_owned(), "kitchendeviceid0001".to_owned())]);
    let mut registry = ToolRegistry::new();
    register_spotify_tools(
        &mut registry,
        Arc::new(SpotifyClient::with_transport(config, transport)),
    )
    .expect("spotify tools register");
    registry
}

const DEVICES: &str = r#"{"devices": [
  {"id": "kitchendeviceid0001", "name": "Kitchen Sonos", "is_active": false},
  {"id": "deskdeviceid0002", "name": "Desk", "is_active": true}
]}"#;

const TAKE_ON_ME_SEARCH: &str = r#"{
  "artists": {"items": []},
  "tracks": {"items": [
    {"name": "Take On Me", "uri": "spotify:track:2WfaOiMkCvy7F5fcp2zZ8L",
     "artists": [{"name": "a-ha"}]}
  ]}
}"#;

const ABBA_SEARCH: &str = r#"{
  "artists": {"items": [
    {"name": "ABBA", "uri": "spotify:artist:0LcJLqbBmaGUft1e9Mm8HV", "genres": ["europop"]}
  ]},
  "tracks": {"items": [
    {"name": "Dancing Queen", "uri": "spotify:track:0GjEhVFGZW8afUYGChu3Rr",
     "artists": [{"name": "ABBA"}]}
  ]}
}"#;

const OWN_PLAYLISTS: &str = r#"{"items": [
  {"name": "Running", "uri": "spotify:playlist:37i9dQZF1DXOWNqUlibrary",
   "tracks": {"total": 42}, "owner": {"display_name": "Benjamin"}}
]}"#;

const PUBLIC_RUNNING_PLAYLIST: &str = r#"{"playlists": {"items": [
  {"name": "Running", "uri": "spotify:playlist:37i9dQZF1DXpublicrunn",
   "owner": {"display_name": "Someone Else"}}
]}}"#;

// ---------------------------------------------------------------------------
// fixture: MPRIS
// ---------------------------------------------------------------------------

/// The session bus, recorded rather than driven. The outermost hop and the only
/// thing faked on the media path — everything above it (grammar → proposal →
/// policy → executor → audit) is production code.
#[derive(Default)]
struct RecordingMedia {
    snapshot: Mutex<MediaSnapshot>,
    transports: Mutex<Vec<(String, TransportCommand)>>,
}

impl RecordingMedia {
    fn with(players: impl IntoIterator<Item = PlayerState>) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(MediaSnapshot::new(players)),
            transports: Mutex::new(Vec::new()),
        })
    }

    fn transports(&self) -> Vec<(String, TransportCommand)> {
        self.transports.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl MediaController for RecordingMedia {
    async fn snapshot(&self, _cancel: CancellationToken) -> Result<MediaSnapshot, MediaError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn transport(
        &self,
        player: &PlayerId,
        command: TransportCommand,
        _cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        self.transports
            .lock()
            .unwrap()
            .push((player.as_str().to_owned(), command));
        Ok(())
    }

    async fn set_volume(
        &self,
        _player: &PlayerId,
        _volume: VolumePct,
        _cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        Ok(())
    }
}

fn spotify_player(title: &str, artist: &str, album: &str) -> PlayerState {
    PlayerState::new(
        PlayerId::new("org.mpris.MediaPlayer2.spotify").expect("valid bus name"),
        Some("Spotify"),
        PlaybackStatus::Playing,
        TrackMetadata::sanitized(Some(title), Some(artist), Some(album), None, None),
        None,
    )
}

/// The HUD canvas, recorded. `NowPlayingHud` publishes through this seam, so a
/// scenario can assert the card really reached it.
#[derive(Default)]
struct RecordingCanvas {
    published: Mutex<Vec<jarvis_contracts::deepdive::HudCanvasDto>>,
}

impl RecordingCanvas {
    fn published(&self) -> Vec<jarvis_contracts::deepdive::HudCanvasDto> {
        self.published.lock().unwrap().clone()
    }
}

impl jarvisd::cards::CanvasSink for RecordingCanvas {
    fn publish(&self, canvas: jarvis_contracts::deepdive::HudCanvasDto) {
        self.published.lock().unwrap().push(canvas);
    }
}

// ===========================================================================
// Evidence #1 — the full voice round trip
// ===========================================================================

/// **Exit evidence #1** (the *functional* half): push-to-talk PCM → Wyoming STT
/// → a final transcript → a run started through the very same
/// `RunApi::start_turn` a typed message takes → the streamed answer → clause
/// segmented TTS back to the client as bracketed binary audio, with the
/// transcript durably committed as an ordinary user message.
///
/// **The NFR-04 number is NOT claimed here.** The speech engines are fixtures
/// that answer instantly; a latency measured against them is a measurement of
/// this test, not of faster-whisper and Piper on the reference machine. What is
/// repeatable here is the *shape* of the round trip. For the daemon's own share
/// of the budget, run `cargo xtask perf --voice`, which measures exactly that
/// and says so ("MODEL TIME EXCLUDED"). See `docs/milestones/M5-acceptance.md`
/// §3.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence1_a_full_voice_round_trip_answers_aloud_what_it_heard(pool: PgPool) {
    let stt_url = voice_fixture::stt_returning("tell me a bedtime story").await;
    let tts_url = voice_fixture::tts_streaming(3, 1024, Duration::from_millis(5)).await;
    let harness = Harness::start(
        pool,
        FakeModel::streaming(["Once upon a time."]),
        VoiceWiring {
            transcriber: Some(Arc::new(jarvis_adapters::wyoming::WyomingClient::new(
                "stt",
                addr_of(&stt_url),
            ))),
            synthesizer: Some(Arc::new(jarvis_adapters::wyoming::WyomingClient::new(
                "tts",
                addr_of(&tts_url),
            ))),
        },
    )
    .await;

    let mut socket = harness.connect().await;
    socket
        .send_control(VoiceSocket::start_stream("m5", Some(VOICE_SESSION)))
        .await;
    socket.send_pcm(vec![0u8; 640]).await;
    socket.send_control(VoiceSocket::stop_stream("m5")).await;

    let received = socket
        .collect_until(
            BUDGET,
            |r| matches!(r, Received::Event { event_type, .. } if event_type == "voice.speak.stop"),
        )
        .await;
    let events = events_of(&received);

    // The whole leg, in order: heard → ran → answered → spoke.
    for expected in [
        "voice.transcript",
        "run.started",
        "text.delta",
        "voice.speak.start",
        "voice.speak.stop",
    ] {
        assert!(
            events.contains(&expected),
            "missing {expected} in the round trip: {events:?}"
        );
    }
    assert_eq!(
        payload_of(&received, "voice.transcript").expect("a transcript")["text"],
        "tell me a bedtime story"
    );
    assert!(
        audio_frame_count(&received) > 0,
        "the answer must come back as audio: {events:?}"
    );
    assert_eq!(
        payload_of(&received, "voice.speak.stop").expect("a stop")["reason"],
        "completed"
    );

    // Durable evidence that voice took the ordinary path: the transcript is
    // committed as a user message exactly as if it had been typed (invariant 1).
    let mut messages = Vec::new();
    for _ in 0..200 {
        let timeline = harness.timeline().await;
        messages = timeline["items"]
            .as_array()
            .expect("timeline items")
            .iter()
            .filter(|item| item["type"] == "message")
            .cloned()
            .collect::<Vec<_>>();
        if messages.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        messages.len() >= 2,
        "user + assistant message: {messages:?}"
    );
    assert_eq!(messages[0]["message"]["role"], "user");
    assert_eq!(
        messages[0]["message"]["content"][0]["text"], "tell me a bedtime story",
        "the transcript is an ordinary user message, not a voice-only shortcut"
    );
    assert_eq!(messages[1]["message"]["role"], "assistant");

    harness.shutdown.cancel();
}

// ===========================================================================
// Evidence #2 — safely control one allowlisted HA entity
// ===========================================================================

/// **Exit evidence #2**, the R1 half: an allowlisted light is driven end to end
/// through the real authorization path — `policy::evaluate` auto-authorizes the
/// reversible single-entity mutation, the executor pre-reads so the undo is
/// real, HA is driven once, and BOTH the policy decision and the effect land on
/// the hash-chained audit log.
///
/// The "safely" half is the second drive: a light the owner never allowlisted
/// is refused, and the transport never even *reads* it — the allowlist is
/// enforced before any I/O, so a proposal cannot be used to probe the house.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence2_an_allowlisted_light_is_driven_through_policy_and_audit(pool: PgPool) {
    let home = FakeHome::new()
        .add("light.kitchen_lamp", "off", "Kitchen lamp", Some("kitchen"))
        .add("light.hallway", "off", "Hallway", Some("hall"));
    let registry = home_registry(
        Arc::clone(&home),
        allowlist(&["light.kitchen_lamp"], &["scene.movie_night"]),
    );
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());

    // -- the allowlisted entity: proposed, authorized, executed, audited ----
    let sink = RecordingSink::default();
    let model = propose_then_answer(
        proposal(
            "home.set_light",
            [
                ("entity_id", V::str("light.kitchen_lamp")),
                ("state", V::str("on")),
            ],
        ),
        "The kitchen lamp is on.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["home:control"],
        "turn on the kitchen lamp",
        &sink,
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert!(
        sink.states().contains(&RunState::PolicyReview),
        "every effect passes policy first: {:?}",
        sink.states()
    );
    assert!(sink.states().contains(&RunState::ToolRunning));
    assert!(
        !sink.states().contains(&RunState::WaitingApproval),
        "a reversible single-entity light is R1: no approval card"
    );
    assert_eq!(
        home.driven(),
        vec!["light.kitchen_lamp".to_owned()],
        "exactly the proposed entity was driven, once"
    );

    let types = audit_types(&pool).await;
    assert!(
        types.contains(&"policy.auto_authorized".to_owned()),
        "the policy decision is audited: {types:?}"
    );
    assert!(
        types.contains(&"tool.executed".to_owned()),
        "the physical effect is audited: {types:?}"
    );
    let mut conn = pool.acquire().await.expect("acquire");
    assert!(
        jarvis_infra::audit::verify_chain(&mut conn)
            .await
            .expect("verify chain")
            >= 2,
        "the audit chain covers the decision and the effect, intact"
    );
    assert!(
        grant_rows(&pool).await.is_empty(),
        "R1 mints no grant — the grant is the R2+ authorization, not a receipt"
    );

    // -- a non-allowlisted entity is refused before any I/O ----------------
    let sink = RecordingSink::default();
    let model = propose_then_answer(
        proposal(
            "home.set_light",
            [
                ("entity_id", V::str("light.hallway")),
                ("state", V::str("on")),
            ],
        ),
        "I could not do that.",
    );
    drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["home:control"],
        "turn on the hallway light",
        &sink,
    )
    .await;

    assert!(
        !home.driven().contains(&"light.hallway".to_owned()),
        "a light outside the allowlist is never driven: {:?}",
        home.driven()
    );
    assert!(
        !home.touched("light.hallway"),
        "nor read — the allowlist refuses before any HA I/O, so a proposal \
         cannot be used to probe the house"
    );
}

/// **Exit evidence #2**, the R2 half: a broad-effect home action (a scene, whose
/// blast radius the owner cannot see from the entity name) parks for human
/// approval, and only the approval mints the single-use `ExecutionGrant` that
/// the executor validates and consumes. Approval → grant → execute → audit, all
/// on real infrastructure.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence2_a_broad_home_action_needs_approval_and_a_single_use_grant(pool: PgPool) {
    let home = FakeHome::new().add(
        "scene.movie_night",
        "unknown",
        "Movie night",
        Some("living_room"),
    );
    let registry = home_registry(
        Arc::clone(&home),
        allowlist(&["light.kitchen_lamp"], &["scene.movie_night"]),
    );
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());

    // The human, acting on the `approval.requested` card the gate persisted.
    let approver = tokio::spawn(approve_when_requested(pool.clone(), Arc::clone(&gate)));

    let sink = RecordingSink::default();
    let model = propose_then_answer(
        proposal(
            "home.execute_scene",
            [
                ("entity_id", V::str("scene.movie_night")),
                ("friendly_name", V::str("Movie night")),
            ],
        ),
        "Movie night is on.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["home:control"],
        "start movie night",
        &sink,
    )
    .await;
    approver.await.expect("the approver task joins");

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert!(
        sink.states().contains(&RunState::WaitingApproval),
        "a broad-effect home action must park for a human: {:?}",
        sink.states()
    );
    assert_eq!(
        home.driven(),
        vec!["scene.movie_night".to_owned()],
        "the approved scene ran, exactly once"
    );

    // The grant is the authorization, and it is spent.
    assert_eq!(
        grant_rows(&pool).await,
        vec![("home.execute_scene".to_owned(), true)],
        "one grant, minted for this tool and consumed single-use"
    );

    let types = audit_types(&pool).await;
    for expected in [
        "approval.requested",
        "approval.resolved",
        "policy.approval_requested",
        "grant.minted",
        "tool.executed",
    ] {
        assert!(
            types.contains(&expected.to_owned()),
            "missing {expected} on the audit chain: {types:?}"
        );
    }
    let mut conn = pool.acquire().await.expect("acquire");
    jarvis_infra::audit::verify_chain(&mut conn)
        .await
        .expect("the whole approval → grant → execute chain verifies");
}

// ===========================================================================
// Evidence #3 — "pause the music" with zero LLM calls
// ===========================================================================

/// **Exit evidence #3**: the recognized transport utterance reaches the MPRIS
/// player through the ordinary policy-gated tool path, and the reasoning
/// provider is **never opened**.
///
/// `FakeModel::opened()` is the assertion that matters. "It returned the right
/// text" and "it cost no quota" are different claims, and the roadmap makes the
/// second one; a regression that quietly delegated to the provider would still
/// produce a correct-looking answer.
///
/// # One command, one effect (regression: D-M5-1, fixed)
///
/// F5.8 found — and this scenario used to pin — a defect where the recognized
/// command re-fired on **every** replan turn: `DeterministicFirstProvider`
/// classifies the slice before the first `[Untrusted …]` marker, which on a
/// replan is still the user's original utterance, so it re-proposed the same
/// call once per model turn until `max_model_turns` (8) tripped. One spoken
/// "pause the music" drove eight `Pause` calls and ended `Failed` on budget;
/// the home route would have made eight real service calls at physical
/// hardware.
///
/// The fix is structural: the orchestrator hands each turn a
/// `ModelRequest::prior_tool_result`, and a command whose tool has already run
/// reports the executor's own sentence instead of proposing again. The
/// assertions below therefore demand **exactly one** transport call and a
/// `Completed` run — a re-introduced loop fails this scenario, not merely a unit
/// test. See `docs/milestones/M5-acceptance.md` §4.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence3_pause_the_music_drives_the_player_with_zero_model_calls(pool: PgPool) {
    let media = RecordingMedia::with([spotify_player("Dancing Queen", "ABBA", "Arrival")]);
    let mut registry = ToolRegistry::new();
    register_media_tools(
        &mut registry,
        Arc::clone(&media) as Arc<dyn MediaController>,
        VolumePct::new(70).expect("valid cap"),
        None,
    )
    .expect("media tools register");

    let inner = Arc::new(FakeModel::streaming(["the provider must not be consulted"]));
    let provider = DeterministicFirstProvider::new(Arc::clone(&inner) as Arc<dyn ModelProvider>);

    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());
    let sink = RecordingSink::default();

    let run = drive(
        &provider,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["media:control"],
        "pause the music",
        &sink,
    )
    .await;

    // -- the roadmap bullet -------------------------------------------------
    assert!(
        !inner.opened(),
        "\"pause the music\" must cost zero LLM calls (docs/08 §1, M5 evidence #3)"
    );
    let transports = media.transports();
    assert_eq!(
        transports,
        vec![(
            "org.mpris.MediaPlayer2.spotify".to_owned(),
            TransportCommand::Pause
        )],
        "one spoken command is exactly one effect, on the unambiguous active \
         player — a replan must not re-propose it (D-M5-1)"
    );
    assert_eq!(
        run.state,
        RunState::Completed,
        "the run ends cleanly, not on the model-turn budget (outcome: {:?})",
        run.outcome
    );
    assert!(
        sink.states().contains(&RunState::PolicyReview),
        "recognition is not authorization: the proposal still passed policy: {:?}",
        sink.states()
    );
    let types = audit_types(&pool).await;
    assert!(
        types.contains(&"tool.executed".to_owned()),
        "a quota-free effect is still an audited effect: {types:?}"
    );

    // -- what the owner is actually told (D-M5-1's honesty half) ------------
    // The replan turn speaks the executor's own sentence rather than a canned
    // acknowledgement, so a partial or failed outcome survives to the user.
    let spoken = sink.text();
    assert_eq!(
        spoken, "Paused Spotify.",
        "the answer is what the tool reported, verbatim: {spoken}"
    );
    assert!(
        !spoken.contains("must not be consulted"),
        "and it is the executor's text, not the provider's: {spoken}"
    );
}

/// The home half of D-M5-1, at the same seam. `home.set_light` is the other
/// route that turns a recognized utterance into a proposal, and it is the one
/// where a per-turn repeat would be eight real service calls at a physical
/// lamp — so the "exactly once" property is pinned here too, not inferred from
/// the media scenario sharing the code path.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence2_a_spoken_light_command_drives_the_lamp_exactly_once(pool: PgPool) {
    let home = FakeHome::new().add("light.desk_lamp", "off", "Desk lamp", Some("study"));
    let registry = home_registry(Arc::clone(&home), allowlist(&["light.desk_lamp"], &[]));

    let inner = Arc::new(FakeModel::streaming(["the provider must not be consulted"]));
    let provider = DeterministicFirstProvider::new(Arc::clone(&inner) as Arc<dyn ModelProvider>)
        .with_light_targets(Arc::new(DeskLampTargets));

    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());
    let sink = RecordingSink::default();

    let run = drive(
        &provider,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["home:control"],
        "turn on desk lamp",
        &sink,
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert!(!inner.opened(), "the home grammar costs no quota either");
    assert_eq!(
        home.driven(),
        vec!["light.desk_lamp".to_owned()],
        "one spoken command is ONE service call at the lamp — the replan turn \
         must not re-actuate it (D-M5-1)"
    );
    assert!(
        sink.states().contains(&RunState::PolicyReview),
        "recognition is not authorization: {:?}",
        sink.states()
    );
    assert!(
        audit_types(&pool)
            .await
            .contains(&"tool.executed".to_owned()),
        "the physical effect is audited"
    );
    assert_eq!(
        sink.text(),
        "Desk lamp (light.desk_lamp) is now on.",
        "the owner hears what the executor reported, verbatim"
    );
}

/// The host's spoken-target → entity resolution for the scenario above; the
/// production wiring reads it from the HA registry.
struct DeskLampTargets;

impl jarvis_application::home::LightTargetResolver for DeskLampTargets {
    fn resolve_light(&self, spoken_target: &str) -> Option<String> {
        (spoken_target == "desk lamp").then(|| "light.desk_lamp".to_owned())
    }
}

// ===========================================================================
// Evidence #4/#5/#6 — Spotify
// ===========================================================================

/// **Exit evidence #4**: a track found by search is played on a *chosen*
/// Connect device — the search result feeds the play call, and the device the
/// caller named resolves to that device's id on the play request (not the
/// active one).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence4_a_searched_track_plays_on_the_chosen_device(pool: PgPool) {
    let spotify = FakeSpotify::new()
        .json("GET /search", TAKE_ON_ME_SEARCH)
        .json("GET /me/player/devices", DEVICES);
    let registry = spotify_registry(Arc::clone(&spotify));
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());

    // 1. R0 search — the catalogue lookup that finds the URI.
    let sink = RecordingSink::default();
    let model = propose_then_answer(
        proposal(
            "spotify.search",
            [("query", V::str("take on me")), ("types", V::str("track"))],
        ),
        "Found it.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["media:search"],
        "find take on me",
        &sink,
    )
    .await;
    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert!(
        spotify.keys().contains(&"GET /search".to_owned()),
        "the search really went through the adapter: {:?}",
        spotify.keys()
    );

    // 2. R1 play of exactly that URI, on the named device.
    let sink = RecordingSink::default();
    let model = propose_then_answer(
        proposal(
            "spotify.play",
            [
                ("uri", V::str("spotify:track:2WfaOiMkCvy7F5fcp2zZ8L")),
                ("device", V::str("Kitchen Sonos")),
            ],
        ),
        "Playing it in the kitchen.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["media:control"],
        "play it in the kitchen",
        &sink,
    )
    .await;
    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);

    let play = spotify
        .call("PUT /me/player/play")
        .expect("playback was started");
    assert!(
        play.body()
            .contains("\"uris\":[\"spotify:track:2WfaOiMkCvy7F5fcp2zZ8L\"]"),
        "the searched track is what plays: {}",
        play.body()
    );
    assert_eq!(
        play.q("device_id").as_deref(),
        Some("kitchendeviceid0001"),
        "the chosen device, not wherever playback happened to be"
    );
    assert!(
        !sink.states().contains(&RunState::WaitingApproval),
        "playing a track is R1 — reversible, no approval theatre"
    );
}

/// **Exit evidence #5**: an artist-only request starts that artist's own
/// context with shuffle on, and asks **nothing** (ADR-022 (1)). The absence of
/// a clarifying question is the property — a picker or a "did you mean" here
/// would be the failure.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence5_play_abba_starts_shuffled_top_tracks_without_asking(pool: PgPool) {
    let spotify = FakeSpotify::new().json("GET /search", ABBA_SEARCH);
    let registry = spotify_registry(Arc::clone(&spotify));
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());
    let sink = RecordingSink::default();

    let model = propose_then_answer(
        proposal("spotify.play", [("query", V::str("abba"))]),
        "Playing ABBA, shuffled.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["media:control"],
        "play abba",
        &sink,
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert_eq!(
        spotify.keys(),
        vec![
            "GET /search".to_owned(),
            "PUT /me/player/shuffle".to_owned(),
            "PUT /me/player/play".to_owned()
        ],
        "artist context = shuffle on, then the artist's own context_uri"
    );
    assert_eq!(
        spotify
            .call("PUT /me/player/shuffle")
            .expect("shuffle was set")
            .q("state")
            .as_deref(),
        Some("true")
    );
    assert!(
        spotify
            .call("PUT /me/player/play")
            .expect("playback started")
            .body()
            .contains("\"context_uri\":\"spotify:artist:0LcJLqbBmaGUft1e9Mm8HV\""),
        "the artist's context, not one track"
    );
    let observation = tool_observation(&model);
    assert!(
        observation.contains("shuffled"),
        "the result says what it did: {observation}"
    );
    assert!(
        !observation.contains('?'),
        "no unnecessary clarification: {observation}"
    );
}

/// **Exit evidence #6**: "play playlist X" resolves the owner's **own** saved
/// library first. Both a saved playlist and a public one share the name here;
/// the library must win, and the public catalogue must not even be consulted
/// (ADR-022 (2)).
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence6_play_playlist_resolves_the_owners_library_before_public_search(pool: PgPool) {
    let spotify = FakeSpotify::new()
        .json("GET /me/playlists", OWN_PLAYLISTS)
        .json("GET /search", PUBLIC_RUNNING_PLAYLIST);
    let registry = spotify_registry(Arc::clone(&spotify));
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());
    let sink = RecordingSink::default();

    let model = propose_then_answer(
        proposal("spotify.play_playlist", [("name", V::str("running"))]),
        "Playing your Running playlist.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["media:control"],
        "play my running playlist",
        &sink,
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert_eq!(
        spotify.keys(),
        vec![
            "GET /me/playlists".to_owned(),
            "PUT /me/player/play".to_owned()
        ],
        "a library hit must not fall through to public search"
    );
    assert!(
        spotify
            .call("PUT /me/player/play")
            .expect("playback started")
            .body()
            .contains("spotify:playlist:37i9dQZF1DXOWNqUlibrary"),
        "the owner's own playlist URI, not the public one of the same name"
    );
    let observation = tool_observation(&model);
    assert!(
        observation.contains("your playlist"),
        "the answer says whose playlist it is: {observation}"
    );
}

// ===========================================================================
// Evidence #7 — "what's playing" as a first-class query
// ===========================================================================

/// **Exit evidence #7** (FR-32): "what's playing" is answered from the MPRIS
/// metadata the media bar already reads — spoken text plus a now-playing card —
/// with **zero model calls** and no tool authority at all.
///
/// Two assertions carry the bullet: `opened()` is false (the quota claim), and
/// the card really reached the HUD canvas (the FR-32 claim). Text alone would
/// prove neither.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence7_whats_playing_answers_with_a_card_and_zero_model_calls(pool: PgPool) {
    let media = RecordingMedia::with([spotify_player("Dancing Queen", "ABBA", "Arrival")]);
    let canvas = Arc::new(RecordingCanvas::default());
    let inner = Arc::new(FakeModel::streaming(["the provider must not be consulted"]));
    let provider = DeterministicFirstProvider::new(Arc::clone(&inner) as Arc<dyn ModelProvider>)
        .with_now_playing(Arc::new(jarvisd::media::NowPlayingHud::new(
            Arc::clone(&media) as Arc<dyn MediaController>,
            Some(Arc::clone(&canvas) as Arc<dyn jarvisd::cards::CanvasSink>),
        )));

    // No tool plane at all: the query is an *observation*, so it must be
    // answerable with no tool authority whatsoever (invariant 1).
    let assembler = EchoAssembler;
    let checkpointer = RecordingCheckpointer::default();
    let clock = ManualClock::at_unix(1_700_000_000);
    let sink = RecordingSink::default();
    let run = Orchestrator {
        model: &provider,
        context: &assembler,
        checkpointer: &checkpointer,
        sink: &sink,
        clock: &clock,
        user_id: None,
        tools: None,
    }
    .drive(
        new_run(),
        RunInput {
            text: "what's playing".to_owned(),
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);
    assert!(
        !inner.opened(),
        "\"what's playing\" must cost zero LLM calls (docs/08 §1, M5 evidence #7)"
    );

    let spoken = sink.text();
    assert!(
        spoken.contains("Dancing Queen") && spoken.contains("ABBA") && spoken.contains("Spotify"),
        "the spoken answer names the track, artist and player: {spoken}"
    );

    let published = canvas.published();
    assert_eq!(published.len(), 1, "exactly one now-playing card");
    assert_eq!(
        published[0].label, "Now playing",
        "the card is labelled for what it is"
    );
    assert!(
        !published[0].cards.is_empty(),
        "the card carries the track facts, not just a heading"
    );
    assert_eq!(
        published[0].action,
        jarvis_contracts::deepdive::CanvasActionDto::Extend,
        "an aside extends the canvas; it never shelves work the owner did not put down"
    );

    // No effect, so nothing to audit and nothing to authorize.
    assert!(
        audit_types(&pool).await.is_empty(),
        "a read-only observation writes no effect audit"
    );
}

// ===========================================================================
// Evidence #8 — a plural area command, and honest partial failure
// ===========================================================================

/// **Exit evidence #8** (FR-28, ADR-018): "turn on the living room lamps"
/// resolves to the concrete allowlisted entity **set**, and when one of the
/// three does not respond the result says so — the count leads ("2 of 3"), the
/// survivors are named, and the failure is named. It is emphatically **not**
/// reported as plain success.
///
/// The seeded failure is the whole point: an all-succeed path would demonstrate
/// nothing about this bullet.
#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn evidence8_a_plural_area_command_reports_partial_failure_honestly(pool: PgPool) {
    let home = FakeHome::new()
        .add("light.lr_left", "off", "Left lamp", Some("living_room"))
        .add("light.lr_right", "off", "Right lamp", Some("living_room"))
        .add("light.lr_corner", "off", "Corner lamp", Some("living_room"))
        // Not in the living room: it must not be swept up by the plural command.
        .add("light.kitchen_lamp", "off", "Kitchen lamp", Some("kitchen"))
        // One of the three simply does not answer.
        .fail_service("light.lr_corner");
    let registry = home_registry(
        Arc::clone(&home),
        allowlist(
            &[
                "light.lr_left",
                "light.lr_right",
                "light.lr_corner",
                "light.kitchen_lamp",
            ],
            &[],
        ),
    );
    let audit = PgAuditSink::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let gate = JarvisApprovalGate::new(pool.clone());
    let sink = RecordingSink::default();

    let model = propose_then_answer(
        proposal(
            "home.set_area_lights",
            [("area", V::str("living room")), ("state", V::str("on"))],
        ),
        "Two of the three came on.",
    );
    let run = drive(
        &model,
        &registry,
        &audit,
        &*gate,
        &grants,
        &["home:control"],
        "turn on the living room lamps",
        &sink,
    )
    .await;

    assert_eq!(run.state, RunState::Completed, "outcome: {:?}", run.outcome);

    // The plural command resolved to the *set*, and only the area's members.
    let mut driven = home.driven();
    driven.sort();
    assert_eq!(
        driven,
        vec![
            "light.lr_corner".to_owned(),
            "light.lr_left".to_owned(),
            "light.lr_right".to_owned()
        ],
        "all three living-room lamps were attempted (including the failing one)"
    );
    assert!(
        !home.touched("light.kitchen_lamp"),
        "an allowlisted light in another area is not swept up by the area command"
    );

    // The honesty clause.
    let observation = tool_observation(&model);
    assert!(
        observation.contains("2 of 3"),
        "the count leads and is not rounded up: {observation}"
    );
    assert!(
        observation.contains("Corner lamp") && observation.contains("did not respond"),
        "the light that failed is named: {observation}"
    );
    assert!(
        observation.contains("Left lamp") && observation.contains("Right lamp"),
        "the lights that worked are named: {observation}"
    );
    assert!(
        !observation.contains("all 3") && !observation.contains("all three"),
        "a partial result must never be reported as full success: {observation}"
    );
}

/// The tool result the orchestrator folded back into the next model turn — i.e.
/// what the assistant was actually told happened, read out of the replan prompt
/// between the untrusted-context markers.
///
/// Reading it here rather than from the streamed text is deliberate: the
/// streamed text is the *scripted* model answer, so asserting on it would prove
/// nothing about the executor.
fn tool_observation(model: &FakeModel) -> String {
    const OPEN: &str = "[Untrusted tool result";
    const CLOSE: &str = " [End untrusted tool result]";
    let prompt = model
        .last_prompt()
        .expect("the run opened the provider at least once");
    let marker = prompt
        .find(OPEN)
        .expect("the tool result was folded back into the next turn");
    let rest = &prompt[marker..];
    let start = rest.find("] ").expect("the open marker closes") + 2;
    let end = rest.find(CLOSE).expect("the untrusted block is terminated");
    rest[start..end].to_owned()
}
