//! Blocking authenticated client for the shared sync backend.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, Response};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{DomainState, Task, STORE_FORMAT_VERSION};
use crate::fsperm;
use crate::scope::paths_equivalent;

use super::protocol::{
    ErrorBody, ErrorCode, EventsResponse, ExpectedTask, ExpectedTaskVersion, MutationRequest,
    MutationResponse, ProposedTaskChange, SnapshotResponse, TaskChangeKind,
};

const DEFAULT_CONFIG_FILE: &str = "sync.json";
const CACHE_FILE: &str = "sync-cache.json";
const PENDING_FILE: &str = "sync-pending.json";
const STALE_FILE: &str = "sync-stale.json";
const ERROR_FILE: &str = "sync-error.json";
const LOCK_FILE: &str = "tsk.json.lock";
const CLIENT_DOCUMENT_VERSION: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteErrorKind {
    Config,
    Transport,
    Authentication,
    Protocol,
    Conflict,
    UnknownOutcome,
    Unsupported,
    Cache,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteError {
    pub(crate) kind: RemoteErrorKind,
    pub(crate) message: String,
}

impl RemoteError {
    fn new(kind: RemoteErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorKind::Config, message)
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorKind::Protocol, message)
    }

    fn cache(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorKind::Cache, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(RemoteErrorKind::Unsupported, message)
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientConfigDocument {
    endpoint: String,
    workspace: String,
    bearer: String,
    projects: Vec<ProjectMappingDocument>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectMappingDocument {
    id: Uuid,
    name: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ClientConfig {
    endpoint: Url,
    workspace: String,
    bearer: String,
    projects: Vec<ProjectMapping>,
    remote_identity: String,
}

#[derive(Debug, Clone)]
struct ProjectMapping {
    local: String,
    shared: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheDocument {
    version: u32,
    remote_identity: String,
    workspace: String,
    next_sequence: u64,
    fetched_unix_ms: u64,
    state: DomainState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingDocument {
    version: u32,
    remote_identity: String,
    workspace: String,
    created_unix_ms: u64,
    request: MutationRequest,
}

#[derive(Debug, Clone, Serialize)]
struct StaleMarker<'a> {
    workspace: &'a str,
    cache_fetched_unix_ms: u64,
    stale_since_unix_ms: u64,
    cache_age_seconds: u64,
    error: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorMarker<'a> {
    workspace: &'a str,
    observed_unix_ms: u64,
    error: &'a str,
}

#[derive(Clone)]
pub(crate) struct RemoteClient {
    state_dir: PathBuf,
    config: ClientConfig,
    http: Client,
}

pub(crate) fn selected_config_path(state_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = env::var_os("TSK_SYNC_CONFIG") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let default = state_dir.join(DEFAULT_CONFIG_FILE);
    default.exists().then_some(default)
}

impl RemoteClient {
    pub(crate) fn open(state_dir: &Path, config_path: &Path) -> Result<Self, RemoteError> {
        let config = load_client_config(config_path)?;
        fsperm::ensure_private_dir(state_dir).map_err(|error| {
            RemoteError::cache(format!("sync cache directory is unavailable: {error}"))
        })?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|error| RemoteError::config(format!("cannot build sync client: {error}")))?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            config,
            http,
        })
    }

    pub(crate) fn load(&self) -> Result<DomainState, RemoteError> {
        match self.fetch_snapshot() {
            Ok(snapshot) => {
                self.store_snapshot(snapshot)?;
                self.load_cached_local()
            }
            Err(error) if error.kind == RemoteErrorKind::Transport => {
                let cache = self.read_cache()?;
                self.mark_stale(&cache, &error.message)?;
                let age = now_unix_ms().saturating_sub(cache.fetched_unix_ms) / 1_000;
                eprintln!(
                    "tsk sync: offline; using cached workspace {} snapshot ({}s old): {}",
                    self.config.workspace, age, error.message
                );
                self.local_state_from_cache(cache)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn fetch_live_local(&self) -> Result<DomainState, RemoteError> {
        let snapshot = self.fetch_snapshot().map_err(|error| {
            if error.kind == RemoteErrorKind::Transport {
                RemoteError::new(
                    RemoteErrorKind::Transport,
                    format!(
                        "sync unavailable: a live workspace snapshot is required for writes: {}",
                        error.message
                    ),
                )
            } else {
                error
            }
        })?;
        self.store_snapshot(snapshot)?;
        self.load_cached_local()
    }

    pub(crate) fn reconcile_pending(&self) -> Result<(), RemoteError> {
        let Some(pending) = self.read_pending()? else {
            return Ok(());
        };
        if pending.workspace != self.config.workspace
            || pending.remote_identity != self.config.remote_identity
        {
            return Err(RemoteError::protocol(format!(
                "sync pending operation does not belong to configured remote workspace {}",
                self.config.workspace
            )));
        }
        self.post_pending(&pending)
    }

    pub(crate) fn submit(
        &self,
        base: &DomainState,
        proposed: &DomainState,
    ) -> Result<Option<DomainState>, RemoteError> {
        let Some(request) = build_mutation_request(base, proposed, &self.config.projects)? else {
            return Ok(None);
        };
        let pending = PendingDocument {
            version: CLIENT_DOCUMENT_VERSION,
            remote_identity: self.config.remote_identity.clone(),
            workspace: self.config.workspace.clone(),
            created_unix_ms: now_unix_ms(),
            request,
        };
        self.write_pending(&pending)?;
        self.post_pending(&pending)?;
        self.load_cached_local().map(Some)
    }

    fn post_pending(&self, pending: &PendingDocument) -> Result<(), RemoteError> {
        let operation_id = pending.request.operation_id;
        let url = self.endpoint(&format!(
            "v1/workspaces/{}/mutations",
            self.config.workspace
        ))?;
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.config.bearer)
            .json(&pending.request)
            .send()
            .map_err(|error| {
                RemoteError::new(
                    RemoteErrorKind::UnknownOutcome,
                    format!(
                        "sync outcome unknown for operation {operation_id}; it remains pending for idempotent retry: {error}"
                    ),
                )
            })?;
        let response = match decode_mutation_response(response, operation_id) {
            Ok(response) => response,
            Err(error) => {
                if matches!(
                    error.kind,
                    RemoteErrorKind::Conflict | RemoteErrorKind::Protocol
                ) {
                    let _ = remove_if_exists(&self.pending_path());
                }
                return Err(error);
            }
        };
        self.validate_mutation_response(&response, &pending.request)?;
        self.write_cache(CacheDocument {
            version: CLIENT_DOCUMENT_VERSION,
            remote_identity: self.config.remote_identity.clone(),
            workspace: response.workspace,
            next_sequence: response.result.next_sequence,
            fetched_unix_ms: now_unix_ms(),
            state: response.state,
        })?;
        remove_if_exists(&self.pending_path()).map_err(|error| {
            RemoteError::cache(format!(
                "sync operation {operation_id} was accepted but its pending record could not be cleared: {error}"
            ))
        })?;
        Ok(())
    }

    fn fetch_snapshot(&self) -> Result<SnapshotResponse, RemoteError> {
        let url = self.endpoint(&format!("v1/workspaces/{}/snapshot", self.config.workspace))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.config.bearer)
            .send()
            .map_err(|error| {
                RemoteError::new(
                    RemoteErrorKind::Transport,
                    format!("cannot reach sync service: {error}"),
                )
            })?;
        let response = decode_snapshot_response(response)?;
        if response.workspace != self.config.workspace {
            return Err(RemoteError::protocol(format!(
                "sync snapshot named workspace {}, expected {}",
                response.workspace, self.config.workspace
            )));
        }
        check_wire_state(&response.state)?;
        let _ = inbound_state(response.state.clone(), &self.config.projects)?;
        Ok(response)
    }

    fn fetch_poll_document(&self) -> Result<CacheDocument, RemoteError> {
        let cache = match self.read_cache() {
            Ok(cache) => cache,
            Err(_) => {
                let snapshot = self.fetch_snapshot()?;
                return Ok(CacheDocument {
                    version: CLIENT_DOCUMENT_VERSION,
                    remote_identity: self.config.remote_identity.clone(),
                    workspace: snapshot.workspace,
                    next_sequence: snapshot.next_sequence,
                    fetched_unix_ms: now_unix_ms(),
                    state: snapshot.state,
                });
            }
        };
        let mut url = self.endpoint(&format!("v1/workspaces/{}/events", self.config.workspace))?;
        url.query_pairs_mut()
            .append_pair("from", &cache.next_sequence.to_string())
            .append_pair("limit", "100")
            .append_pair("follow", "false");
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.config.bearer)
            .send()
            .map_err(|error| {
                RemoteError::new(
                    RemoteErrorKind::Transport,
                    format!("cannot reach sync service: {error}"),
                )
            })?;
        let events = decode_events_response(response)?;
        if events.workspace != self.config.workspace {
            return Err(RemoteError::protocol(format!(
                "sync events named workspace {}, expected {}",
                events.workspace, self.config.workspace
            )));
        }
        let mut expected = cache.next_sequence;
        let mut state = cache.state;
        for sequenced in events.events {
            if sequenced.sequence != expected
                || sequenced.event.workspace != self.config.workspace
                || sequenced.event.result.sequence != sequenced.sequence
                || sequenced.event.result.next_sequence != sequenced.sequence.saturating_add(1)
            {
                return Err(RemoteError::protocol(
                    "sync event response has inconsistent cursor metadata",
                ));
            }
            check_wire_state(&sequenced.event.state)?;
            let _ = inbound_state(sequenced.event.state.clone(), &self.config.projects)?;
            state = sequenced.event.state;
            expected = expected.saturating_add(1);
        }
        if events.next_sequence != expected {
            return Err(RemoteError::protocol(
                "sync event response ended at an inconsistent cursor",
            ));
        }
        Ok(CacheDocument {
            version: CLIENT_DOCUMENT_VERSION,
            remote_identity: self.config.remote_identity.clone(),
            workspace: self.config.workspace.clone(),
            next_sequence: expected,
            fetched_unix_ms: now_unix_ms(),
            state,
        })
    }

    fn store_snapshot(&self, snapshot: SnapshotResponse) -> Result<(), RemoteError> {
        self.write_cache(CacheDocument {
            version: CLIENT_DOCUMENT_VERSION,
            remote_identity: self.config.remote_identity.clone(),
            workspace: snapshot.workspace,
            next_sequence: snapshot.next_sequence,
            fetched_unix_ms: now_unix_ms(),
            state: snapshot.state,
        })
    }

    fn validate_mutation_response(
        &self,
        response: &MutationResponse,
        request: &MutationRequest,
    ) -> Result<(), RemoteError> {
        let operation_id = request.operation_id;
        if response.workspace != self.config.workspace || response.operation_id != operation_id {
            return Err(RemoteError::new(
                RemoteErrorKind::UnknownOutcome,
                format!(
                    "sync outcome unknown for operation {operation_id}: response identity did not match; pending retry was retained"
                ),
            ));
        }
        if response.result.next_sequence != response.result.sequence.saturating_add(1) {
            return Err(RemoteError::new(
                RemoteErrorKind::UnknownOutcome,
                format!(
                    "sync outcome unknown for operation {operation_id}: response cursor was inconsistent; pending retry was retained"
                ),
            ));
        }
        check_wire_state(&response.state).map_err(|error| {
            RemoteError::new(
                RemoteErrorKind::UnknownOutcome,
                format!(
                    "sync outcome unknown for operation {operation_id}: {}; pending retry was retained",
                    error.message
                ),
            )
        })?;
        for change in &request.changes {
            let Some(task) = response.state.get(change.task_id) else {
                return Err(unknown_response(
                    operation_id,
                    format!("accepted state omitted changed task {}", change.task_id),
                ));
            };
            if task.revision != change.result_revision {
                return Err(unknown_response(
                    operation_id,
                    format!(
                        "accepted state has the wrong revision for task {}",
                        change.task_id
                    ),
                ));
            }
            let tombstone_matches = match change.kind {
                TaskChangeKind::Create | TaskChangeKind::Update | TaskChangeKind::Restore => {
                    !task.soft_deleted
                }
                TaskChangeKind::Delete => task.soft_deleted,
            };
            if !tombstone_matches {
                return Err(unknown_response(
                    operation_id,
                    format!(
                        "accepted state has the wrong deletion state for task {}",
                        change.task_id
                    ),
                ));
            }
            if change.kind == TaskChangeKind::Create
                && task.number.is_none()
                && task
                    .notice
                    .as_ref()
                    .and_then(|notice| notice.number)
                    .is_none()
            {
                return Err(unknown_response(
                    operation_id,
                    format!(
                        "accepted state did not assign a number to task {}",
                        change.task_id
                    ),
                ));
            }
        }
        let _ = inbound_state(response.state.clone(), &self.config.projects).map_err(|error| {
            unknown_response(
                operation_id,
                format!("accepted state could not be translated: {}", error.message),
            )
        })?;
        Ok(())
    }

    fn endpoint(&self, relative: &str) -> Result<Url, RemoteError> {
        self.config.endpoint.join(relative).map_err(|error| {
            RemoteError::config(format!("cannot construct sync endpoint URL: {error}"))
        })
    }

    pub(crate) fn load_cached_local(&self) -> Result<DomainState, RemoteError> {
        let cache = self.read_cache()?;
        self.local_state_from_cache(cache)
    }

    fn local_state_from_cache(&self, cache: CacheDocument) -> Result<DomainState, RemoteError> {
        if cache.workspace != self.config.workspace {
            return Err(RemoteError::cache(format!(
                "sync cache belongs to workspace {}, not {}",
                cache.workspace, self.config.workspace
            )));
        }
        inbound_state(cache.state, &self.config.projects)
    }

    fn read_cache(&self) -> Result<CacheDocument, RemoteError> {
        let data = fs::read(self.cache_path()).map_err(|error| {
            RemoteError::cache(if error.kind() == io::ErrorKind::NotFound {
                "sync service is offline and no cached snapshot is available".to_string()
            } else {
                format!("cannot read sync cache: {error}")
            })
        })?;
        let cache: CacheDocument = serde_json::from_slice(&data)
            .map_err(|error| RemoteError::cache(format!("invalid sync cache: {error}")))?;
        if cache.version != CLIENT_DOCUMENT_VERSION {
            return Err(RemoteError::cache(format!(
                "unsupported sync cache version {}",
                cache.version
            )));
        }
        if cache.remote_identity != self.config.remote_identity {
            return Err(RemoteError::cache(
                "sync cache belongs to a different configured remote workspace",
            ));
        }
        check_wire_state(&cache.state)?;
        Ok(cache)
    }

    fn write_cache(&self, cache: CacheDocument) -> Result<(), RemoteError> {
        if cache.workspace != self.config.workspace
            || cache.remote_identity != self.config.remote_identity
        {
            return Err(RemoteError::protocol(
                "refusing to cache a different configured remote workspace",
            ));
        }
        if let Ok(existing) = self.read_cache() {
            if existing.next_sequence > cache.next_sequence {
                return Ok(());
            }
            if existing.next_sequence == cache.next_sequence
                && serde_json::to_value(&existing.state).ok()
                    != serde_json::to_value(&cache.state).ok()
            {
                return Err(RemoteError::protocol(
                    "sync service returned different states for the same cursor",
                ));
            }
        }
        write_json_atomic(&self.cache_path(), &cache)
            .map_err(|error| RemoteError::cache(format!("cannot update sync cache: {error}")))?;
        remove_if_exists(&self.stale_path()).map_err(|error| {
            RemoteError::cache(format!("cannot clear stale sync marker: {error}"))
        })?;
        remove_if_exists(&self.error_path())
            .map_err(|error| RemoteError::cache(format!("cannot clear sync error marker: {error}")))
    }

    fn read_pending(&self) -> Result<Option<PendingDocument>, RemoteError> {
        let data = match fs::read(self.pending_path()) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(RemoteError::cache(format!(
                    "cannot read pending sync operation: {error}"
                )))
            }
        };
        let pending: PendingDocument = serde_json::from_slice(&data).map_err(|error| {
            RemoteError::cache(format!("invalid pending sync operation: {error}"))
        })?;
        if pending.version != CLIENT_DOCUMENT_VERSION {
            return Err(RemoteError::cache(format!(
                "unsupported pending sync version {}",
                pending.version
            )));
        }
        if pending.remote_identity != self.config.remote_identity {
            return Err(RemoteError::cache(
                "pending sync operation belongs to a different configured remote workspace",
            ));
        }
        Ok(Some(pending))
    }

    fn write_pending(&self, pending: &PendingDocument) -> Result<(), RemoteError> {
        write_json_atomic(&self.pending_path(), pending).map_err(|error| {
            RemoteError::cache(format!("cannot persist pending sync operation: {error}"))
        })
    }

    fn mark_stale(&self, cache: &CacheDocument, message: &str) -> Result<(), RemoteError> {
        let now = now_unix_ms();
        let stale_since = fs::read(self.stale_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["stale_since_unix_ms"].as_u64())
            .unwrap_or(now);
        write_json_atomic(
            &self.stale_path(),
            &StaleMarker {
                workspace: &self.config.workspace,
                cache_fetched_unix_ms: cache.fetched_unix_ms,
                stale_since_unix_ms: stale_since,
                cache_age_seconds: now.saturating_sub(cache.fetched_unix_ms) / 1_000,
                error: message,
            },
        )
        .map_err(|error| RemoteError::cache(format!("cannot mark sync cache stale: {error}")))
    }

    fn mark_nontransport_error(&self, message: &str) {
        let _ = write_json_atomic(
            &self.error_path(),
            &ErrorMarker {
                workspace: &self.config.workspace,
                observed_unix_ms: now_unix_ms(),
                error: message,
            },
        );
    }

    fn cache_path(&self) -> PathBuf {
        self.state_dir.join(CACHE_FILE)
    }

    fn pending_path(&self) -> PathBuf {
        self.state_dir.join(PENDING_FILE)
    }

    fn stale_path(&self) -> PathBuf {
        self.state_dir.join(STALE_FILE)
    }

    fn error_path(&self) -> PathBuf {
        self.state_dir.join(ERROR_FILE)
    }
}

