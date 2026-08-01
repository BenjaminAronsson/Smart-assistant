//! List surface (F3b.8, FR-34, docs/02 §11e, ADR-024): the REST entry points,
//! the wire projection, and the HUD list card.
//!
//! **[`ListApi`]** serves `GET/POST /api/v1/lists`, `GET /api/v1/lists/{id}`,
//! `POST /api/v1/lists/{id}/items`, `PATCH /api/v1/lists/{id}/items/{itemId}`,
//! `DELETE /api/v1/lists/{id}/items/{itemId}`, `POST /api/v1/lists/command`, and
//! `POST /api/v1/lists/{id}/promote`. Owner-driven and authenticated, the same
//! shape as the timer and media surfaces: this is a human speaking to, or
//! tapping on, their own paired device.
//!
//! **Invariant 1 note.** There is deliberately **no registered list tool** in
//! this feature, so no model output reaches these endpoints — the policy engine
//! is untouched by them, exactly as for timers. `POST /lists/command` in
//! particular takes an *utterance from the owner's own device* and runs the
//! deterministic grammar (`jarvis_domain::lists::parse_list_command`), which is
//! a pure function with no model call in it; the resulting write is a row in the
//! owner's own list store, not a policy-gated side effect. If a later feature
//! wants the model to be able to add to a list, that arrives as a registered
//! tool going through `policy::evaluate` — never as a second door into here.
//!
//! **Grammar refusals are refusals.** An utterance the grammar does not
//! unambiguously recognize is `list.unrecognized_command` (422), never a guess:
//! putting untrusted text on a list the owner did not name is exactly the
//! failure ADR-016 forbids.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Json};
use jarvis_application::lists::{
    CommandEffect, CommandIds, CommandOutcome, ListsError, ListsService,
};
use jarvis_contracts::cards::HudCardDto;
use jarvis_contracts::errors::ErrorCode;
use jarvis_contracts::lists::{
    AddListItemRequest, CheckListItemRequest, CreateListRequest, ListCommandRequest,
    ListCommandResponse, ListDto, ListEffectDto, ListIndexResponse, PromoteListResponse,
};
use jarvis_domain::ids::{ArtifactId, ListId, ListItemId, RunId};
use jarvis_domain::lists::{ItemList, ItemText, ListName, parse_list_command};
use tokio_util::sync::CancellationToken;

use crate::auth::DeviceContext;
use crate::problem::problem;

/// Project a domain list onto the wire.
pub fn to_list_dto(list: &ItemList) -> ListDto {
    ListDto::from(list)
}

/// Build the HUD list card for a list (docs/12 §2.3, FR-34). The card id is a
/// presentation handle derived from the list id; the list id itself rides in its
/// own field because the check-off tap posts against it.
pub fn to_list_card(list: &ItemList) -> HudCardDto {
    HudCardDto::List {
        id: format!("list-{}", list.id()),
        list_id: list.id().clone(),
        list: to_list_dto(list),
    }
}

/// State for the list routes. Cloneable so it can be axum route state.
#[derive(Clone)]
pub struct ListApi {
    service: Arc<ListsService>,
}

impl ListApi {
    pub fn new(service: Arc<ListsService>) -> Self {
        Self { service }
    }
}

fn actor(device: &DeviceContext) -> String {
    format!("device:{}", device.device_id)
}

/// `GET /api/v1/lists` — every list, name-ordered.
#[tracing::instrument(skip_all)]
pub async fn index(State(api): State<ListApi>) -> Result<Json<ListIndexResponse>, Response> {
    let lists = api.service.all().await.map_err(service_problem)?;
    Ok(Json(ListIndexResponse {
        lists: lists.iter().map(to_list_dto).collect(),
    }))
}

