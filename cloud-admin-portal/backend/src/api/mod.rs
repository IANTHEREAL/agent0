pub mod audit;
pub mod customer;
pub mod fs9_proxy;
pub mod system;
pub mod tenants;
pub mod users;

use axum::routing::{delete, get, post};
use axum::Router;

use crate::AppState;

pub fn router() -> Router<AppState> {
    let customer_router = customer::router();

    Router::new()
        // Tenants
        .route(
            "/tenants",
            get(tenants::list_tenants).post(tenants::create_tenant),
        )
        .route("/tenants/batch", post(tenants::batch_create_tenants))
        .route("/tenants/batch-delete", post(tenants::batch_delete_tenants))
        .route("/tenants/batch-update", post(tenants::batch_update_tenants))
        .route(
            "/tenants/:tenant_id",
            get(tenants::get_tenant)
                .delete(tenants::delete_tenant)
                .put(tenants::update_tenant),
        )
        .route("/tenants/:tenant_id/remove", post(tenants::remove_tenant))
        .route("/tenants/:tenant_id/connect", post(tenants::connect_tenant))
        .route("/tenants/:tenant_id/query", post(tenants::execute_query))
        .route(
            "/tenants/:tenant_id/observability",
            get(tenants::get_observability),
        )
        .route(
            "/tenants/:tenant_id/observability/bootstrap",
            post(tenants::bootstrap_observability),
        )
        // Users
        .route(
            "/tenants/:tenant_id/users",
            get(users::list_users).post(users::create_user),
        )
        .route(
            "/tenants/:tenant_id/users/:username",
            delete(users::delete_user),
        )
        .route(
            "/tenants/:tenant_id/users/:username/password",
            post(users::reset_password),
        )
        // System
        .route("/health", get(system::health_check))
        .route("/info", get(system::api_info))
        // Audit
        .route("/audit-logs", get(audit::query_audit_logs))
        // Credential migration
        .route(
            "/admin/credential-migration-status",
            get(system::credential_migration_status),
        )
        .route(
            "/admin/migrate-credentials",
            post(system::migrate_credentials),
        )
        .nest("/customer", customer_router)
}
