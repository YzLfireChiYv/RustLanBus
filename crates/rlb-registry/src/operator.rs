//! Reusable persistent operator idempotency and bounded audit ledgers.

use std::collections::HashSet;

use rlb_control::{
    CanonicalCommandHash, CanonicalRevisions, RequestBoundary, RequestCache, RequestCacheAction,
    RequestCacheEffect, RequestCacheKey, RequestCachePolicy, RequestScope, RoomCommandName,
    reduce_request_cache,
};
use rlb_model::{Capability, ErrorCode, MemberId, RequestId, RoomId, ServerGeneration};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum retained operator requests and audit records.
pub const OPERATOR_LEDGER_CAPACITY: usize = 1_024;
/// Maximum records returned by one audit page.
pub const OPERATOR_AUDIT_PAGE_LIMIT: usize = 100;
/// Maximum audit retention measured from each appended wall-clock timestamp.
pub const OPERATOR_AUDIT_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Terminal state retained for an operator mutation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorCommandStatus {
    Applied,
    Unchanged,
    Rejected,
    OutcomeUnknown,
}

/// Authenticated operator identity and wall-clock observation attached to one
/// mutation from submission through its terminal outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAuditContext {
    pub principal: String,
    pub principal_kind: OperatorPrincipalKind,
    pub wall_time: String,
}

impl OperatorAuditContext {
    /// Validates the bounded principal and RFC 3339 wall time.
    pub fn validate(&self) -> Result<(), OperatorLedgerError> {
        validate_audit_context(self)
    }
}

/// Bounded terminal metadata retained for operator retries.
///
/// Delegated command arguments and effective member values are intentionally
/// absent so the authority never becomes a member-configuration replica.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCommandResult {
    pub request_id: RequestId,
    pub target_member_id: Option<MemberId>,
    pub capability: Option<Capability>,
    pub command: RoomCommandName,
    pub status: OperatorCommandStatus,
    pub revisions: CanonicalRevisions,
    pub code: Option<ErrorCode>,
}

impl OperatorCommandResult {
    /// Validates the status/error relationship.
    pub fn validate(&self) -> Result<(), OperatorLedgerError> {
        let needs_code = matches!(
            self.status,
            OperatorCommandStatus::Rejected | OperatorCommandStatus::OutcomeUnknown
        );
        if needs_code != self.code.is_some() {
            return Err(OperatorLedgerError::ResultShape);
        }
        Ok(())
    }
}

/// Persisted metadata for a delivered request awaiting a terminal result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorPendingRequest {
    pub request_key: RequestCacheKey,
    pub command_hash: CanonicalCommandHash,
    pub audit_context: OperatorAuditContext,
    pub target_member_id: MemberId,
    pub capability: Capability,
    pub command: RoomCommandName,
    pub before_revisions: CanonicalRevisions,
    pub accepted_revisions: CanonicalRevisions,
}

impl From<crate::wire::MemberCommandStatus> for OperatorCommandStatus {
    fn from(value: crate::wire::MemberCommandStatus) -> Self {
        match value {
            crate::wire::MemberCommandStatus::Applied => Self::Applied,
            crate::wire::MemberCommandStatus::Unchanged => Self::Unchanged,
            crate::wire::MemberCommandStatus::Rejected => Self::Rejected,
            crate::wire::MemberCommandStatus::OutcomeUnknown => Self::OutcomeUnknown,
        }
    }
}

/// Result of beginning an operator request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperatorRequestDecision {
    Execute,
    Replay(Option<OperatorCommandResult>),
    RequestIdConflict,
    Full,
}

/// Persistent request ledger for one room.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorRequestLedger {
    room_id: RoomId,
    cache: RequestCache<OperatorCommandResult>,
    #[serde(default)]
    pending: Vec<OperatorPendingRequest>,
}

