//! Structured audit logging for admin session management operations.
//!
//! All write operations (cancel, terminate, terminate-all) emit a structured
//! audit log entry at INFO level. See `docs/design/32_admin_session_management.md`.

use tracing::info;

/// Actions that can be audited.
#[derive(Debug, Clone, Copy)]
pub enum AuditAction {
    CancelQuery,
    TerminateSession,
    TerminateAll,
}

impl AuditAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CancelQuery => "cancel_query",
            Self::TerminateSession => "terminate_session",
            Self::TerminateAll => "terminate_all",
        }
    }
}

/// Result of an audited operation.
#[derive(Debug, Clone, Copy)]
pub enum AuditResult {
    Success,
    NotFound,
    NoActiveQuery,
}

impl AuditResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotFound => "not_found",
            Self::NoActiveQuery => "no_active_query",
        }
    }
}

/// Emit a structured audit log entry for an admin session operation.
///
/// All fields are included as structured tracing fields so they can be
/// filtered, queried, and forwarded by the tracing subscriber pipeline.
pub fn emit_audit_log(
    admin_actor: &str,
    action: AuditAction,
    target_tenant_id: &str,
    target_connection_ids: &[i64],
    server_id: &str,
    result: AuditResult,
    reason: Option<&str>,
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    info!(
        audit = true,
        admin_actor = admin_actor,
        action = action.as_str(),
        target_tenant_id = target_tenant_id,
        target_connection_ids = ?target_connection_ids,
        server_id = server_id,
        result = result.as_str(),
        reason = reason.unwrap_or(""),
        timestamp_epoch_ms = now_ms,
        "admin session operation"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_action_as_str() {
        assert_eq!(AuditAction::CancelQuery.as_str(), "cancel_query");
        assert_eq!(AuditAction::TerminateSession.as_str(), "terminate_session");
        assert_eq!(AuditAction::TerminateAll.as_str(), "terminate_all");
    }

    #[test]
    fn audit_result_as_str() {
        assert_eq!(AuditResult::Success.as_str(), "success");
        assert_eq!(AuditResult::NotFound.as_str(), "not_found");
        assert_eq!(AuditResult::NoActiveQuery.as_str(), "no_active_query");
    }

    #[test]
    fn emit_audit_log_does_not_panic() {
        // Verify emit_audit_log can be called without panic.
        // Actual log output depends on subscriber configuration.
        emit_audit_log(
            "test_admin",
            AuditAction::CancelQuery,
            "tenant_a",
            &[1, 2, 3],
            "test-server",
            AuditResult::Success,
            Some("slow query"),
        );

        emit_audit_log(
            "test_admin",
            AuditAction::TerminateAll,
            "tenant_b",
            &[],
            "test-server",
            AuditResult::NotFound,
            None,
        );
    }
}
