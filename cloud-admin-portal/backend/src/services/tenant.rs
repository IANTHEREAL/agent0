use rand::Rng;
use sqlx::AnyPool;

use crate::db;
use crate::error::AppError;
use crate::services::fs9_client::Fs9Client;
use crate::services::pd_client::PdClient;
use crate::services::pg_client::PgClient;
use crate::{tenant_state, KEYSPACE_PREFIX, TENANT_ID_LEN};

#[derive(Clone, Copy)]
pub enum RollbackPolicy {
    PropagateDbError,
    BestEffort,
}

pub enum PrepareKeyspaceOutcome {
    Ready { created_at: String },
    Collision,
    PdCreateFailed,
    DbInsertFailed(sqlx::Error),
}

pub enum BootstrapPasswordOutcome {
    Success,
    PasswordBootstrapFailed,
}

pub enum DisableKeyspaceOutcome {
    Disabled,
    PdDisableFailed,
}

#[derive(Clone, Copy)]
pub struct AuditConfig<'a> {
    pub operation_type: &'a str,
    pub resource_type: &'a str,
    pub operator: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub struct ProvisionRequest<'a> {
    pub tenant_id: &'a str,
    pub keyspace: &'a str,
    pub admin_user: &'a str,
    pub current_admin_password: &'a str,
    pub desired_admin_password: &'a str,
}

#[derive(Clone, Copy)]
pub struct Fs9BootstrapConfig<'a> {
    pub fs9_client: &'a Fs9Client,
    pub token_owner: &'a str,
    pub credential_key: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub struct SchemaBootstrapConfig<'a> {
    pub sql: &'a str,
    pub failed_state_reason: &'a str,
    pub failed_error_prefix: &'a str,
}

#[derive(Clone, Copy)]
pub struct ProvisionConfig<'a> {
    pub pd_endpoints: &'a str,
    pub http_client: &'a reqwest::Client,
    pub pg_host: &'a str,
    pub pg_port: u16,
    pub rollback: RollbackPolicy,
    pub id_collision_message: &'a str,
    pub pd_create_failed_state_reason: &'static str,
    pub pd_create_failed_audit_reason: Option<&'a str>,
    pub pd_create_failed_error_message: &'a str,
    pub password_failed_state_reason: &'static str,
    pub password_failed_audit_reason: Option<&'a str>,
    pub password_failed_error_message: &'a str,
    pub audit: Option<AuditConfig<'a>>,
    pub bootstrap_default_extensions: bool,
    pub schema_bootstrap: Option<SchemaBootstrapConfig<'a>>,
    pub set_customer_id: Option<&'a str>,
    pub store_admin_credential: bool,
    pub credential_key: Option<&'a str>,
    pub metadata_notes: Option<&'a str>,
    pub metadata_tags_json: Option<&'a str>,
    pub fs9: Option<Fs9BootstrapConfig<'a>>,
    pub write_success_audit: bool,
}

#[derive(Debug)]
pub struct ProvisionOutcome {
    pub created_at: String,
}

#[derive(Clone, Copy)]
pub struct DeprovisionRequest<'a> {
    pub tenant_id: &'a str,
    pub keyspace: &'a str,
}

#[derive(Clone, Copy)]
pub struct DeprovisionConfig<'a> {
    pub pd_endpoints: &'a str,
    pub http_client: &'a reqwest::Client,
    pub rollback: RollbackPolicy,
    pub rollback_to_active_reason: &'static str,
    pub disabling_state_reason: Option<&'a str>,
    pub disable_failed_audit_reason: Option<&'a str>,
    pub disable_failed_error_message: &'a str,
    pub disabled_state_reason: &'a str,
    pub audit: Option<AuditConfig<'a>>,
    pub write_success_audit: bool,
}

pub fn generate_tenant_id() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..TENANT_ID_LEN)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

pub fn make_keyspace(id: &str) -> String {
    format!("{KEYSPACE_PREFIX}{id}")
}

pub fn generate_admin_password() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*";
    (0..16)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

pub fn generate_customer_password() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_=+.~";
    (0..16)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