impl OperatorRequestLedger {
    /// Creates an empty 1,024-record operator ledger.
    #[must_use]
    pub const fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            cache: RequestCache::new(RequestCachePolicy::OPERATOR),
            pending: Vec::new(),
        }
    }

    /// Returns the room scope of every request in this ledger.
    #[must_use]
    pub const fn room_id(&self) -> RoomId {
        self.room_id
    }

    /// Returns the immutable request records for persistence and diagnostics.
    #[must_use]
    pub fn records(&self) -> &[rlb_control::RequestCacheRecord<OperatorCommandResult>] {
        &self.cache.records
    }

    /// Validates policy, scope, uniqueness, and terminal-result shapes.
    pub fn validate(&self) -> Result<(), OperatorLedgerError> {
        if self.cache.policy != RequestCachePolicy::OPERATOR
            || self.cache.records.len() > OPERATOR_LEDGER_CAPACITY
        {
            return Err(OperatorLedgerError::RequestPolicy);
        }
        let mut keys = HashSet::new();
        for record in &self.cache.records {
            if record.key.boundary() != RequestBoundary::Operator
                || record.key.scope()
                    != (RequestScope::Room {
                        room_id: self.room_id,
                    })
                || !keys.insert(record.key.clone())
            {
                return Err(OperatorLedgerError::RequestScope);
            }
            if let Some(result) = &record.response {
                if result.request_id != record.key.request_id() {
                    return Err(OperatorLedgerError::RequestScope);
                }
                result.validate()?;
            }
        }
        let mut pending_keys = HashSet::new();
        for pending in &self.pending {
            if pending.request_key.boundary() != RequestBoundary::Operator
                || pending.request_key.principal() != pending.audit_context.principal
                || pending.request_key.scope()
                    != (RequestScope::Room {
                        room_id: self.room_id,
                    })
                || !pending_keys.insert(pending.request_key.clone())
                || !self.cache.records.iter().any(|record| {
                    record.key == pending.request_key
                        && record.command_hash == pending.command_hash
                        && record.response.is_none()
                })
                || validate_audit_context(&pending.audit_context).is_err()
            {
                return Err(OperatorLedgerError::RequestScope);
            }
        }
        if self
            .cache
            .records
            .iter()
            .filter(|record| record.response.is_none())
            .count()
            != self.pending.len()
        {
            return Err(OperatorLedgerError::InvalidTransition);
        }
        Ok(())
    }

    /// Begins a request, returning a cached terminal result or in-flight marker
    /// for a repeated canonical body.
    pub fn begin(
        &mut self,
        principal: &str,
        request_id: RequestId,
        command_hash: CanonicalCommandHash,
        now_ms: u64,
    ) -> Result<(RequestCacheKey, OperatorRequestDecision), OperatorLedgerError> {
        let key = RequestCacheKey::new(
            principal,
            RequestBoundary::Operator,
            RequestScope::Room {
                room_id: self.room_id,
            },
            request_id,
        )
        .map_err(|_| OperatorLedgerError::Principal)?;
        let (next, effect) = reduce_request_cache(
            &self.cache,
            RequestCacheAction::Begin {
                key: key.clone(),
                command_hash,
                now_ms,
            },
        );
        self.cache = next;
        let decision = match effect {
            RequestCacheEffect::Execute => OperatorRequestDecision::Execute,
            RequestCacheEffect::Replay(result) => OperatorRequestDecision::Replay(result),
            RequestCacheEffect::RequestIdConflict => OperatorRequestDecision::RequestIdConflict,
            RequestCacheEffect::Full => OperatorRequestDecision::Full,
            RequestCacheEffect::Stored
            | RequestCacheEffect::Missing
            | RequestCacheEffect::Purged(_) => {
                return Err(OperatorLedgerError::InvalidTransition);
            }
        };
        Ok((key, decision))
    }

    /// Attaches bounded command metadata after the request is delivered.
    pub fn mark_in_flight(
        &mut self,
        pending: OperatorPendingRequest,
    ) -> Result<(), OperatorLedgerError> {
        if pending.request_key.boundary() != RequestBoundary::Operator
            || pending.request_key.principal() != pending.audit_context.principal
            || pending.request_key.scope()
                != (RequestScope::Room {
                    room_id: self.room_id,
                })
            || self
                .pending
                .iter()
                .any(|entry| entry.request_key == pending.request_key)
            || validate_audit_context(&pending.audit_context).is_err()
            || !self.cache.records.iter().any(|record| {
                record.key == pending.request_key
                    && record.command_hash == pending.command_hash
                    && record.response.is_none()
            })
        {
            return Err(OperatorLedgerError::InvalidTransition);
        }
        self.pending.push(pending);
        Ok(())
    }

    /// Stores one terminal result against its exact canonical body.
    pub fn complete(
        &mut self,
        key: RequestCacheKey,
        command_hash: CanonicalCommandHash,
        result: OperatorCommandResult,
    ) -> Result<(), OperatorLedgerError> {
        result.validate()?;
        if result.request_id != key.request_id() {
            return Err(OperatorLedgerError::RequestScope);
        }
        let completed_key = key.clone();
        let (next, effect) = reduce_request_cache(
            &self.cache,
            RequestCacheAction::StoreResponse {
                key,
                command_hash,
                response: result,
            },
        );
        if effect != RequestCacheEffect::Stored {
            return Err(match effect {
                RequestCacheEffect::RequestIdConflict => OperatorLedgerError::RequestIdConflict,
                RequestCacheEffect::Missing => OperatorLedgerError::MissingRequest,
                RequestCacheEffect::Execute
                | RequestCacheEffect::Replay(_)
                | RequestCacheEffect::Stored
                | RequestCacheEffect::Full
                | RequestCacheEffect::Purged(_) => OperatorLedgerError::InvalidTransition,
            });
        }
        self.cache = next;
        self.pending
            .retain(|pending| pending.request_key != completed_key);
        Ok(())
    }

    /// Purges records whose minimum retention has elapsed.
    pub fn purge_expired(&mut self, now_ms: u64) -> usize {
        let (next, effect) =
            reduce_request_cache(&self.cache, RequestCacheAction::PurgeExpired { now_ms });
        self.cache = next;
        match effect {
            RequestCacheEffect::Purged(count) => count,
            RequestCacheEffect::Execute
            | RequestCacheEffect::Replay(_)
            | RequestCacheEffect::RequestIdConflict
            | RequestCacheEffect::Stored
            | RequestCacheEffect::Missing
            | RequestCacheEffect::Full => 0,
        }
    }
}