fn load_client_config(path: &Path) -> Result<ClientConfig, RemoteError> {
    let mut file = open_private_config(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| RemoteError::config(format!("cannot read sync config: {error}")))?;
    let document: ClientConfigDocument = serde_json::from_str(&contents)
        .map_err(|error| RemoteError::config(format!("invalid sync config JSON: {error}")))?;
    validate_client_config(document)
}

fn validate_client_config(document: ClientConfigDocument) -> Result<ClientConfig, RemoteError> {
    let mut endpoint = Url::parse(&document.endpoint)
        .map_err(|error| RemoteError::config(format!("invalid sync endpoint: {error}")))?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.cannot_be_a_base()
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(RemoteError::config(
            "sync endpoint must be an http(s) base URL without credentials, query, or fragment",
        ));
    }
    let loopback_http = endpoint.scheme() == "http"
        && endpoint.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if endpoint.scheme() != "https" && !loopback_http {
        return Err(RemoteError::config(
            "sync endpoint must use HTTPS (plain HTTP is allowed only on loopback)",
        ));
    }
    if !endpoint.path().ends_with('/') {
        endpoint.set_path(&format!("{}/", endpoint.path()));
    }
    validate_identifier("workspace", &document.workspace)?;
    if document.bearer.is_empty()
        || document
            .bearer
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(RemoteError::config(
            "sync bearer must be non-empty and contain no whitespace",
        ));
    }

    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut projects = Vec::with_capacity(document.projects.len());
    for project in document.projects {
        if project.id.is_nil() || !ids.insert(project.id) {
            return Err(RemoteError::config(
                "sync project IDs must be unique and non-nil",
            ));
        }
        validate_identifier("project name", &project.name)?;
        if !names.insert(project.name.clone()) {
            return Err(RemoteError::config(
                "sync project names must be unique in one client config",
            ));
        }
        if !project.path.is_absolute() {
            return Err(RemoteError::config(
                "sync project paths must be absolute local paths",
            ));
        }
        let local = project
            .path
            .to_str()
            .ok_or_else(|| RemoteError::config("sync project paths must contain valid UTF-8"))?
            .to_string();
        if !paths.insert(local.clone()) {
            return Err(RemoteError::config(
                "sync project paths must be unique in one client config",
            ));
        }
        let shared = format!("shared://{}/{}", project.id, project.name);
        projects.push(ProjectMapping { local, shared });
    }
    for (index, left) in projects.iter().enumerate() {
        if projects[index + 1..]
            .iter()
            .any(|right| paths_equivalent(&left.local, &right.local))
        {
            return Err(RemoteError::config(
                "sync project paths must identify distinct local directories",
            ));
        }
    }
    let remote_identity = remote_identity(&endpoint, &document.workspace, &projects);
    Ok(ClientConfig {
        endpoint,
        workspace: document.workspace,
        bearer: document.bearer,
        projects,
        remote_identity,
    })
}