pub async fn prepare_tenant_keyspace(
    db_pool: &AnyPool,
    pd: &PdClient,
    tenant_id: &str,
    keyspace: &str,
    pd_create_failed_reason: &'static str,
    rollback: RollbackPolicy,
) -> Result<PrepareKeyspaceOutcome, sqlx::Error> {
    if db::get_tenant_by_id(db_pool, tenant_id).await?.is_some() {
        return Ok(PrepareKeyspaceOutcome::Collision);
    }

    let created_at = chrono::Utc::now().to_rfc3339();
    if let Err(err) = db::insert_tenant(
        db_pool,
        tenant_id,
        keyspace,
        tenant_state::CREATING,
        &created_at,
    )
    .await
    {
        return Ok(PrepareKeyspaceOutcome::DbInsertFailed(err));
    }

    if !pd.create_keyspace(keyspace).await {
        match rollback {
            RollbackPolicy::PropagateDbError => {
                db::update_tenant_state(
                    db_pool,
                    tenant_id,
                    tenant_state::CREATE_FAILED,
                    Some(pd_create_failed_reason),
                )
                .await?;
            }
            RollbackPolicy::BestEffort => {
                db::update_tenant_state(
                    db_pool,
                    tenant_id,
                    tenant_state::CREATE_FAILED,
                    Some(pd_create_failed_reason),
                )
                .await
                .ok();
            }
        }

        return Ok(PrepareKeyspaceOutcome::PdCreateFailed);
    }

    Ok(PrepareKeyspaceOutcome::Ready { created_at })
}

#[allow(clippy::too_many_arguments)]
pub async fn bootstrap_tenant_password(
    pg: &PgClient,
    db_pool: &AnyPool,
    tenant_id: &str,
    admin_user: &str,
    current_password: &str,
    new_password: &str,
    password_failed_reason: &'static str,
    rollback: RollbackPolicy,
) -> Result<BootstrapPasswordOutcome, sqlx::Error> {
    if pg
        .bootstrap_admin_password(tenant_id, admin_user, current_password, new_password)
        .await
    {
        return Ok(BootstrapPasswordOutcome::Success);
    }

    match rollback {
        RollbackPolicy::PropagateDbError => {
            db::update_tenant_state(
                db_pool,
                tenant_id,
                tenant_state::CREATE_FAILED,
                Some(password_failed_reason),
            )
            .await?;
        }
        RollbackPolicy::BestEffort => {
            db::update_tenant_state(
                db_pool,
                tenant_id,
                tenant_state::CREATE_FAILED,
                Some(password_failed_reason),
            )
            .await
            .ok();
        }
    }

    Ok(BootstrapPasswordOutcome::PasswordBootstrapFailed)
}

pub async fn disable_keyspace_with_rollback(
    pd: &PdClient,
    db_pool: &AnyPool,
    tenant_id: &str,
    keyspace: &str,
    rollback_to_active_reason: &'static str,
    rollback: RollbackPolicy,
) -> Result<DisableKeyspaceOutcome, sqlx::Error> {
    if pd.disable_keyspace(keyspace).await {
        return Ok(DisableKeyspaceOutcome::Disabled);
    }

    match rollback {
        RollbackPolicy::PropagateDbError => {
            db::update_tenant_state(
                db_pool,
                tenant_id,
                tenant_state::ACTIVE,
                Some(rollback_to_active_reason),
            )
            .await?;
        }
        RollbackPolicy::BestEffort => {
            db::update_tenant_state(
                db_pool,
                tenant_id,
                tenant_state::ACTIVE,
                Some(rollback_to_active_reason),
            )
            .await
            .ok();
        }
    }

    Ok(DisableKeyspaceOutcome::PdDisableFailed)
}

async fn write_audit(
    db_pool: &AnyPool,
    audit: Option<AuditConfig<'_>>,
    resource_name: &str,
    success: bool,
    error_message: Option<&str>,
) {
    if let Some(audit_cfg) = audit {
        db::insert_audit_log(
            db_pool,
            audit_cfg.operation_type,
            audit_cfg.resource_type,
            resource_name,
            Some(resource_name),
            audit_cfg.operator,
            success,
            error_message,
            None,
        )
        .await
        .ok();
    }
}

