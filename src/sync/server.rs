use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use super::config::{load_config, ServerConfig};
use super::projection::{DedupeLookup, WorkspaceProjection};
use super::protocol::{
    ErrorBody, ErrorCode, EventEnvelope, EventsResponse, HealthResponse, MutationRequest,
    MutationResponse, MutationResult, SequencedEvent, SnapshotResponse, SyncEventType,
    ENVELOPE_VERSION, EVENT_SCHEMA_VERSION,
};
use super::s2log::{LogError, S2Log};
use super::validation::{mutation_content_hash, unix_millis, validate_mutation, ValidationKind};

const FAILURE_HEADER: &str = "x-tsk-sync-failure";
const MAX_APPEND_ATTEMPTS: usize = 10;

#[derive(Clone)]
struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    log: S2Log,
    identities: HashMap<[u8; 32], Identity>,
    workspaces: HashMap<String, Arc<WorkspaceRuntime>>,
    failure: FailureControl,
}

#[derive(Clone)]
struct Identity {
    actor_id: String,
    workspaces: BTreeSet<String>,
}

struct WorkspaceRuntime {
    projection: Mutex<WorkspaceProjection>,
    notify: Notify,
}

#[derive(Clone, Copy)]
enum FailureMode {
    LostResponseAfterAppend,
    ErrorBeforeProjectionUpdate,
    CrashBeforeProjectionUpdate,
}

struct FailureControl {
    enabled: bool,
    default: Option<FailureMode>,
}

pub async fn run(config_path: &Path) -> Result<(), String> {
    let config = load_config(config_path)?;
    let bind = config.bind;
    let state = build_state(config).await?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| format!("cannot bind {bind}: {error}"))?;
    eprintln!("tsk-sync listening on {bind}");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| format!("HTTP server failed: {error}"))
}