/// `GET /api/v1/lists/{id}` — one list with its items.
#[tracing::instrument(skip_all, fields(list.id = %id))]
pub async fn get(
    State(api): State<ListApi>,
    Path(id): Path<String>,
) -> Result<Json<ListDto>, Response> {
    let id = parse_list_id(&id).map_err(fault_response)?;
    let list = api.service.get(&id).await.map_err(service_problem)?;
    Ok(Json(to_list_dto(&list)))
}

/// `POST /api/v1/lists` — create a named list, or return the existing one with
/// the same normalized key. Idempotent on the key by design: "add milk to the
/// shopping list" must work before a shopping list exists, and two devices
/// naming the same list must converge rather than fork.
#[tracing::instrument(skip_all)]
pub async fn create(
    State(api): State<ListApi>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<CreateListRequest>,
) -> Result<(StatusCode, Json<ListDto>), Response> {
    let cancel = CancellationToken::new();
    let name = ListName::new(&req.name).map_err(|e| bad_request(&e.to_string()))?;
    // The id is minted here (the host owns randomness; the domain only
    // validates) — ULID, so the index is naturally creation-ordered too.
    let id: ListId = crate::auth::fresh_id();
    let list = api
        .service
        .ensure_list(id, name, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    Ok((StatusCode::CREATED, Json(to_list_dto(&list))))
}

/// `POST /api/v1/lists/{id}/items` — append one line.
#[tracing::instrument(skip_all, fields(list.id = %id))]
pub async fn add_item(
    State(api): State<ListApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<AddListItemRequest>,
) -> Result<(StatusCode, Json<ListDto>), Response> {
    let cancel = CancellationToken::new();
    let id = parse_list_id(&id).map_err(fault_response)?;
    let text = ItemText::new(&req.text).map_err(|e| bad_request(&e.to_string()))?;
    let item_id: ListItemId = crate::auth::fresh_id();
    let list = api
        .service
        .add_item(&id, item_id, text, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    Ok((StatusCode::CREATED, Json(to_list_dto(&list))))
}

/// `PATCH /api/v1/lists/{id}/items/{itemId}` — the card's check-off tap.
#[tracing::instrument(skip_all, fields(list.id = %id, item.id = %item_id))]
pub async fn check_item(
    State(api): State<ListApi>,
    Path((id, item_id)): Path<(String, String)>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<CheckListItemRequest>,
) -> Result<Json<ListDto>, Response> {
    let cancel = CancellationToken::new();
    let id = parse_list_id(&id).map_err(fault_response)?;
    let item_id = parse_item_id(&item_id).map_err(fault_response)?;
    let list = api
        .service
        .set_checked(&id, &item_id, req.checked, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    Ok(Json(to_list_dto(&list)))
}

/// `DELETE /api/v1/lists/{id}/items/{itemId}` — take a line off the list.
#[tracing::instrument(skip_all, fields(list.id = %id, item.id = %item_id))]
pub async fn remove_item(
    State(api): State<ListApi>,
    Path((id, item_id)): Path<(String, String)>,
    Extension(device): Extension<DeviceContext>,
) -> Result<Json<ListDto>, Response> {
    let cancel = CancellationToken::new();
    let id = parse_list_id(&id).map_err(fault_response)?;
    let item_id = parse_item_id(&item_id).map_err(fault_response)?;
    let list = api
        .service
        .remove_item(&id, &item_id, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    Ok(Json(to_list_dto(&list)))
}

/// `POST /api/v1/lists/command` — the deterministic grammar (ADR-024).
///
/// Zero model calls, which is what makes lists work offline, in degraded mode,
/// and with the quota exhausted. An unrecognized phrasing is refused, not
/// guessed.
#[tracing::instrument(skip_all)]
pub async fn command(
    State(api): State<ListApi>,
    Extension(device): Extension<DeviceContext>,
    Json(req): Json<ListCommandRequest>,
) -> Result<Json<ListCommandResponse>, Response> {
    let cancel = CancellationToken::new();
    let Some(parsed) = parse_list_command(&req.utterance) else {
        return Err(problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::ListUnrecognizedCommand,
            "that is not clearly a list command; name the list explicitly",
            None,
        ));
    };
    tracing::info!(list.verb = parsed.verb(), "list grammar command");
    let ids = CommandIds {
        list: crate::auth::fresh_id(),
        item: crate::auth::fresh_id(),
    };
    let CommandOutcome { list, effect } = api
        .service
        .apply(&parsed, ids, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    let (effect, item_id) = match effect {
        CommandEffect::Added(id) => (ListEffectDto::Added, Some(id)),
        CommandEffect::Removed(id) => (ListEffectDto::Removed, Some(id)),
        CommandEffect::CheckedOff(id) => (ListEffectDto::CheckedOff, Some(id)),
        CommandEffect::Read => (ListEffectDto::Read, None),
    };
    Ok(Json(ListCommandResponse {
        list: to_list_dto(&list),
        effect,
        item_id,
    }))
}

/// `POST /api/v1/lists/{id}/promote` — the list becomes a versioned markdown
/// artifact (FR-08, ADR-024). Re-promoting adds a version to the same document.
#[tracing::instrument(skip_all, fields(list.id = %id))]
pub async fn promote(
    State(api): State<ListApi>,
    Path(id): Path<String>,
    Extension(device): Extension<DeviceContext>,
) -> Result<Json<PromoteListResponse>, Response> {
    let cancel = CancellationToken::new();
    let id = parse_list_id(&id).map_err(fault_response)?;
    let artifact_id: ArtifactId = crate::auth::fresh_id();
    // A promotion the owner asked for directly is its own occasion; the run id
    // correlates the artifact's provenance to this request.
    let run_id: RunId = crate::auth::fresh_id();
    let promoted = api
        .service
        .promote(&id, artifact_id, run_id, &actor(&device), &cancel)
        .await
        .map_err(service_problem)?;
    Ok(Json(PromoteListResponse {
        artifact_id: promoted.artifact_id,
        version: promoted.version,
        sha256: promoted.sha256_hex,
        first_promotion: promoted.first_promotion,
    }))
}

/// Which path segment failed to parse as a ULID. A unit-sized enum rather than
/// a prebuilt `Response`, for the same reason as `TimerFault` in
/// [`crate::timers`]: an axum `Response` is large, and returning one in every
/// helper's `Err` makes those results enormous (clippy `result_large_err`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdFault {
    List,
    Item,
}

fn fault_response(fault: IdFault) -> Response {
    bad_request(match fault {
        IdFault::List => "list id is not a ULID",
        IdFault::Item => "list item id is not a ULID",
    })
}

fn parse_list_id(raw: &str) -> Result<ListId, IdFault> {
    raw.parse().map_err(|_| IdFault::List)
}

fn parse_item_id(raw: &str) -> Result<ListItemId, IdFault> {
    raw.parse().map_err(|_| IdFault::Item)
}

fn bad_request(detail: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        ErrorCode::ValidationFailed,
        detail,
        None,
    )
}

fn service_problem(error: ListsError) -> Response {
    match error {
        ListsError::UnknownList(_) => problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "no such list",
            None,
        ),
        ListsError::UnknownItem => problem(
            StatusCode::NOT_FOUND,
            ErrorCode::ResourceNotFound,
            "that item is not on the list",
            None,
        ),
        // The list is at its bound: well-formed, so not a 400, and retrying it
        // unchanged will not help — remove something, or promote the list.
        ListsError::Invalid(jarvis_domain::lists::ListError::Full) => problem(
            StatusCode::CONFLICT,
            ErrorCode::ListFull,
            "that list is full; check something off, or promote it to a document",
            None,
        ),
        ListsError::Invalid(e) => bad_request(&e.to_string()),
        ListsError::Storage(e) => {
            tracing::error!(error = %e, "list storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                "storage unavailable",
                None,
            )
        }
        ListsError::Cancelled => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderUnavailable,
            "the request was cancelled",
            None,
        ),
    }
}