/// Principal class retained in room audit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorPrincipalKind {
    Operator,
    Member,
}

/// Caller-provided bounded audit payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorAuditInput {
    pub wall_time: String,
    pub server_generation: ServerGeneration,
    pub principal: String,
    pub principal_kind: OperatorPrincipalKind,
    pub room_id: RoomId,
    pub result: OperatorCommandResult,
    pub canonical_command_hash: CanonicalCommandHash,
    pub before_revisions: CanonicalRevisions,
}

/// Complete data required to atomically finish idempotency and append audit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorOutcomeInput {
    pub request_key: RequestCacheKey,
    pub canonical_command_hash: CanonicalCommandHash,
    pub audit_context: OperatorAuditContext,
    pub server_generation: ServerGeneration,
    pub room_id: RoomId,
    pub result: OperatorCommandResult,
    pub before_revisions: CanonicalRevisions,
}

/// One persistent room-audit record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAuditRecord {
    pub audit_seq: u64,
    pub wall_time: String,
    pub server_generation: ServerGeneration,
    pub principal: String,
    pub principal_kind: OperatorPrincipalKind,
    pub room_id: RoomId,
    pub target_member_id: Option<MemberId>,
    pub capability: Option<Capability>,
    pub command: RoomCommandName,
    pub request_id: RequestId,
    pub canonical_command_hash: CanonicalCommandHash,
    pub outcome: OperatorCommandStatus,
    pub error_code: Option<ErrorCode>,
    pub before_revisions: CanonicalRevisions,
    pub after_revisions: CanonicalRevisions,
}

/// Persistent 1,024-entry room audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAuditLedger {
    room_id: RoomId,
    next_audit_seq: u64,
    records: Vec<OperatorAuditRecord>,
}