async fn run_fs9_bootstrap(
    db_pool: &AnyPool,
    pd: &PdClient,
    tenant_id: &str,
    cfg: Fs9BootstrapConfig<'_>,
) -> Result<(), AppError> {
    let fs_keyspace = format!("db9_fs_{tenant_id}");
    if !pd.create_keyspace(&fs_keyspace).await {
        tracing::warn!(
            tenant_id = tenant_id,
            fs_keyspace = fs_keyspace.as_str(),
            "Failed to create fs keyspace in PD (non-fatal)"
        );
    }

    if let Err(error) = cfg.fs9_client.create_namespace(tenant_id).await {
        tracing::warn!(
            tenant_id = tenant_id,
            error = %error,
            "Failed to create fs9 namespace (non-fatal)"
        );
        return Ok(());
    }

    let fs9_user_id = match cfg.fs9_client.create_user(tenant_id, cfg.token_owner).await {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(
                tenant_id = tenant_id,
                error = %error,
                "Failed to create fs9 user (non-fatal)"
            );
            None
        }
    };

    if let Some(user_id) = fs9_user_id {
        match cfg.fs9_client.generate_token(&user_id, tenant_id).await {
            Ok(token) => {
                db::upsert_credential(
                    db_pool,
                    tenant_id,
                    "fs9_token",
                    cfg.token_owner,
                    &token,
                    cfg.credential_key,
                )
                .await?;
            }
            Err(error) => {
                tracing::warn!(
                    tenant_id = tenant_id,
                    error = %error,
                    "Failed to generate fs9 token (non-fatal)"
                );
            }
        }
    }

    Ok(())
}

pub async fn provision_tenant(
    db_pool: &AnyPool,
    req: &ProvisionRequest<'_>,
    cfg: ProvisionConfig<'_>,
) -> Result<ProvisionOutcome, AppError> {
    let pd = PdClient::new(cfg.pd_endpoints, cfg.http_client);
    let created_at = match prepare_tenant_keyspace(
        db_pool,
        &pd,
        req.tenant_id,
        req.keyspace,
        cfg.pd_create_failed_state_reason,
        cfg.rollback,
    )
    .await?
    {
        PrepareKeyspaceOutcome::Ready { created_at } => created_at,
        PrepareKeyspaceOutcome::Collision => {
            return Err(AppError::conflict(cfg.id_collision_message));
        }
        PrepareKeyspaceOutcome::PdCreateFailed => {
            if let Some(reason) = cfg.pd_create_failed_audit_reason {
                write_audit(db_pool, cfg.audit, req.tenant_id, false, Some(reason)).await;
            }
            return Err(AppError::internal(cfg.pd_create_failed_error_message));
        }
        PrepareKeyspaceOutcome::DbInsertFailed(err) => return Err(err.into()),
    };

    if let Some(fs9_cfg) = cfg.fs9 {
        run_fs9_bootstrap(db_pool, &pd, req.tenant_id, fs9_cfg).await?;
    }

    let pg = PgClient::new(cfg.pg_host, cfg.pg_port);
    match bootstrap_tenant_password(
        &pg,
        db_pool,
        req.tenant_id,
        req.admin_user,
        req.current_admin_password,
        req.desired_admin_password,
        cfg.password_failed_state_reason,
        cfg.rollback,
    )
    .await?
    {
        BootstrapPasswordOutcome::Success => {}
        BootstrapPasswordOutcome::PasswordBootstrapFailed => {
            if let Some(reason) = cfg.password_failed_audit_reason {
                write_audit(db_pool, cfg.audit, req.tenant_id, false, Some(reason)).await;
            }
            return Err(AppError::internal(cfg.password_failed_error_message));
        }
    }

    if let Some(schema_bootstrap) = cfg.schema_bootstrap {
        if !schema_bootstrap.sql.is_empty() {
            if let Err(error) = pg
                .run_sql_structured(
                    req.tenant_id,
                    req.admin_user,
                    req.desired_admin_password,
                    schema_bootstrap.sql,
                )
                .await
            {
                db::update_tenant_state(
                    db_pool,
                    req.tenant_id,
                    tenant_state::CREATE_FAILED,
                    Some(schema_bootstrap.failed_state_reason),
                )
                .await?;
                return Err(AppError::bad_gateway(format!(
                    "{}: {}",
                    schema_bootstrap.failed_error_prefix, error
                )));
            }
        }
    }

    if cfg.bootstrap_default_extensions {
        pg.bootstrap_default_extensions(req.tenant_id, req.admin_user, req.desired_admin_password)
            .await;
    }

    db::update_tenant_state(db_pool, req.tenant_id, tenant_state::ACTIVE, None).await?;

    if let Some(customer_id) = cfg.set_customer_id {
        db::set_tenant_customer_id(db_pool, req.tenant_id, customer_id).await?;
    }

    if cfg.store_admin_credential {
        db::upsert_credential(
            db_pool,
            req.tenant_id,
            "admin",
            req.admin_user,
            req.desired_admin_password,
            cfg.credential_key,
        )
        .await?;
    }

    if cfg.metadata_notes.is_some() || cfg.metadata_tags_json.is_some() {
        db::update_tenant_metadata(
            db_pool,
            req.tenant_id,
            cfg.metadata_notes,
            cfg.metadata_tags_json,
        )
        .await?;
    }

    if cfg.write_success_audit {
        write_audit(db_pool, cfg.audit, req.tenant_id, true, None).await;
    }

    Ok(ProvisionOutcome { created_at })
}