async fn build_state(config: ServerConfig) -> Result<AppState, String> {
    let log = S2Log::new(&config.s2)?;
    let mut identities = HashMap::new();
    let mut workspace_names = BTreeSet::new();
    for token in config.tokens {
        workspace_names.extend(token.workspaces.iter().cloned());
        identities.insert(
            bearer_digest(token.bearer.as_bytes()),
            Identity {
                actor_id: token.actor_id,
                workspaces: token.workspaces,
            },
        );
    }

    let mut workspaces = HashMap::new();
    for workspace in workspace_names {
        log.ensure_workspace(&workspace)
            .await
            .map_err(|error| format!("cannot initialize workspace {workspace}: {error}"))?;
        let events = log
            .read_from(&workspace, 0)
            .await
            .map_err(|error| format!("cannot replay workspace {workspace}: {error}"))?;
        let mut projection = WorkspaceProjection::new(&workspace);
        projection
            .apply_all(events)
            .map_err(|error| format!("cannot replay workspace {workspace}: {error}"))?;
        workspaces.insert(
            workspace,
            Arc::new(WorkspaceRuntime {
                projection: Mutex::new(projection),
                notify: Notify::new(),
            }),
        );
    }

    Ok(AppState {
        inner: Arc::new(AppStateInner {
            log,
            identities,
            workspaces,
            failure: FailureControl::from_env()?,
        }),
    })
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/workspaces/{workspace}/snapshot", get(snapshot))
        .route("/v1/workspaces/{workspace}/mutations", post(mutate))
        .route("/v1/workspaces/{workspace}/events", get(events))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn snapshot(
    State(state): State<AppState>,
    AxumPath(workspace): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<SnapshotResponse>, ApiProblem> {
    let (_, runtime) = authorize(&state, &headers, &workspace)?;
    let mut projection = runtime.projection.lock().await;
    catch_up(&state.inner.log, &workspace, &mut projection).await?;
    Ok(Json(SnapshotResponse {
        workspace,
        next_sequence: projection.next_sequence(),
        state: projection.state().clone(),
    }))
}

async fn mutate(
    State(state): State<AppState>,
    AxumPath(workspace): AxumPath<String>,
    headers: HeaderMap,
    body: Result<Json<MutationRequest>, JsonRejection>,
) -> Result<Json<MutationResponse>, ApiProblem> {
    let (identity, runtime) = authorize(&state, &headers, &workspace)?;
    let Json(request) = body.map_err(|error| ApiProblem::invalid(error.body_text()))?;
    let content_hash = mutation_content_hash(&workspace, &identity.actor_id, &request)
        .map_err(|error| ApiProblem::invalid(error.message))?;
    let failure_mode = state.inner.failure.for_request(&headers)?;
    let mut projection = runtime.projection.lock().await;

    for attempt in 0..MAX_APPEND_ATTEMPTS {
        catch_up(&state.inner.log, &workspace, &mut projection).await?;
        match projection.lookup(request.operation_id, &identity.actor_id, &content_hash) {
            DedupeLookup::Match(response) => return Ok(Json(*response)),
            DedupeLookup::Reuse => {
                return Err(ApiProblem::operation_reuse(format!(
                    "operation {} was already used with different content or actor",
                    request.operation_id
                )));
            }
            DedupeLookup::Missing => {}
        }

        let validated = validate_mutation(projection.state(), &request).map_err(|error| {
            if error.kind == ValidationKind::Conflict {
                ApiProblem::conflict(error.message)
            } else {
                ApiProblem::invalid(error.message)
            }
        })?;
        let sequence = projection.next_sequence();
        let next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| ApiProblem::unknown("stream sequence exhausted"))?;
        let result = MutationResult {
            sequence,
            next_sequence,
            assigned_numbers: validated.assigned_numbers,
        };
        let event = EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            workspace: workspace.clone(),
            operation_id: request.operation_id,
            actor_id: identity.actor_id.clone(),
            event_type: SyncEventType::StateMutation,
            schema_version: EVENT_SCHEMA_VERSION,
            changes: validated.changes,
            content_hash: content_hash.clone(),
            state: validated.state,
            result,
            timestamp_unix_ms: unix_millis(SystemTime::now())
                .map_err(|error| ApiProblem::unknown(error.message))?,
        };

        match state.inner.log.append(&workspace, sequence, &event).await {
            Ok(appended_next) => {
                if appended_next != next_sequence {
                    return Err(ApiProblem::unknown(
                        "S2 returned an inconsistent append position",
                    ));
                }
                match failure_mode {
                    Some(FailureMode::ErrorBeforeProjectionUpdate) => {
                        return Err(ApiProblem::unknown(
                            "injected error after append and before projection update",
                        ));
                    }
                    Some(FailureMode::CrashBeforeProjectionUpdate) => {
                        std::process::abort();
                    }
                    Some(FailureMode::LostResponseAfterAppend) | None => {}
                }

                projection
                    .apply(SequencedEvent {
                        sequence,
                        event: event.clone(),
                    })
                    .map_err(|error| ApiProblem::unknown(error.to_string()))?;
                runtime.notify.notify_waiters();
                let response = MutationResponse {
                    workspace: workspace.clone(),
                    operation_id: event.operation_id,
                    content_hash: event.content_hash,
                    result: event.result,
                    state: event.state,
                };
                if matches!(failure_mode, Some(FailureMode::LostResponseAfterAppend)) {
                    return Err(ApiProblem::unknown(
                        "injected lost response after durable append",
                    ));
                }
                return Ok(Json(response));
            }
            Err(LogError::TailMismatch(_)) => continue,
            Err(LogError::Ambiguous(_)) => {
                catch_up(&state.inner.log, &workspace, &mut projection).await?;
                match projection.lookup(request.operation_id, &identity.actor_id, &content_hash) {
                    DedupeLookup::Match(response) => return Ok(Json(*response)),
                    DedupeLookup::Reuse => {
                        return Err(ApiProblem::operation_reuse(format!(
                            "operation {} was accepted with different content or actor",
                            request.operation_id
                        )));
                    }
                    DedupeLookup::Missing if attempt + 1 < MAX_APPEND_ATTEMPTS => continue,
                    DedupeLookup::Missing => {
                        return Err(ApiProblem::unknown(
                            "append outcome remained ambiguous after reconciliation",
                        ));
                    }
                }
            }
            Err(LogError::Invalid(message)) => return Err(ApiProblem::invalid(message)),
            Err(LogError::Definite(message)) => return Err(ApiProblem::unknown(message)),
            Err(error) => return Err(ApiProblem::unknown(error.to_string())),
        }
    }
    Err(ApiProblem::unknown(
        "mutation could not be serialized after repeated tail changes",
    ))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    from: u64,
    #[serde(default = "default_event_limit")]
    limit: usize,
    #[serde(default)]
    follow: bool,
    #[serde(default = "default_follow_timeout_ms")]
    timeout_ms: u64,
}

fn default_event_limit() -> usize {
    20
}

fn default_follow_timeout_ms() -> u64 {
    25_000
}