impl OperatorAuditLedger {
    /// Creates an empty audit sequence for one room.
    #[must_use]
    pub const fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            next_audit_seq: 1,
            records: Vec::new(),
        }
    }

    /// Appends one bounded audit record and evicts the oldest at capacity.
    pub fn append(
        &mut self,
        input: OperatorAuditInput,
    ) -> Result<OperatorAuditRecord, OperatorLedgerError> {
        input.result.validate()?;
        let context = OperatorAuditContext {
            principal: input.principal.clone(),
            principal_kind: input.principal_kind,
            wall_time: input.wall_time.clone(),
        };
        if input.room_id != self.room_id || validate_audit_context(&context).is_err() {
            return Err(OperatorLedgerError::AuditValue);
        }
        self.purge_older_than(&input.wall_time)?;
        let record = OperatorAuditRecord {
            audit_seq: self.next_audit_seq,
            wall_time: input.wall_time,
            server_generation: input.server_generation,
            principal: input.principal,
            principal_kind: input.principal_kind,
            room_id: input.room_id,
            target_member_id: input.result.target_member_id,
            capability: input.result.capability,
            command: input.result.command,
            request_id: input.result.request_id,
            canonical_command_hash: input.canonical_command_hash,
            outcome: input.result.status,
            error_code: input.result.code,
            before_revisions: input.before_revisions,
            after_revisions: input.result.revisions,
        };
        self.next_audit_seq = self
            .next_audit_seq
            .checked_add(1)
            .ok_or(OperatorLedgerError::AuditSequence)?;
        while self.records.len() >= OPERATOR_LEDGER_CAPACITY {
            self.records.remove(0);
        }
        self.records.push(record.clone());
        Ok(record)
    }

    /// Returns every retained audit record.
    #[must_use]
    pub fn records(&self) -> &[OperatorAuditRecord] {
        &self.records
    }

    /// Removes records older than thirty days at the supplied wall time.
    pub fn purge_older_than(
        &mut self,
        current_wall_time: &str,
    ) -> Result<usize, OperatorLedgerError> {
        let current_ms =
            parse_rfc3339_millis(current_wall_time).ok_or(OperatorLedgerError::AuditValue)?;
        let before = self.records.len();
        let mut invalid = false;
        self.records.retain(|record| {
            let Some(record_ms) = parse_rfc3339_millis(&record.wall_time) else {
                invalid = true;
                return true;
            };
            current_ms.saturating_sub(record_ms) <= OPERATOR_AUDIT_RETENTION_MS
        });
        if invalid {
            return Err(OperatorLedgerError::AuditValue);
        }
        Ok(before - self.records.len())
    }

    /// Returns a bounded page strictly after an audit sequence.
    #[must_use]
    pub fn records_after(&self, after_audit_seq: u64, limit: usize) -> Vec<&OperatorAuditRecord> {
        self.records
            .iter()
            .filter(|record| record.audit_seq > after_audit_seq)
            .take(limit.min(OPERATOR_AUDIT_PAGE_LIMIT))
            .collect()
    }

    /// Validates room scope, sequence order, bounds, and field limits.
    pub fn validate(&self) -> Result<(), OperatorLedgerError> {
        if self.records.len() > OPERATOR_LEDGER_CAPACITY
            || self
                .records
                .windows(2)
                .any(|pair| pair[0].audit_seq >= pair[1].audit_seq)
            || self
                .records
                .last()
                .is_some_and(|record| record.audit_seq >= self.next_audit_seq)
        {
            return Err(OperatorLedgerError::AuditSequence);
        }
        for record in &self.records {
            if record.room_id != self.room_id
                || record.wall_time.is_empty()
                || record.wall_time.len() > 64
                || !record.wall_time.is_ascii()
                || record.principal.is_empty()
                || record.principal.len() > 128
                || record.principal.chars().any(char::is_control)
                || parse_rfc3339_millis(&record.wall_time).is_none()
            {
                return Err(OperatorLedgerError::AuditValue);
            }
        }
        Ok(())
    }
}

/// Complete persistent operator state owned by registryd.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorState {
    pub requests: OperatorRequestLedger,
    pub audit: OperatorAuditLedger,
}

impl OperatorState {
    /// Creates empty request and audit ledgers for one room.
    #[must_use]
    pub const fn new(room_id: RoomId) -> Self {
        Self {
            requests: OperatorRequestLedger::new(room_id),
            audit: OperatorAuditLedger::new(room_id),
        }
    }

    /// Validates both ledgers and their common room scope.
    pub fn validate(&self) -> Result<(), OperatorLedgerError> {
        if self.requests.room_id() != self.audit.room_id {
            return Err(OperatorLedgerError::RequestScope);
        }
        self.requests.validate()?;
        self.audit.validate()
    }

    /// Completes a request and appends its audit record as one candidate state.
    pub fn record_outcome(
        &mut self,
        input: OperatorOutcomeInput,
    ) -> Result<OperatorAuditRecord, OperatorLedgerError> {
        if input.room_id != self.requests.room_id()
            || input.request_key.principal() != input.audit_context.principal
        {
            return Err(OperatorLedgerError::RequestScope);
        }
        validate_audit_context(&input.audit_context)?;
        let mut candidate = self.clone();
        candidate.requests.complete(
            input.request_key,
            input.canonical_command_hash,
            input.result.clone(),
        )?;
        let record = candidate.audit.append(OperatorAuditInput {
            wall_time: input.audit_context.wall_time,
            server_generation: input.server_generation,
            principal: input.audit_context.principal,
            principal_kind: input.audit_context.principal_kind,
            room_id: input.room_id,
            result: input.result,
            canonical_command_hash: input.canonical_command_hash,
            before_revisions: input.before_revisions,
        })?;
        candidate.validate()?;
        *self = candidate;
        Ok(record)
    }

