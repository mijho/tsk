use s2_sdk::error::{AppendConditionFailed, AppendError};
use s2_sdk::types::{
    AccountEndpoint, AppendInput, AppendRecord, AppendRecordBatch, AppendRetryPolicy,
    BasinEndpoint, BasinName, EnsureStreamInput, ReadFrom, ReadInput, ReadLimits, ReadStart,
    ReadStop, RetryConfig, S2Config, S2Endpoints, StreamName,
};
use s2_sdk::{S2Basin, S2};

use super::config;
use super::protocol::{EventEnvelope, SequencedEvent};

#[derive(Debug)]
pub enum LogError {
    TailMismatch(u64),
    Invalid(String),
    Definite(String),
    Ambiguous(String),
    Unavailable(String),
    Corrupt(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TailMismatch(actual) => write!(formatter, "stream tail is now {actual}"),
            Self::Invalid(message) => write!(formatter, "event is invalid: {message}"),
            Self::Definite(message) => write!(formatter, "append was not accepted: {message}"),
            Self::Ambiguous(message) => write!(formatter, "append outcome is unknown: {message}"),
            Self::Unavailable(message) => write!(formatter, "S2 is unavailable: {message}"),
            Self::Corrupt(message) => write!(formatter, "invalid sync log: {message}"),
        }
    }
}

impl std::error::Error for LogError {}

#[derive(Clone)]
pub struct S2Log {
    basin: S2Basin,
    stream_prefix: String,
}

impl S2Log {
    pub fn new(settings: &config::S2Config) -> Result<Self, String> {
        let endpoints = S2Endpoints::new(
            AccountEndpoint::new(&settings.account_endpoint)
                .map_err(|_| "invalid s2.account_endpoint".to_string())?,
            BasinEndpoint::new(&settings.basin_endpoint)
                .map_err(|_| "invalid s2.basin_endpoint".to_string())?,
        )
        .map_err(|_| "S2 account and basin endpoints are incompatible".to_string())?;
        let basin_name: BasinName = settings
            .basin
            .parse()
            .map_err(|_| "invalid s2.basin".to_string())?;
        let retry = RetryConfig::new().with_append_retry_policy(AppendRetryPolicy::NoSideEffects);
        let client = S2::new(
            S2Config::new(settings.access_token.clone())
                .with_endpoints(endpoints)
                .with_retry(retry),
        )
        .map_err(|_| "failed to configure S2 client".to_string())?;
        Ok(Self {
            basin: client.basin(basin_name),
            stream_prefix: settings.stream_prefix.clone(),
        })
    }

    pub async fn ensure_workspace(&self, workspace: &str) -> Result<(), LogError> {
        let name = self.stream_name(workspace)?;
        self.basin
            .ensure_stream(EnsureStreamInput::new(name))
            .await
            .map_err(|error| LogError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn read_from(
        &self,
        workspace: &str,
        from: u64,
    ) -> Result<Vec<SequencedEvent>, LogError> {
        let stream = self.basin.stream(self.stream_name(workspace)?);
        let mut cursor = from;
        let mut events = Vec::new();

        loop {
            let tail = stream
                .check_tail()
                .await
                .map_err(|error| LogError::Unavailable(error.to_string()))?
                .seq_num;
            if cursor >= tail {
                return Ok(events);
            }
            let batch = stream
                .read(
                    ReadInput::new()
                        .with_start(ReadStart::new().with_from(ReadFrom::SeqNum(cursor)))
                        .with_stop(ReadStop::new().with_limits(ReadLimits::new().with_count(100))),
                )
                .await
                .map_err(|error| LogError::Unavailable(error.to_string()))?;
            if batch.records.is_empty() {
                return Err(LogError::Corrupt(format!(
                    "read stopped at {cursor} before tail {tail}"
                )));
            }
            for record in batch.records {
                if record.seq_num != cursor {
                    return Err(LogError::Corrupt(format!(
                        "expected sequence {cursor}, found {}",
                        record.seq_num
                    )));
                }
                let event: EventEnvelope =
                    serde_json::from_slice(&record.body).map_err(|error| {
                        LogError::Corrupt(format!(
                            "event at sequence {cursor} is not valid JSON: {error}"
                        ))
                    })?;
                events.push(SequencedEvent {
                    sequence: cursor,
                    event,
                });
                cursor = cursor
                    .checked_add(1)
                    .ok_or_else(|| LogError::Corrupt("sequence number exhausted".to_string()))?;
            }
        }
    }

    pub async fn append(
        &self,
        workspace: &str,
        expected_tail: u64,
        event: &EventEnvelope,
    ) -> Result<u64, LogError> {
        let body = serde_json::to_vec(event)
            .map_err(|error| LogError::Invalid(format!("event serialization failed: {error}")))?;
        let record = AppendRecord::new(body).map_err(|error| {
            LogError::Invalid(format!("event is too large or invalid: {error}"))
        })?;
        let records = AppendRecordBatch::try_from_iter([record])
            .map_err(|error| LogError::Invalid(format!("event batch is invalid: {error}")))?;
        let stream = self.basin.stream(self.stream_name(workspace)?);
        let expected_end = expected_tail
            .checked_add(1)
            .ok_or_else(|| LogError::Corrupt("sequence number exhausted".to_string()))?;
        match stream
            .append(AppendInput::new(records).with_match_seq_num(expected_tail))
            .await
        {
            Ok(ack) => {
                if ack.start.seq_num != expected_tail || ack.end.seq_num != expected_end {
                    return Err(LogError::Corrupt(format!(
                        "append acknowledgement was {}..{}, expected {}..{}",
                        ack.start.seq_num, ack.end.seq_num, expected_tail, expected_end
                    )));
                }
                Ok(ack.end.seq_num)
            }
            Err(AppendError::ConditionFailed(AppendConditionFailed::SeqNumMismatch(actual))) => {
                Err(LogError::TailMismatch(actual))
            }
            Err(error) if error.has_no_side_effects() => Err(LogError::Definite(error.to_string())),
            Err(error) => Err(LogError::Ambiguous(error.to_string())),
        }
    }

    fn stream_name(&self, workspace: &str) -> Result<StreamName, LogError> {
        format!("{}{}", self.stream_prefix, workspace)
            .parse()
            .map_err(|_| {
                LogError::Corrupt(format!("invalid stream name for workspace {workspace}"))
            })
    }
}