fn remote_identity(endpoint: &Url, workspace: &str, projects: &[ProjectMapping]) -> String {
    let mut hash = Sha256::new();
    hash.update(endpoint.as_str().as_bytes());
    hash.update([0]);
    hash.update(workspace.as_bytes());
    let mut shared = projects
        .iter()
        .map(|project| project.shared.as_str())
        .collect::<Vec<_>>();
    shared.sort_unstable();
    for project in shared {
        hash.update([0]);
        hash.update(project.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_identifier(label: &str, value: &str) -> Result<(), RemoteError> {
    if matches!(value, "." | "..")
        || value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RemoteError::config(format!(
            "{label} must be 1..=128 ASCII letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

fn open_private_config(path: &Path) -> Result<File, RemoteError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let path_metadata = fs::symlink_metadata(path)
            .map_err(|error| RemoteError::config(format!("cannot inspect sync config: {error}")))?;
        if !path_metadata.file_type().is_file() {
            return Err(RemoteError::config(
                "sync config must be a regular file, not a symlink",
            ));
        }
        if path_metadata.permissions().mode() & 0o7777 != 0o600 {
            return Err(RemoteError::config(
                "sync config permissions must be exactly 0600",
            ));
        }
        let file = File::open(path)
            .map_err(|error| RemoteError::config(format!("cannot open sync config: {error}")))?;
        let opened = file.metadata().map_err(|error| {
            RemoteError::config(format!("cannot inspect opened sync config: {error}"))
        })?;
        if opened.dev() != path_metadata.dev() || opened.ino() != path_metadata.ino() {
            return Err(RemoteError::config(
                "sync config changed while it was being opened",
            ));
        }
        if opened.permissions().mode() & 0o7777 != 0o600 {
            return Err(RemoteError::config(
                "opened sync config permissions must be exactly 0600",
            ));
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| RemoteError::config(format!("cannot inspect sync config: {error}")))?;
        if !metadata.file_type().is_file() {
            return Err(RemoteError::config("sync config must be a regular file"));
        }
        File::open(path)
            .map_err(|error| RemoteError::config(format!("cannot open sync config: {error}")))
    }
}

fn check_wire_state(state: &DomainState) -> Result<(), RemoteError> {
    if state.format_version() != STORE_FORMAT_VERSION {
        return Err(RemoteError::protocol(format!(
            "sync state format {} is unsupported (expected {})",
            state.format_version(),
            STORE_FORMAT_VERSION
        )));
    }
    Ok(())
}

fn inbound_state(
    state: DomainState,
    projects: &[ProjectMapping],
) -> Result<DomainState, RemoteError> {
    translate_state_paths(state, projects, Translation::Inbound, &BTreeSet::new())
}

fn outbound_state(
    mut state: DomainState,
    projects: &[ProjectMapping],
    created: &BTreeSet<Uuid>,
) -> Result<DomainState, RemoteError> {
    state.clear_client_only_sync_state();
    translate_state_paths(state, projects, Translation::Outbound, created)
}

#[derive(Clone, Copy)]
enum Translation {
    Inbound,
    Outbound,
}

fn translate_state_paths(
    state: DomainState,
    projects: &[ProjectMapping],
    direction: Translation,
    created: &BTreeSet<Uuid>,
) -> Result<DomainState, RemoteError> {
    let mut document = serde_json::to_value(state)
        .map_err(|error| RemoteError::protocol(format!("cannot inspect sync state: {error}")))?;
    let tasks = document["tasks"]
        .as_array_mut()
        .ok_or_else(|| RemoteError::protocol("sync state tasks are not an array"))?;
    for task in tasks {
        let task_id = task["id"]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| RemoteError::protocol("sync task has an invalid ID"))?;
        if let Some(path) = task["scope"]["project"]["path"].as_str() {
            let translated = translate_path(path, projects, direction)?;
            task["scope"]["project"]["path"] = serde_json::Value::String(translated);
        }
        if created.contains(&task_id) {
            if let Some(object) = task.as_object_mut() {
                object.remove("number");
                if let Some(notice) = object
                    .get_mut("notice")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    notice.remove("number");
                }
            }
        }
    }

    let old_projects = document["projects"]
        .as_object_mut()
        .ok_or_else(|| RemoteError::protocol("sync project records are not an object"))?
        .clone();
    let mut translated_projects = serde_json::Map::new();
    for (path, record) in old_projects {
        let translated = translate_path(&path, projects, direction)?;
        if translated_projects.insert(translated, record).is_some() {
            return Err(RemoteError::protocol(
                "two sync project records translated to the same project",
            ));
        }
    }
    document["projects"] = serde_json::Value::Object(translated_projects);
    let mut translated: DomainState = serde_json::from_value(document).map_err(|error| {
        RemoteError::protocol(format!("invalid translated sync state: {error}"))
    })?;
    if matches!(direction, Translation::Inbound) {
        translated.mark_shared();
    }
    Ok(translated)
}

fn translate_path(
    path: &str,
    projects: &[ProjectMapping],
    direction: Translation,
) -> Result<String, RemoteError> {
    match direction {
        Translation::Inbound => {
            let mapping = projects.iter().find(|mapping| mapping.shared == path);
            mapping.map(|mapping| mapping.local.clone()).ok_or_else(|| {
                RemoteError::protocol(format!(
                    "shared project identity {path} has no local mapping"
                ))
            })
        }
        Translation::Outbound => projects
            .iter()
            .find(|mapping| mapping.local == path || paths_equivalent(&mapping.local, path))
            .map(|mapping| mapping.shared.clone())
            .ok_or_else(|| {
                RemoteError::config(format!(
                    "local project path {path} has no sync project mapping"
                ))
            }),
    }
}

fn build_mutation_request(
    base: &DomainState,
    proposed: &DomainState,
    projects: &[ProjectMapping],
) -> Result<Option<MutationRequest>, RemoteError> {
    if base.projects() != proposed.projects() {
        return Err(RemoteError::unsupported(
            "shared project archive/unarchive is not supported by this sync protocol",
        ));
    }
    let before = base
        .tasks()
        .iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    let after = proposed
        .tasks()
        .iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    for task_id in before.keys() {
        if !after.contains_key(task_id) {
            return Err(RemoteError::unsupported(
                "hard task removal is not supported on a shared board",
            ));
        }
    }

    let changed = after
        .iter()
        .filter_map(|(id, task)| (before.get(id).copied() != Some(*task)).then_some(*id))
        .collect::<BTreeSet<_>>();
    if changed.is_empty() {
        return Ok(None);
    }

    let created = changed
        .iter()
        .copied()
        .filter(|id| !before.contains_key(id))
        .collect::<BTreeSet<_>>();
    let wire_state = outbound_state(proposed.clone(), projects, &created)?;
    let mut expected = Vec::with_capacity(changed.len());
    let mut changes = Vec::with_capacity(changed.len());
    for task_id in changed {
        let result = wire_state
            .get(task_id)
            .ok_or_else(|| RemoteError::protocol("translated state lost a changed task"))?;
        match before.get(&task_id).copied() {
            None => {
                expected.push(ExpectedTask {
                    task_id,
                    version: ExpectedTaskVersion::Absent,
                });
                changes.push(ProposedTaskChange {
                    task_id,
                    kind: TaskChangeKind::Create,
                    base_revision: None,
                    result_revision: result.revision,
                });
            }
            Some(base_task) => {
                let kind = change_kind(base_task, result)?;
                expected.push(ExpectedTask {
                    task_id,
                    version: ExpectedTaskVersion::Revision {
                        revision: base_task.revision,
                        tombstone: base_task.soft_deleted,
                    },
                });
                changes.push(ProposedTaskChange {
                    task_id,
                    kind,
                    base_revision: Some(base_task.revision),
                    result_revision: result.revision,
                });
            }
        }
    }
    Ok(Some(MutationRequest {
        operation_id: Uuid::new_v4(),
        expected,
        changes,
        proposed_state: wire_state,
    }))
}

fn change_kind(base: &Task, result: &Task) -> Result<TaskChangeKind, RemoteError> {
    match (base.soft_deleted, result.soft_deleted) {
        (false, false) => Ok(TaskChangeKind::Update),
        (false, true) => Ok(TaskChangeKind::Delete),
        (true, false) => Ok(TaskChangeKind::Restore),
        (true, true) => Err(RemoteError::new(
            RemoteErrorKind::Conflict,
            format!("sync conflict: task {} is already deleted", base.id),
        )),
    }
}

fn unknown_response(operation_id: Uuid, detail: impl std::fmt::Display) -> RemoteError {
    RemoteError::new(
        RemoteErrorKind::UnknownOutcome,
        format!(
            "sync outcome unknown for operation {operation_id}: {detail}; pending retry was retained"
        ),
    )
}

fn decode_events_response(response: Response) -> Result<EventsResponse, RemoteError> {
    if response.status().is_success() {
        return response.json().map_err(|error| {
            RemoteError::protocol(format!("invalid sync events response: {error}"))
        });
    }
    Err(classify_api_error(response, false, None))
}

fn decode_snapshot_response(response: Response) -> Result<SnapshotResponse, RemoteError> {
    if response.status().is_success() {
        return response.json().map_err(|error| {
            RemoteError::protocol(format!("invalid sync snapshot response: {error}"))
        });
    }
    Err(classify_api_error(response, false, None))
}

fn decode_mutation_response(
    response: Response,
    operation_id: Uuid,
) -> Result<MutationResponse, RemoteError> {
    if response.status().is_success() {
        return response.json().map_err(|error| {
            RemoteError::new(
                RemoteErrorKind::UnknownOutcome,
                format!(
                    "sync outcome unknown for operation {operation_id}: invalid success response: {error}; pending retry was retained"
                ),
            )
        });
    }
    Err(classify_api_error(response, true, Some(operation_id)))
}

fn classify_api_error(
    response: Response,
    mutation: bool,
    operation_id: Option<Uuid>,
) -> RemoteError {
    let status = response.status();
    let body = response.json::<ErrorBody>().ok();
    classify_api_parts(status, body.as_ref(), mutation, operation_id)
}

fn classify_api_parts(
    status: StatusCode,
    body: Option<&ErrorBody>,
    mutation: bool,
    operation_id: Option<Uuid>,
) -> RemoteError {
    let detail = body
        .map(|body| body.message.as_str())
        .unwrap_or("sync service returned an unreadable error response");
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return RemoteError::new(
            RemoteErrorKind::Authentication,
            format!("sync authentication/authorization failed: {detail}"),
        );
    }
    match body.map(|body| body.code) {
        Some(ErrorCode::Unauthorized | ErrorCode::Forbidden) => RemoteError::new(
            RemoteErrorKind::Authentication,
            format!("sync authentication/authorization failed: {detail}"),
        ),
        Some(ErrorCode::Conflict) => RemoteError::new(
            RemoteErrorKind::Conflict,
            format!("sync conflict: {detail}"),
        ),
        Some(ErrorCode::InvalidRequest) => RemoteError::new(
            RemoteErrorKind::Protocol,
            format!("sync protocol rejected the request: {detail}"),
        ),
        Some(ErrorCode::OperationReuse) => RemoteError::new(
            if mutation {
                RemoteErrorKind::UnknownOutcome
            } else {
                RemoteErrorKind::Protocol
            },
            format!(
                "sync operation ID safety check failed{}: {detail}; the pending request was retained",
                operation_id.map_or(String::new(), |id| format!(" for {id}"))
            ),
        ),
        Some(ErrorCode::Unknown) if mutation => RemoteError::new(
            RemoteErrorKind::UnknownOutcome,
            format!(
                "sync outcome unknown for operation {}; it remains pending for idempotent retry: {detail}",
                operation_id.map_or_else(|| "<unknown>".to_string(), |id| id.to_string())
            ),
        ),
        Some(ErrorCode::Unknown) => RemoteError::new(
            RemoteErrorKind::Protocol,
            format!("sync service could not serve the snapshot: {detail}"),
        ),
        None if mutation => RemoteError::new(
            RemoteErrorKind::UnknownOutcome,
            format!(
                "sync outcome unknown for operation {} after HTTP {status}; pending retry was retained",
                operation_id.map_or_else(|| "<unknown>".to_string(), |id| id.to_string())
            ),
        ),
        None => RemoteError::protocol(format!("sync service returned HTTP {status}: {detail}")),
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("sync file has no parent directory"))?;
    fsperm::ensure_private_dir(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        now_unix_ms()
    ));
    let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let result = (|| {
        let mut file = fsperm::create_private_file(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn cached_signature(state_dir: &Path) -> Option<(u64, bool, bool)> {
    let data = fs::read(state_dir.join(CACHE_FILE)).ok()?;
    let cache: CacheDocument = serde_json::from_slice(&data).ok()?;
    Some((
        cache.next_sequence,
        state_dir.join(STALE_FILE).exists(),
        state_dir.join(ERROR_FILE).exists(),
    ))
}

#[derive(Clone, Copy)]
struct PollState {
    last_started: Instant,
    in_flight: bool,
}

static POLLS: OnceLock<Mutex<HashMap<PathBuf, PollState>>> = OnceLock::new();

pub(crate) fn poll_in_background(state_dir: PathBuf, config_path: PathBuf) {
    let polls = POLLS.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    {
        let mut states = polls.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = states.get(&state_dir) {
            if state.in_flight || now.duration_since(state.last_started) < POLL_INTERVAL {
                return;
            }
        }
        states.insert(
            state_dir.clone(),
            PollState {
                last_started: now,
                in_flight: true,
            },
        );
    }

    std::thread::spawn(move || {
        poll_once(&state_dir, &config_path);
        if let Some(polls) = POLLS.get() {
            let mut states = polls.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(state) = states.get_mut(&state_dir) {
                state.in_flight = false;
            }
        }
    });
}

fn poll_once(state_dir: &Path, config_path: &Path) {
    let client = match RemoteClient::open(state_dir, config_path) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("tsk sync: {}", error.message);
            return;
        }
    };
    let result = client.fetch_poll_document();
    let _lock = match CacheLock::acquire(state_dir) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("tsk sync: cannot lock cache for polling: {error}");
            return;
        }
    };
    match result {
        Ok(cache) => {
            if let Err(error) = client.write_cache(cache) {
                eprintln!("tsk sync: {}", error.message);
            }
        }
        Err(error) if error.kind == RemoteErrorKind::Transport => {
            if let Ok(cache) = client.read_cache() {
                let was_stale = client.stale_path().exists();
                let _ = client.mark_stale(&cache, &error.message);
                if !was_stale {
                    let age = now_unix_ms().saturating_sub(cache.fetched_unix_ms) / 1_000;
                    eprintln!(
                        "tsk sync: offline; cached workspace {} snapshot is {}s old: {}",
                        client.config.workspace, age, error.message
                    );
                }
            }
        }
        Err(error) => {
            let first = !client.error_path().exists();
            client.mark_nontransport_error(&error.message);
            if first {
                eprintln!("tsk sync: {}", error.message);
            }
        }
    }
}