    /// Terminalizes requests restored without live delivery state.
    pub fn interrupt_incomplete(
        &mut self,
        server_generation: ServerGeneration,
        code: ErrorCode,
    ) -> Result<Vec<OperatorCommandResult>, OperatorLedgerError> {
        let pending = self.requests.pending.clone();
        let mut results = Vec::with_capacity(pending.len());
        for pending in pending {
            let result = OperatorCommandResult {
                request_id: pending.request_key.request_id(),
                target_member_id: Some(pending.target_member_id),
                capability: Some(pending.capability),
                command: pending.command,
                status: OperatorCommandStatus::OutcomeUnknown,
                revisions: pending.accepted_revisions,
                code: Some(code),
            };
            self.record_outcome(OperatorOutcomeInput {
                request_key: pending.request_key,
                canonical_command_hash: pending.command_hash,
                audit_context: pending.audit_context,
                server_generation,
                room_id: self.requests.room_id(),
                result: result.clone(),
                before_revisions: pending.before_revisions,
            })?;
            results.push(result);
        }
        Ok(results)
    }

    /// Applies the thirty-day audit boundary during persistent reload.
    pub fn purge_audit(&mut self, current_wall_time: &str) -> Result<usize, OperatorLedgerError> {
        self.audit.purge_older_than(current_wall_time)
    }
}

fn validate_audit_context(context: &OperatorAuditContext) -> Result<(), OperatorLedgerError> {
    if context.wall_time.is_empty()
        || context.wall_time.len() > 64
        || !context.wall_time.is_ascii()
        || parse_rfc3339_millis(&context.wall_time).is_none()
        || context.principal.is_empty()
        || context.principal.len() > 128
        || context.principal.chars().any(char::is_control)
    {
        Err(OperatorLedgerError::AuditValue)
    } else {
        Ok(())
    }
}

fn parse_rfc3339_millis(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = parse_digits(bytes.get(0..4)?)? as i64;
    let month = parse_digits(bytes.get(5..7)?)? as u8;
    let day = parse_digits(bytes.get(8..10)?)? as u8;
    let hour = parse_digits(bytes.get(11..13)?)? as u8;
    let minute = parse_digits(bytes.get(14..16)?)? as u8;
    let second = parse_digits(bytes.get(17..19)?)? as u8;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut index = 19;
    let mut milliseconds = 0_u64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            if index - fraction_start < 3 {
                milliseconds = milliseconds * 10 + u64::from(bytes[index].saturating_sub(b'0'));
            }
            index += 1;
        }
        let fraction_digits = index - fraction_start;
        if fraction_digits == 0 {
            return None;
        }
        for _ in fraction_digits.min(3)..3 {
            milliseconds *= 10;
        }
    }

    let offset_seconds = match bytes.get(index).copied()? {
        b'Z' if index + 1 == bytes.len() => 0_i64,
        sign @ (b'+' | b'-') if index + 6 == bytes.len() && bytes.get(index + 3) == Some(&b':') => {
            let offset_hour = parse_digits(bytes.get(index + 1..index + 3)?)? as i64;
            let offset_minute = parse_digits(bytes.get(index + 4..index + 6)?)? as i64;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let magnitude = offset_hour * 3_600 + offset_minute * 60;
            if sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    let days = days_from_civil(year, i64::from(month), i64::from(day));
    let local_seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    let utc_seconds = local_seconds.checked_sub(offset_seconds)?;
    u64::try_from(utc_seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(milliseconds)
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
    })
}