pub async fn provision_database(
    db_pool: &AnyPool,
    req: &ProvisionRequest<'_>,
    cfg: ProvisionConfig<'_>,
) -> Result<ProvisionOutcome, AppError> {
    provision_tenant(db_pool, req, cfg).await
}

pub async fn deprovision_tenant(
    db_pool: &AnyPool,
    req: &DeprovisionRequest<'_>,
    cfg: DeprovisionConfig<'_>,
) -> Result<(), AppError> {
    db::update_tenant_state(
        db_pool,
        req.tenant_id,
        tenant_state::DISABLING,
        cfg.disabling_state_reason,
    )
    .await?;

    let pd = PdClient::new(cfg.pd_endpoints, cfg.http_client);
    match disable_keyspace_with_rollback(
        &pd,
        db_pool,
        req.tenant_id,
        req.keyspace,
        cfg.rollback_to_active_reason,
        cfg.rollback,
    )
    .await?
    {
        DisableKeyspaceOutcome::Disabled => {}
        DisableKeyspaceOutcome::PdDisableFailed => {
            if let Some(reason) = cfg.disable_failed_audit_reason {
                write_audit(db_pool, cfg.audit, req.tenant_id, false, Some(reason)).await;
            }
            return Err(AppError::internal(cfg.disable_failed_error_message));
        }
    }

    db::update_tenant_state(
        db_pool,
        req.tenant_id,
        tenant_state::DISABLED,
        Some(cfg.disabled_state_reason),
    )
    .await?;

    if cfg.write_success_audit {
        write_audit(db_pool, cfg.audit, req.tenant_id, true, None).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{post, put};
    use axum::Router;
    use sqlx::Row;

    async fn test_pool() -> AnyPool {
        let path = format!("/tmp/db9-tenant-service-test-{}.db", uuid::Uuid::new_v4());
        let db_url = format!("sqlite://{path}?mode=rwc");
        let pool = db::connect(&db_url).await.unwrap();
        db::create_tables(&pool).await.unwrap();
        pool
    }

    #[derive(Clone, Copy)]
    struct PdMockState {
        create_ok: bool,
        disable_ok: bool,
    }

    async fn pd_create(State(state): State<PdMockState>) -> StatusCode {
        if state.create_ok {
            StatusCode::OK
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }

    async fn pd_disable(Path(_name): Path<String>, State(state): State<PdMockState>) -> StatusCode {
        if state.disable_ok {
            StatusCode::OK
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }

    async fn spawn_pd_mock(state: PdMockState) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/pd/api/v2/keyspaces", post(pd_create))
            .route("/pd/api/v2/keyspaces/:name/state", put(pd_disable))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn prepare_keyspace_pd_failure_marks_create_failed() {
        let pool = test_pool().await;
        let http_client = reqwest::Client::new();
        let (pd_endpoint, server) = spawn_pd_mock(PdMockState {
            create_ok: false,
            disable_ok: true,
        })
        .await;

        let tenant_id = "tpdcreatefail01";
        let keyspace = make_keyspace(tenant_id);
        let pd = PdClient::new(&pd_endpoint, &http_client);

        let outcome = prepare_tenant_keyspace(
            &pool,
            &pd,
            tenant_id,
            &keyspace,
            "PD create failed",
            RollbackPolicy::PropagateDbError,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, PrepareKeyspaceOutcome::PdCreateFailed));
        let tenant = db::get_tenant_by_id(&pool, tenant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tenant.state, tenant_state::CREATE_FAILED);
        assert_eq!(tenant.state_reason.as_deref(), Some("PD create failed"));

        server.abort();
    }

    #[tokio::test]
    async fn bootstrap_password_failure_marks_create_failed() {
        let pool = test_pool().await;
        let tenant_id = "tbootstrapfail";
        let keyspace = make_keyspace(tenant_id);
        let now = chrono::Utc::now().to_rfc3339();
        db::insert_tenant(&pool, tenant_id, &keyspace, tenant_state::CREATING, &now)
            .await
            .unwrap();

        let pg = PgClient::new("127.0.0.1", 1);
        let outcome = bootstrap_tenant_password(
            &pg,
            &pool,
            tenant_id,
            "admin",
            "admin",
            "new-password",
            "password bootstrap failed",
            RollbackPolicy::PropagateDbError,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            BootstrapPasswordOutcome::PasswordBootstrapFailed
        ));
        let tenant = db::get_tenant_by_id(&pool, tenant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tenant.state, tenant_state::CREATE_FAILED);
        assert_eq!(
            tenant.state_reason.as_deref(),
            Some("password bootstrap failed")
        );
    }

    #[tokio::test]
    async fn provision_tenant_password_failure_updates_state_and_audit() {
        let pool = test_pool().await;
        let http_client = reqwest::Client::new();
        let (pd_endpoint, server) = spawn_pd_mock(PdMockState {
            create_ok: true,
            disable_ok: true,
        })
        .await;

        let tenant_id = "tprovpwdfail1";
        let keyspace = make_keyspace(tenant_id);
        let error = provision_tenant(
            &pool,
            &ProvisionRequest {
                tenant_id,
                keyspace: &keyspace,
                admin_user: "admin",
                current_admin_password: "admin",
                desired_admin_password: "new-password",
            },
            ProvisionConfig {
                pd_endpoints: &pd_endpoint,
                http_client: &http_client,
                pg_host: "127.0.0.1",
                pg_port: 1,
                rollback: RollbackPolicy::PropagateDbError,
                id_collision_message: "ID collision, please retry",
                pd_create_failed_state_reason: "Failed to create keyspace in PD",
                pd_create_failed_audit_reason: Some("Failed to create keyspace in PD"),
                pd_create_failed_error_message: "Failed to create keyspace in TiKV",
                password_failed_state_reason: "Keyspace created but password bootstrap failed",
                password_failed_audit_reason: Some("Password bootstrap failed"),
                password_failed_error_message:
                    "Failed to set admin password. Keyspace created but password unchanged.",
                audit: Some(AuditConfig {
                    operation_type: "CREATE",
                    resource_type: "TENANT",
                    operator: None,
                }),
                bootstrap_default_extensions: true,
                schema_bootstrap: None,
                set_customer_id: None,
                store_admin_credential: false,
                credential_key: None,
                metadata_notes: None,
                metadata_tags_json: None,
                fs9: None,
                write_success_audit: true,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.message,
            "Failed to set admin password. Keyspace created but password unchanged."
        );
        let tenant = db::get_tenant_by_id(&pool, tenant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tenant.state, tenant_state::CREATE_FAILED);
        assert_eq!(
            tenant.state_reason.as_deref(),
            Some("Keyspace created but password bootstrap failed")
        );

        let logs = db::query_audit_logs(
            &pool,
            Some(tenant_id),
            Some("CREATE"),
            Some("TENANT"),
            Some(false),
            20,
            0,
        )
        .await
        .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].error_message.as_deref(),
            Some("Password bootstrap failed")
        );

        server.abort();
    }

    #[tokio::test]
    async fn deprovision_tenant_failure_rolls_back_and_logs_audit() {
        let pool = test_pool().await;
        let tenant_id = "tdeprovfail01";
        let keyspace = make_keyspace(tenant_id);
        let now = chrono::Utc::now().to_rfc3339();
        db::insert_tenant(&pool, tenant_id, &keyspace, tenant_state::ACTIVE, &now)
            .await
            .unwrap();

        let http_client = reqwest::Client::new();
        let error = deprovision_tenant(
            &pool,
            &DeprovisionRequest {
                tenant_id,
                keyspace: &keyspace,
            },
            DeprovisionConfig {
                pd_endpoints: "127.0.0.1:1",
                http_client: &http_client,
                rollback: RollbackPolicy::PropagateDbError,
                rollback_to_active_reason: "Failed to disable keyspace in PD",
                disabling_state_reason: None,
                disable_failed_audit_reason: Some("Failed to disable keyspace"),
                disable_failed_error_message: "Failed to disable tenant",
                disabled_state_reason: "Deleted via API",
                audit: Some(AuditConfig {
                    operation_type: "DELETE",
                    resource_type: "TENANT",
                    operator: None,
                }),
                write_success_audit: true,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.message, "Failed to disable tenant");

        let tenant = db::get_tenant_by_id(&pool, tenant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tenant.state, tenant_state::ACTIVE);
        assert_eq!(
            tenant.state_reason.as_deref(),
            Some("Failed to disable keyspace in PD")
        );

        let logs = db::query_audit_logs(
            &pool,
            Some(tenant_id),
            Some("DELETE"),
            Some("TENANT"),
            Some(false),
            20,
            0,
        )
        .await
        .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].error_message.as_deref(),
            Some("Failed to disable keyspace")
        );
    }

    #[tokio::test]
    async fn deprovision_tenant_success_disables_and_logs_audit() {
        let pool = test_pool().await;
        let tenant_id = "tdeprovok0001";
        let keyspace = make_keyspace(tenant_id);
        let now = chrono::Utc::now().to_rfc3339();
        db::insert_tenant(&pool, tenant_id, &keyspace, tenant_state::ACTIVE, &now)
            .await
            .unwrap();

        let http_client = reqwest::Client::new();
        let (pd_endpoint, server) = spawn_pd_mock(PdMockState {
            create_ok: true,
            disable_ok: true,
        })
        .await;

        deprovision_tenant(
            &pool,
            &DeprovisionRequest {
                tenant_id,
                keyspace: &keyspace,
            },
            DeprovisionConfig {
                pd_endpoints: &pd_endpoint,
                http_client: &http_client,
                rollback: RollbackPolicy::PropagateDbError,
                rollback_to_active_reason: "Failed to disable keyspace in PD",
                disabling_state_reason: None,
                disable_failed_audit_reason: Some("Failed to disable keyspace"),
                disable_failed_error_message: "Failed to disable tenant",
                disabled_state_reason: "Deleted via API",
                audit: Some(AuditConfig {
                    operation_type: "DELETE",
                    resource_type: "TENANT",
                    operator: None,
                }),
                write_success_audit: true,
            },
        )
        .await
        .unwrap();

        let tenant = db::get_tenant_by_id(&pool, tenant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tenant.state, tenant_state::DISABLED);
        assert_eq!(tenant.state_reason.as_deref(), Some("Deleted via API"));

        let logs = db::query_audit_logs(
            &pool,
            Some(tenant_id),
            Some("DELETE"),
            Some("TENANT"),
            Some(true),
            20,
            0,
        )
        .await
        .unwrap();
        assert_eq!(logs.len(), 1);

        let sql = db::adapt_sql(
            "SELECT COUNT(*) AS cnt FROM audit_logs WHERE tenant_id = $1",
            &pool,
        );
        let count: i64 = sqlx::query(&sql)
            .bind(tenant_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("cnt");
        assert_eq!(count, 1);

        server.abort();
    }
}
