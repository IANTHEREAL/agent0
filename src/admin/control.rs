//! Transport-agnostic admin control service for session management.
//!
//! Maps logical operations (list, cancel, terminate, terminate-all) to
//! `SessionRegistry` calls with audit logging. This is the shared core
//! consumed by any admin API entrypoint (backend proxy, server-direct, or both).
//!
//! See `docs/design/32_admin_session_management.md`.

use super::audit::{emit_audit_log, AuditAction, AuditResult};
use super::session_registry::{
    CancelError, SessionFilter, SessionRegistry, SessionSnapshot, TerminateAllResult,
    ALL_TENANTS_MAX_LIMIT,
};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by control service operations.
#[derive(Debug)]
pub enum ControlError {
    /// No `tenant_id` provided and `all_tenants` is false.
    MissingTenantScope,
    /// Empty tenant ID for terminate-all.
    EmptyTenantId,
    /// Connection not found.
    NotFound,
    /// No active query to cancel.
    NoActiveQuery,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTenantScope => {
                write!(f, "tenant_id is required unless all_tenants=true")
            }
            Self::EmptyTenantId => write!(f, "tenant_id must not be empty"),
            Self::NotFound => write!(f, "connection not found"),
            Self::NoActiveQuery => write!(f, "no active query"),
        }
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Response for list operations.
#[derive(Debug)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSnapshot>,
    pub has_more: bool,
}

/// Response for cancel/terminate operations.
#[derive(Debug)]
pub struct SessionActionResponse {
    pub connection_id: i64,
    pub result: &'static str,
    pub query_was: String,
}

/// Response for terminate-all operations.
#[derive(Debug)]
pub struct TerminateAllResponse {
    pub tenant_id: String,
    pub requested: u32,
    pub terminated: u32,
    pub already_closed: u32,
}

// ---------------------------------------------------------------------------
// AdminControlService
// ---------------------------------------------------------------------------

/// Transport-agnostic admin control service.
///
/// Holds a reference to the `SessionRegistry` and provides validated,
/// audit-logged operations for admin session management.
pub struct AdminControlService<'a> {
    registry: &'a SessionRegistry,
}

impl<'a> AdminControlService<'a> {
    pub fn new(registry: &'a SessionRegistry) -> Self {
        Self { registry }
    }

    /// List sessions matching the given filter.
    ///
    /// Enforces fail-closed rule: returns `MissingTenantScope` if neither
    /// `tenant_id` nor `all_tenants=true` is provided.
    pub fn list_sessions(&self, filter: &SessionFilter) -> Result<ListSessionsResponse, ControlError> {
        if filter.tenant_id.is_none() && !filter.all_tenants {
            return Err(ControlError::MissingTenantScope);
        }

        // Compute the effective limit the registry will actually use.
        // The registry caps all-tenant queries at ALL_TENANTS_MAX_LIMIT.
        let effective_limit = if filter.all_tenants && filter.tenant_id.is_none() {
            filter.limit.min(ALL_TENANTS_MAX_LIMIT)
        } else {
            filter.limit
        };

        // Probe with effective_limit+1 to determine has_more without
        // false positives or false negatives.
        let probe_filter = SessionFilter {
            tenant_id: filter.tenant_id.clone(),
            all_tenants: filter.all_tenants,
            principal: filter.principal.clone(),
            state: filter.state,
            min_duration_ms: filter.min_duration_ms,
            limit: effective_limit.saturating_add(1),
            offset: filter.offset,
        };
        let mut sessions = self.registry.list(&probe_filter);
        let has_more = sessions.len() > effective_limit;
        sessions.truncate(effective_limit);

        Ok(ListSessionsResponse { sessions, has_more })
    }

    /// Cancel the current query on a connection. Connection stays open.
    pub fn cancel_query(
        &self,
        connection_id: i64,
        admin_actor: &str,
        reason: Option<&str>,
    ) -> Result<SessionActionResponse, ControlError> {
        // Grab the query text before cancelling (for audit/response).
        let query_was = self
            .registry
            .get(connection_id)
            .map(|s| s.current_query.clone())
            .unwrap_or_default();

        let result = self.registry.cancel_query(connection_id);

        // Determine tenant_id for audit (best-effort).
        let tenant_id = self
            .registry
            .get(connection_id)
            .map(|s| s.tenant_id.clone())
            .unwrap_or_default();

        let (audit_result, response_result) = match &result {
            Ok(()) => (AuditResult::Success, "cancelled"),
            Err(CancelError::NotFound) => (AuditResult::NotFound, "not_found"),
            Err(CancelError::NoActiveQuery) => (AuditResult::NoActiveQuery, "no_active_query"),
        };

        emit_audit_log(
            admin_actor,
            AuditAction::CancelQuery,
            &tenant_id,
            &[connection_id],
            self.registry.server_id(),
            audit_result,
            reason,
        );

        match result {
            Ok(()) => Ok(SessionActionResponse {
                connection_id,
                result: response_result,
                query_was,
            }),
            Err(CancelError::NotFound) => Err(ControlError::NotFound),
            Err(CancelError::NoActiveQuery) => Err(ControlError::NoActiveQuery),
        }
    }