const fn days_in_month(year: i64, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Operator-ledger invariant or transition failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum OperatorLedgerError {
    #[error("operator request cache policy is invalid")]
    RequestPolicy,
    #[error("operator request scope or identity is invalid")]
    RequestScope,
    #[error("operator principal label is invalid")]
    Principal,
    #[error("operator request ID conflicts with its canonical body")]
    RequestIdConflict,
    #[error("operator request is missing")]
    MissingRequest,
    #[error("operator result status and error code disagree")]
    ResultShape,
    #[error("operator ledger transition is invalid")]
    InvalidTransition,
    #[error("operator audit value is invalid")]
    AuditValue,
    #[error("operator audit sequence is invalid or exhausted")]
    AuditSequence,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_id() -> RoomId {
        RoomId::from_bytes(1_u128.to_be_bytes()).unwrap()
    }

    fn request_id(value: u128) -> RequestId {
        RequestId::from_bytes(value.to_be_bytes()).unwrap()
    }

    fn result(request_id: RequestId) -> OperatorCommandResult {
        OperatorCommandResult {
            request_id,
            target_member_id: None,
            capability: None,
            command: RoomCommandName::RoomSendMuteSet,
            status: OperatorCommandStatus::Applied,
            revisions: CanonicalRevisions {
                room_revision: Some(rlb_model::RoomRevision::new(2)),
                ..CanonicalRevisions::default()
            },
            code: None,
        }
    }

    #[test]
    fn request_ledger_replays_terminal_result_and_rejects_hash_conflict() {
        let mut ledger = OperatorRequestLedger::new(room_id());
        let hash = CanonicalCommandHash::from_bytes([1; 32]);
        let other_hash = CanonicalCommandHash::from_bytes([2; 32]);
        let request_id = request_id(2);
        let (key, decision) = ledger.begin("operator-1", request_id, hash, 0).unwrap();
        assert_eq!(decision, OperatorRequestDecision::Execute);
        let terminal = result(request_id);
        ledger.complete(key, hash, terminal.clone()).unwrap();
        let (_, decision) = ledger.begin("operator-1", request_id, hash, 1).unwrap();
        assert_eq!(decision, OperatorRequestDecision::Replay(Some(terminal)));
        let (_, decision) = ledger
            .begin("operator-1", request_id, other_hash, 1)
            .unwrap();
        assert_eq!(decision, OperatorRequestDecision::RequestIdConflict);
        assert_eq!(ledger.records().len(), 1);
    }

    #[test]
    fn audit_is_bounded_and_pages_after_sequence() {
        let mut audit = OperatorAuditLedger::new(room_id());
        for value in 1..=OPERATOR_LEDGER_CAPACITY + 1 {
            let request_id = request_id(value as u128 + 10);
            audit
                .append(OperatorAuditInput {
                    wall_time: "2026-09-18T05:20:00+08:00".to_owned(),
                    server_generation: ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap(),
                    principal: "operator-1".to_owned(),
                    principal_kind: OperatorPrincipalKind::Operator,
                    room_id: room_id(),
                    result: result(request_id),
                    canonical_command_hash: CanonicalCommandHash::from_bytes([3; 32]),
                    before_revisions: CanonicalRevisions::default(),
                })
                .unwrap();
        }
        assert_eq!(audit.records.len(), OPERATOR_LEDGER_CAPACITY);
        assert_eq!(audit.records[0].audit_seq, 2);
        assert_eq!(audit.records_after(1, 200).len(), OPERATOR_AUDIT_PAGE_LIMIT);
        audit.validate().unwrap();
    }

    #[test]
    fn audit_uses_thirty_day_retention_on_reload() {
        let mut state = OperatorState::new(room_id());
        for (request, wall_time, hash) in [
            (20, "2026-01-01T00:00:00Z", [4; 32]),
            (21, "2026-01-31T00:00:00Z", [5; 32]),
        ] {
            state
                .audit
                .append(OperatorAuditInput {
                    wall_time: wall_time.to_owned(),
                    server_generation: ServerGeneration::from_bytes(9_u128.to_be_bytes()).unwrap(),
                    principal: "operator-1".to_owned(),
                    principal_kind: OperatorPrincipalKind::Operator,
                    room_id: room_id(),
                    result: result(request_id(request)),
                    canonical_command_hash: CanonicalCommandHash::from_bytes(hash),
                    before_revisions: CanonicalRevisions::default(),
                })
                .unwrap();
        }
        assert_eq!(state.audit.records().len(), 2);

        let encoded = serde_json::to_vec(&state).unwrap();
        let mut restored: OperatorState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.purge_audit("2026-02-01T00:00:00Z").unwrap(), 1);
        assert_eq!(restored.audit.records().len(), 1);
        assert_eq!(restored.audit.records()[0].request_id, request_id(21));
        restored.validate().unwrap();
    }
}