async fn events(
    State(state): State<AppState>,
    AxumPath(workspace): AxumPath<String>,
    headers: HeaderMap,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Json<EventsResponse>, ApiProblem> {
    let (_, runtime) = authorize(&state, &headers, &workspace)?;
    let Query(query) = query.map_err(|error| ApiProblem::invalid(error.body_text()))?;
    if query.limit == 0 || query.limit > 100 {
        return Err(ApiProblem::invalid("limit must be between 1 and 100"));
    }
    if query.timeout_ms > 30_000 {
        return Err(ApiProblem::invalid("timeout_ms must not exceed 30000"));
    }
    let deadline = Instant::now() + Duration::from_millis(query.timeout_ms);

    loop {
        {
            let mut projection = runtime.projection.lock().await;
            catch_up(&state.inner.log, &workspace, &mut projection).await?;
            let available = projection
                .events_from(query.from, query.limit)
                .map_err(|error| ApiProblem::conflict(error.to_string()))?;
            if !available.is_empty() || !query.follow || Instant::now() >= deadline {
                let next_sequence = available
                    .last()
                    .map_or(query.from, |event| event.sequence + 1);
                return Ok(Json(EventsResponse {
                    workspace,
                    events: available,
                    next_sequence,
                }));
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Json(EventsResponse {
                workspace,
                events: Vec::new(),
                next_sequence: query.from,
            }));
        }
        let poll = remaining.min(Duration::from_secs(1));
        tokio::select! {
            _ = runtime.notify.notified() => {}
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

async fn catch_up(
    log: &S2Log,
    workspace: &str,
    projection: &mut WorkspaceProjection,
) -> Result<(), ApiProblem> {
    let events = log
        .read_from(workspace, projection.next_sequence())
        .await
        .map_err(|error| ApiProblem::unknown(error.to_string()))?;
    projection
        .apply_all(events)
        .map_err(|error| ApiProblem::unknown(error.to_string()))
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    workspace: &str,
) -> Result<(Identity, Arc<WorkspaceRuntime>), ApiProblem> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiProblem::unauthorized("missing bearer token"))?;
    let bearer = authorization
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiProblem::unauthorized("invalid bearer token"))?;
    let digest = bearer_digest(bearer.as_bytes());
    let identity = state
        .inner
        .identities
        .get(&digest)
        .cloned()
        .ok_or_else(|| ApiProblem::unauthorized("invalid bearer token"))?;
    if !identity.workspaces.contains(workspace) {
        return Err(ApiProblem::forbidden(format!(
            "actor {} is not a member of workspace {workspace}",
            identity.actor_id
        )));
    }
    let runtime = state
        .inner
        .workspaces
        .get(workspace)
        .cloned()
        .ok_or_else(|| ApiProblem::forbidden("workspace is not configured"))?;
    Ok((identity, runtime))
}

fn bearer_digest(bearer: &[u8]) -> [u8; 32] {
    Sha256::digest(bearer).into()
}

impl FailureControl {
    fn from_env() -> Result<Self, String> {
        let default = std::env::var("TSK_SYNC_FAILURE")
            .ok()
            .map(|value| parse_failure_mode(&value))
            .transpose()?;
        let enabled = default.is_some()
            || std::env::var("TSK_SYNC_ENABLE_FAILURE_INJECTION")
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        Ok(Self { enabled, default })
    }

    fn for_request(&self, headers: &HeaderMap) -> Result<Option<FailureMode>, ApiProblem> {
        let Some(value) = headers.get(FAILURE_HEADER) else {
            return Ok(self.default);
        };
        if !self.enabled {
            return Err(ApiProblem::forbidden(
                "failure injection is disabled on this server",
            ));
        }
        let value = value
            .to_str()
            .map_err(|_| ApiProblem::invalid("invalid failure injection header"))?;
        parse_failure_mode(value)
            .map(Some)
            .map_err(ApiProblem::invalid)
    }
}

fn parse_failure_mode(value: &str) -> Result<FailureMode, String> {
    match value {
        "lost_response_after_append" => Ok(FailureMode::LostResponseAfterAppend),
        "error_before_projection_update" => Ok(FailureMode::ErrorBeforeProjectionUpdate),
        "crash_before_projection_update" => Ok(FailureMode::CrashBeforeProjectionUpdate),
        _ => Err(format!(
            "unknown failure mode {value}; expected lost_response_after_append, error_before_projection_update, or crash_before_projection_update"
        )),
    }
}

struct ApiProblem {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiProblem {
    fn new(
        status: StatusCode,
        code: ErrorCode,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                code,
                message: message.into(),
                retryable,
            },
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            false,
            message,
        )
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::Forbidden, false, message)
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            false,
            message,
        )
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, false, message)
    }

    fn operation_reuse(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            ErrorCode::OperationReuse,
            false,
            message,
        )
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unknown,
            true,
            message,
        )
    }
}

impl IntoResponse for ApiProblem {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}