    /// Terminate a connection entirely.
    pub fn terminate(
        &self,
        connection_id: i64,
        admin_actor: &str,
        reason: Option<&str>,
    ) -> Result<SessionActionResponse, ControlError> {
        let query_was = self
            .registry
            .get(connection_id)
            .map(|s| s.current_query.clone())
            .unwrap_or_default();

        let tenant_id = self
            .registry
            .get(connection_id)
            .map(|s| s.tenant_id.clone())
            .unwrap_or_default();

        let result = self.registry.terminate(connection_id);

        let audit_result = match &result {
            Ok(()) => AuditResult::Success,
            Err(CancelError::NotFound) => AuditResult::NotFound,
            Err(CancelError::NoActiveQuery) => unreachable!("terminate never returns NoActiveQuery"),
        };

        emit_audit_log(
            admin_actor,
            AuditAction::TerminateSession,
            &tenant_id,
            &[connection_id],
            self.registry.server_id(),
            audit_result,
            reason,
        );

        match result {
            Ok(()) => Ok(SessionActionResponse {
                connection_id,
                result: "terminated",
                query_was,
            }),
            Err(CancelError::NotFound) => Err(ControlError::NotFound),
            Err(CancelError::NoActiveQuery) => unreachable!(),
        }
    }

    /// Terminate all connections for a tenant.
    ///
    /// Enforces fail-closed rule: returns `EmptyTenantId` if tenant_id is empty.
    pub fn terminate_all(
        &self,
        tenant_id: &str,
        admin_actor: &str,
        reason: Option<&str>,
    ) -> Result<TerminateAllResponse, ControlError> {
        if tenant_id.is_empty() {
            return Err(ControlError::EmptyTenantId);
        }

        let TerminateAllResult {
            requested,
            terminated,
            already_closed,
        } = self.registry.terminate_all(tenant_id);

        // Collect connection IDs for audit (best-effort from tenant index).
        // After terminate_all, connections may already be gone, so we log
        // the counts rather than exact IDs.
        emit_audit_log(
            admin_actor,
            AuditAction::TerminateAll,
            tenant_id,
            &[],
            self.registry.server_id(),
            AuditResult::Success,
            reason,
        );

        Ok(TerminateAllResponse {
            tenant_id: tenant_id.to_owned(),
            requested,
            terminated,
            already_closed,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::session_registry::SessionInfo;
    use pgwire::tokio::CancellationToken;

    fn make_registry() -> SessionRegistry {
        SessionRegistry::new()
    }

    fn make_info(connection_id: i64, tenant_id: &str, principal: &str) -> SessionInfo {
        SessionInfo::new(
            connection_id,
            tenant_id.to_owned(),
            principal.to_owned(),
            "testdb".to_owned(),
            "127.0.0.1:5432".to_owned(),
            CancellationToken::new(),
        )
    }

    #[test]
    fn list_sessions_fail_closed() {
        let reg = make_registry();
        reg.register(make_info(1, "t", "alice"));
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter::default();
        let err = svc.list_sessions(&filter).unwrap_err();
        assert!(matches!(err, ControlError::MissingTenantScope));
    }

    #[test]
    fn list_sessions_by_tenant() {
        let reg = make_registry();
        reg.register(make_info(1, "tenant_a", "alice"));
        reg.register(make_info(2, "tenant_a", "bob"));
        reg.register(make_info(3, "tenant_b", "carol"));
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter {
            tenant_id: Some("tenant_a".to_owned()),
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(resp.sessions.len(), 2);
        assert!(resp.sessions.iter().all(|s| s.tenant_id == "tenant_a"));
    }

    #[test]
    fn list_sessions_all_tenants() {
        let reg = make_registry();
        reg.register(make_info(1, "t1", "alice"));
        reg.register(make_info(2, "t2", "bob"));
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter {
            all_tenants: true,
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(resp.sessions.len(), 2);
    }

    #[test]
    fn list_sessions_has_more() {
        let reg = make_registry();
        for i in 0..5 {
            reg.register(make_info(i, "t", "user"));
        }
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 3,
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(resp.sessions.len(), 3);
        assert!(resp.has_more);

        let filter_all = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 100,
            ..Default::default()
        };
        let resp_all = svc.list_sessions(&filter_all).unwrap();
        assert_eq!(resp_all.sessions.len(), 5);
        assert!(!resp_all.has_more);
    }

    #[test]
    fn cancel_query_success() {
        let reg = make_registry();
        let token = CancellationToken::new();
        reg.register(SessionInfo::new(
            1,
            "t".to_owned(),
            "alice".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            token.clone(),
        ));
        let info = reg.get_session(1).unwrap();
        let qc = info.begin_query("SELECT 1");

        let svc = AdminControlService::new(&reg);
        let resp = svc.cancel_query(1, "admin@test", Some("slow query")).unwrap();
        assert_eq!(resp.connection_id, 1);
        assert_eq!(resp.result, "cancelled");
        assert_eq!(resp.query_was, "SELECT 1");
        assert!(qc.is_cancelled());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_query_not_found() {
        let reg = make_registry();
        let svc = AdminControlService::new(&reg);
        let err = svc.cancel_query(999, "admin", None).unwrap_err();
        assert!(matches!(err, ControlError::NotFound));
    }

    #[test]
    fn cancel_query_no_active() {
        let reg = make_registry();
        reg.register(make_info(1, "t", "alice"));
        let svc = AdminControlService::new(&reg);
        let err = svc.cancel_query(1, "admin", None).unwrap_err();
        assert!(matches!(err, ControlError::NoActiveQuery));
    }

    #[test]
    fn terminate_success() {
        let reg = make_registry();
        let token = CancellationToken::new();
        reg.register(SessionInfo::new(
            1,
            "t".to_owned(),
            "alice".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            token.clone(),
        ));

        let svc = AdminControlService::new(&reg);
        let resp = svc.terminate(1, "admin", Some("misbehaving")).unwrap();
        assert_eq!(resp.connection_id, 1);
        assert_eq!(resp.result, "terminated");
        assert!(token.is_cancelled());
    }

    #[test]
    fn terminate_not_found() {
        let reg = make_registry();
        let svc = AdminControlService::new(&reg);
        let err = svc.terminate(999, "admin", None).unwrap_err();
        assert!(matches!(err, ControlError::NotFound));
    }

    #[test]
    fn terminate_all_success() {
        let reg = make_registry();
        let tokens: Vec<CancellationToken> = (0..3)
            .map(|i| {
                let t = CancellationToken::new();
                reg.register(SessionInfo::new(
                    i,
                    "tenant_x".to_owned(),
                    "user".to_owned(),
                    "db".to_owned(),
                    "127.0.0.1:1".to_owned(),
                    t.clone(),
                ));
                t
            })
            .collect();

        let svc = AdminControlService::new(&reg);
        let resp = svc.terminate_all("tenant_x", "admin", Some("incident")).unwrap();
        assert_eq!(resp.tenant_id, "tenant_x");
        assert_eq!(resp.requested, 3);
        assert_eq!(resp.terminated, 3);
        assert_eq!(resp.already_closed, 0);
        assert!(tokens.iter().all(|t| t.is_cancelled()));
    }

    #[test]
    fn terminate_all_empty_tenant_id() {
        let reg = make_registry();
        let svc = AdminControlService::new(&reg);
        let err = svc.terminate_all("", "admin", None).unwrap_err();
        assert!(matches!(err, ControlError::EmptyTenantId));
    }

    #[test]
    fn terminate_all_nonexistent_tenant() {
        let reg = make_registry();
        let svc = AdminControlService::new(&reg);
        let resp = svc.terminate_all("no_such_tenant", "admin", None).unwrap();
        assert_eq!(resp.requested, 0);
        assert_eq!(resp.terminated, 0);
        assert_eq!(resp.already_closed, 0);
    }

    #[test]
    fn has_more_exact_limit_no_false_positive() {
        // When result count == limit and there are no more rows,
        // has_more must be false (not a false positive).
        let reg = make_registry();
        for i in 0..5 {
            reg.register(make_info(i, "t", "user"));
        }
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 5,
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(resp.sessions.len(), 5);
        assert!(!resp.has_more, "has_more must be false when result count == limit with no extra rows");
    }

    #[test]
    fn has_more_exact_limit_plus_one() {
        // When there are limit+1 rows, has_more must be true and
        // only limit rows are returned.
        let reg = make_registry();
        for i in 0..6 {
            reg.register(make_info(i, "t", "user"));
        }
        let svc = AdminControlService::new(&reg);

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 5,
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(resp.sessions.len(), 5);
        assert!(resp.has_more, "has_more must be true when more rows exist beyond limit");
    }

    #[test]
    fn has_more_all_tenants_cap() {
        // When all_tenants=true and there are more sessions than the
        // ALL_TENANTS_MAX_LIMIT cap, has_more must be true and results
        // must be capped, even if the caller requested a larger limit.
        use crate::admin::session_registry::ALL_TENANTS_MAX_LIMIT;

        let reg = make_registry();
        let count = ALL_TENANTS_MAX_LIMIT + 1;
        for i in 0..count {
            reg.register(make_info(i as i64, &format!("t{}", i), "user"));
        }
        let svc = AdminControlService::new(&reg);

        // Caller asks for limit > cap.
        let filter = SessionFilter {
            all_tenants: true,
            limit: ALL_TENANTS_MAX_LIMIT + 500,
            ..Default::default()
        };
        let resp = svc.list_sessions(&filter).unwrap();
        assert_eq!(
            resp.sessions.len(),
            ALL_TENANTS_MAX_LIMIT,
            "results must be capped at ALL_TENANTS_MAX_LIMIT"
        );
        assert!(
            resp.has_more,
            "has_more must be true when more sessions exist beyond the all-tenants cap"
        );
    }
}