struct CacheLock {
    file: File,
}

impl CacheLock {
    fn acquire(state_dir: &Path) -> io::Result<Self> {
        fsperm::ensure_private_dir(state_dir)?;
        let path = state_dir.join(LOCK_FILE);
        let file = fsperm::open_lock_file(&path)?;
        fsperm::tighten_file(&path);
        file.lock()?;
        Ok(Self { file })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = File::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProvenanceOrigin, TaskScope};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "tsk-sync-client-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn mapping(local: &Path) -> ProjectMapping {
        let id = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        ProjectMapping {
            local: local.to_string_lossy().into_owned(),
            shared: format!("shared://{id}/atlas"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn client_config_requires_exact_0600() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = temp_dir("permissions");
        let path = dir.join("sync.json");
        let project = dir.join("project");
        let document = serde_json::json!({
            "endpoint": "https://sync.example.test/",
            "workspace": "demo",
            "bearer": "secret",
            "projects": [{
                "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "name": "atlas",
                "path": project,
            }]
        });
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(file, "{document}").unwrap();
        drop(file);
        load_client_config(&path).expect("0600 accepted");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            load_client_config(&path).unwrap_err().kind,
            RemoteErrorKind::Config
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn project_paths_translate_without_leaking_local_paths() {
        let local = PathBuf::from("/work/alice/atlas");
        let projects = vec![mapping(&local)];
        let mut state = DomainState::new();
        state
            .create(
                "mapped",
                None,
                TaskScope::Project {
                    path: local.to_string_lossy().into_owned(),
                },
                ProvenanceOrigin::Manual,
                None,
            )
            .unwrap();
        let id = state.tasks()[0].id;
        let wire = outbound_state(state, &projects, &BTreeSet::from([id])).unwrap();
        let encoded = serde_json::to_string(&wire).unwrap();
        assert!(!encoded.contains("/work/alice"));
        assert!(encoded.contains("shared://aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/atlas"));
        let local_state = inbound_state(wire, &projects).unwrap();
        assert_eq!(
            local_state.get(id).unwrap().scope,
            TaskScope::Project {
                path: local.to_string_lossy().into_owned()
            }
        );
    }

    #[test]
    fn inbound_shared_state_disables_undo() {
        let mut state = inbound_state(DomainState::new(), &[]).unwrap();
        assert_eq!(
            state.undo(),
            Err(crate::domain::DomainError::SharedUndoUnsupported)
        );
    }

    #[test]
    fn diff_generates_revision_guards_and_strips_created_number() {
        let base = DomainState::new();
        let mut proposed = base.clone();
        let id = proposed
            .create(
                "concurrent create",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .unwrap();
        proposed.assign_numbers_for_persistence();
        let request = build_mutation_request(&base, &proposed, &[])
            .unwrap()
            .expect("one change");
        assert_eq!(request.expected[0].version, ExpectedTaskVersion::Absent);
        assert_eq!(request.changes[0].kind, TaskChangeKind::Create);
        assert_eq!(request.proposed_state.get(id).unwrap().number, None);
    }

    #[test]
    fn cache_round_trip_keeps_wire_state_and_cursor() {
        let dir = temp_dir("cache");
        let config = ClientConfig {
            endpoint: Url::parse("https://sync.example.test/").unwrap(),
            workspace: "demo".to_string(),
            bearer: "secret".to_string(),
            projects: Vec::new(),
            remote_identity: "remote-test".to_string(),
        };
        let client = RemoteClient {
            state_dir: dir.clone(),
            config,
            http: Client::new(),
        };
        client
            .write_cache(CacheDocument {
                version: CLIENT_DOCUMENT_VERSION,
                remote_identity: "remote-test".to_string(),
                workspace: "demo".to_string(),
                next_sequence: 7,
                fetched_unix_ms: 123,
                state: DomainState::new(),
            })
            .unwrap();
        let cache = client.read_cache().unwrap();
        assert_eq!(cache.next_sequence, 7);
        assert_eq!(cached_signature(&dir), Some((7, false, false)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_errors_distinguish_auth_conflict_and_unknown_outcome() {
        let body = |code, message: &str| ErrorBody {
            code,
            message: message.to_string(),
            retryable: false,
        };
        assert_eq!(
            RemoteError::new(RemoteErrorKind::Transport, "offline").kind,
            RemoteErrorKind::Transport
        );
        assert_eq!(
            classify_api_parts(
                StatusCode::UNAUTHORIZED,
                Some(&body(ErrorCode::Unauthorized, "bad token")),
                false,
                None
            )
            .kind,
            RemoteErrorKind::Authentication
        );
        assert_eq!(
            classify_api_parts(
                StatusCode::CONFLICT,
                Some(&body(ErrorCode::Conflict, "stale")),
                true,
                Some(Uuid::new_v4())
            )
            .kind,
            RemoteErrorKind::Conflict
        );
        assert_eq!(
            classify_api_parts(
                StatusCode::SERVICE_UNAVAILABLE,
                Some(&ErrorBody {
                    code: ErrorCode::Unknown,
                    message: "ambiguous".to_string(),
                    retryable: true,
                }),
                true,
                Some(Uuid::new_v4())
            )
            .kind,
            RemoteErrorKind::UnknownOutcome
        );
    }
}
