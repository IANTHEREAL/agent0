use crate::config;
use crate::sql::error::SqlError;
use crate::txn::{txn_delete, txn_put};
use anyhow::{anyhow, Result};
use dashmap::DashSet;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;
use tikv_client::Transaction;

/// Per-keyspace cache tracking which keyspaces have been initialized (have at
/// least one superuser).  Once a keyspace is inserted it stays until explicitly
/// invalidated via [`invalidate_initialized`] (called on DROP ROLE / ALTER ROLE
/// NOSUPERUSER) so that the bootstrap probe re-runs if all superusers are removed.
static INITIALIZED_KEYSPACES: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);

/// Remove `keyspace` from the initialized cache so the next
/// [`AuthManager::is_initialized`] call re-probes TiKV.
pub fn invalidate_initialized(keyspace: &str) {
    INITIALIZED_KEYSPACES.remove(keyspace);
}

const USER_KEY_PREFIX: &[u8] = b"_sys_user_";
const ROLE_KEY_PREFIX: &[u8] = b"_sys_role_";
const BOOTSTRAP_USER_KEY: &[u8] = b"_sys_bootstrap_user";
const DEFAULT_ADMIN_USER: &str = "admin";
const SCAN_LIMIT: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Privilege {
    SuperUser,
    CreateDB,
    CreateRole,
    CreateTable,
    DropTable,
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Connect,
    Temporary,
    Execute,
    Usage,
    All,
}

impl Privilege {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ALL" | "ALL PRIVILEGES" => Some(Privilege::All),
            "SELECT" => Some(Privilege::Select),
            "INSERT" => Some(Privilege::Insert),
            "UPDATE" => Some(Privilege::Update),
            "DELETE" => Some(Privilege::Delete),
            "TRUNCATE" => Some(Privilege::Truncate),
            "REFERENCES" => Some(Privilege::References),
            "TRIGGER" => Some(Privilege::Trigger),
            "CREATE" => Some(Privilege::CreateTable),
            "CONNECT" => Some(Privilege::Connect),
            "TEMPORARY" | "TEMP" => Some(Privilege::Temporary),
            "EXECUTE" => Some(Privilege::Execute),
            "USAGE" => Some(Privilege::Usage),
            "SUPERUSER" => Some(Privilege::SuperUser),
            "CREATEDB" => Some(Privilege::CreateDB),
            "CREATEROLE" => Some(Privilege::CreateRole),
            _ => None,
        }
    }

    pub fn expand_all() -> HashSet<Privilege> {
        let mut set = HashSet::new();
        set.insert(Privilege::Select);
        set.insert(Privilege::Insert);
        set.insert(Privilege::Update);
        set.insert(Privilege::Delete);
        set.insert(Privilege::Truncate);
        set.insert(Privilege::References);
        set.insert(Privilege::Trigger);
        set
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PrivilegeObject {
    Database(String),
    AllTablesInSchema(String),
    Table { schema: String, name: String },
    AllSequencesInSchema(String),
    Sequence { schema: String, name: String },
    Schema(String),
    Global,
}

#[allow(dead_code)] // forward-compat: RBAC enforcement planned
impl PrivilegeObject {
    pub fn table(name: &str) -> Self {
        PrivilegeObject::Table {
            schema: "public".to_string(),
            name: name.to_string(),
        }
    }

    pub fn all_tables() -> Self {
        PrivilegeObject::AllTablesInSchema("public".to_string())
    }

    pub fn database(name: &str) -> Self {
        PrivilegeObject::Database(name.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantedPrivilege {
    pub privilege: Privilege,
    pub object: PrivilegeObject,
    pub with_grant_option: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub password_hash: String,
    pub password_salt: String,
    pub roles: HashSet<String>,
    pub privileges: Vec<GrantedPrivilege>,
    pub is_superuser: bool,
    pub can_login: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    pub connection_limit: i32,
    pub valid_until: Option<i64>,
    #[serde(default)]
    pub bypass_rls: bool,
}

/// Legacy User struct without the `bypass_rls` field, for backward-compatible
/// bincode deserialization of data written before the RLS BYPASSRLS feature.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct UserLegacy {
    name: String,
    password_hash: String,
    password_salt: String,
    roles: HashSet<String>,
    privileges: Vec<GrantedPrivilege>,
    is_superuser: bool,
    can_login: bool,
    can_create_db: bool,
    can_create_role: bool,
    connection_limit: i32,
    valid_until: Option<i64>,
}

impl From<UserLegacy> for User {
    fn from(legacy: UserLegacy) -> Self {
        Self {
            name: legacy.name,
            password_hash: legacy.password_hash,
            password_salt: legacy.password_salt,
            roles: legacy.roles,
            privileges: legacy.privileges,
            is_superuser: legacy.is_superuser,
            can_login: legacy.can_login,
            can_create_db: legacy.can_create_db,
            can_create_role: legacy.can_create_role,
            connection_limit: legacy.connection_limit,
            valid_until: legacy.valid_until,
            bypass_rls: false,
        }
    }
}

/// Deserialize a User from bincode, falling back to the legacy format (without
/// `bypass_rls`) if the data was written before the BYPASSRLS feature.
fn deserialize_user(data: &[u8]) -> Result<User> {
    match bincode::deserialize::<User>(data) {
        Ok(user) => Ok(user),
        Err(_) => Ok(bincode::deserialize::<UserLegacy>(data)?.into()),
    }
}

impl User {
    pub fn new(name: &str, password: &str) -> Self {
        let salt = super::password::generate_salt();
        let hash = super::password::hash_password(password, &salt);
        Self {
            name: name.to_string(),
            password_hash: hash,
            password_salt: salt,
            roles: HashSet::new(),
            privileges: Vec::new(),
            is_superuser: false,
            can_login: true,
            can_create_db: false,
            can_create_role: false,
            connection_limit: -1,
            valid_until: None,
            bypass_rls: false,
        }
    }

    pub fn new_superuser(name: &str, password: &str) -> Self {
        let mut user = Self::new(name, password);
        user.is_superuser = true;
        user.can_create_db = true;
        user.can_create_role = true;
        user
    }

    pub fn verify_password(&self, password: &str) -> bool {
        super::password::verify_password(password, &self.password_salt, &self.password_hash)
    }

    pub fn set_password(&mut self, password: &str) {
        self.password_salt = super::password::generate_salt();
        self.password_hash = super::password::hash_password(password, &self.password_salt);
    }

    pub fn grant_privilege(
        &mut self,
        privilege: Privilege,
        object: PrivilegeObject,
        with_grant_option: bool,
    ) {
        self.privileges
            .retain(|p| !(p.privilege == privilege && p.object == object));
        self.privileges.push(GrantedPrivilege {
            privilege,
            object,
            with_grant_option,
        });
    }

    pub fn revoke_privilege(&mut self, privilege: &Privilege, object: &PrivilegeObject) {
        self.privileges
            .retain(|p| !(&p.privilege == privilege && &p.object == object));
    }

    #[allow(dead_code)] // forward-compat: RBAC enforcement planned
    pub fn has_privilege(&self, privilege: &Privilege, object: &PrivilegeObject) -> bool {
        if self.is_superuser {
            return true;
        }

        self.privileges
            .iter()
            .any(|granted| Self::granted_matches(granted, privilege, object, false))
    }

    pub fn has_privilege_with_grant_option(
        &self,
        privilege: &Privilege,
        object: &PrivilegeObject,
    ) -> bool {
        if self.is_superuser {
            return true;
        }

        self.privileges
            .iter()
            .any(|granted| Self::granted_matches(granted, privilege, object, true))
    }

    fn privilege_matches(granted: &Privilege, required: &Privilege) -> bool {
        if granted == &Privilege::All {
            return true;
        }
        granted == required
    }

    fn object_matches(granted: &PrivilegeObject, required: &PrivilegeObject) -> bool {
        if granted == &PrivilegeObject::Global {
            return true;
        }

        match (granted, required) {
            (PrivilegeObject::AllTablesInSchema(gs), PrivilegeObject::Table { schema, .. }) => {
                gs == schema
            }
            (PrivilegeObject::AllTablesInSchema(gs), PrivilegeObject::AllTablesInSchema(rs)) => {
                gs == rs
            }
            _ => granted == required,
        }
    }

    fn granted_matches(
        granted: &GrantedPrivilege,
        required_privilege: &Privilege,
        required_object: &PrivilegeObject,
        require_grant_option: bool,
    ) -> bool {
        if require_grant_option && !granted.with_grant_option {
            return false;
        }
        Self::privilege_matches(&granted.privilege, required_privilege)
            && Self::object_matches(&granted.object, required_object)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub privileges: Vec<GrantedPrivilege>,
    pub member_of: HashSet<String>,
    pub is_superuser: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    #[serde(default)]
    pub bypass_rls: bool,
}

/// Legacy Role struct without `bypass_rls`.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct RoleLegacy {
    name: String,
    privileges: Vec<GrantedPrivilege>,
    member_of: HashSet<String>,
    is_superuser: bool,
    can_create_db: bool,
    can_create_role: bool,
}

impl From<RoleLegacy> for Role {
    fn from(legacy: RoleLegacy) -> Self {
        Self {
            name: legacy.name,
            privileges: legacy.privileges,
            member_of: legacy.member_of,
            is_superuser: legacy.is_superuser,
            can_create_db: legacy.can_create_db,
            can_create_role: legacy.can_create_role,
            bypass_rls: false,
        }
    }
}

/// Deserialize a Role from bincode, falling back to the legacy format.
fn deserialize_role(data: &[u8]) -> Result<Role> {
    match bincode::deserialize::<Role>(data) {
        Ok(role) => Ok(role),
        Err(_) => Ok(bincode::deserialize::<RoleLegacy>(data)?.into()),
    }
}

impl Role {
    #[allow(dead_code)] // forward-compat: RBAC enforcement planned
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            privileges: Vec::new(),
            member_of: HashSet::new(),
            is_superuser: false,
            can_create_db: false,
            can_create_role: false,
            bypass_rls: false,
        }
    }
}

pub struct AuthManager;

impl AuthManager {
    pub fn new() -> Self {
        Self
    }

    fn user_key(&self, username: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(USER_KEY_PREFIX);
        key.extend_from_slice(username.as_bytes());
        key
    }

    fn role_key(&self, rolename: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(ROLE_KEY_PREFIX);
        key.extend_from_slice(rolename.as_bytes());
        key
    }

    fn bootstrap_user_key(&self) -> Vec<u8> {
        BOOTSTRAP_USER_KEY.to_vec()
    }

    /// Read-only probe: returns `true` if at least one superuser exists.
    ///
    /// Results are cached per keyspace — once `true` for a given keyspace,
    /// subsequent calls return immediately without a TiKV transaction.
    /// The cache is invalidated by [`invalidate_initialized`] on
    /// DROP ROLE / ALTER ROLE NOSUPERUSER so re-bootstrap can trigger.
    pub async fn is_initialized(&self, store: &crate::storage::TikvStore) -> Result<bool> {
        let ks = store.keyspace().unwrap_or("").to_string();
        if INITIALIZED_KEYSPACES.contains(&ks) {
            return Ok(true);
        }
        let mut txn = store.begin_optimistic().await?;
        let result = self.has_any_superuser(&mut txn).await;
        if let Err(err) = txn.rollback().await {
            return Err(err.into());
        }
        let result = result?;
        if result {
            INITIALIZED_KEYSPACES.insert(ks);
        }
        Ok(result)
    }

    pub async fn bootstrap(&self, txn: &mut Transaction) -> Result<()> {
        if self.has_any_superuser(txn).await? {
            return Ok(());
        }

        let (username, password) = Self::resolve_bootstrap_credentials()?;

        if Self::validate_existing_bootstrap_user(
            self.get_user(txn, &username).await?.as_ref(),
            &username,
        )? {
            return Ok(());
        }

        let admin = User::new_superuser(&username, &password);
        self.create_user(txn, admin).await?;
        self.persist_bootstrap_user(txn, &username).await?;
        if config::env_bool("DB9_DEV") {
            tracing::warn!(
                "DB9_DEV=1: bootstrapped dev superuser '{}' (password from DB9_DEV_ADMIN_PASSWORD)",
                username
            );
        } else {
            tracing::info!("Bootstrapped initial superuser '{}'", username);
        }
        Ok(())
    }

    pub async fn get_bootstrap_user(
        &self,
        txn: &mut Transaction,
        connection_user: Option<&str>,
    ) -> Result<Option<String>> {
        self.ensure_bootstrap_user_key(txn, connection_user).await
    }

    pub async fn persist_bootstrap_user(
        &self,
        txn: &mut Transaction,
        username: &str,
    ) -> Result<()> {
        txn_put(txn, self.bootstrap_user_key(), username.as_bytes().to_vec()).await
    }

    pub async fn ensure_bootstrap_user_key(
        &self,
        txn: &mut Transaction,
        _connection_user: Option<&str>,
    ) -> Result<Option<String>> {
        let key = self.bootstrap_user_key();
        let existing_bootstrap = txn.get(key.clone()).await?;
        let superusers = if existing_bootstrap.is_none() {
            self.find_superuser_names(txn).await?
        } else {
            Vec::new()
        };

        let (bootstrap_user, should_persist, ambiguous_superusers) =
            Self::resolve_bootstrap_user_for_backfill(existing_bootstrap, &superusers)?;
        if let Some(superusers) = ambiguous_superusers {
            tracing::error!(
                "Multiple superusers found without bootstrap marker. Superusers={:?}. Protections are disabled until the marker is set. Set explicitly: INSERT INTO system._sys_bootstrap_user VALUES ('<role>')",
                superusers
            );
        }
        if should_persist {
            if let Some(user) = bootstrap_user.as_ref() {
                txn_put(txn, key, user.as_bytes().to_vec()).await?;
            }
        }
        Ok(bootstrap_user)
    }

    async fn find_superuser_names(&self, txn: &mut Transaction) -> Result<Vec<String>> {
        let users = self.list_users(txn).await?;
        Ok(Self::superuser_names(users.iter()))
    }

    fn resolve_bootstrap_user_for_backfill(
        existing_bootstrap: Option<Vec<u8>>,
        superusers: &[String],
    ) -> Result<(Option<String>, bool, Option<Vec<String>>)> {
        // Backfill contract:
        // - Existing marker always wins.
        // - Missing marker + exactly one superuser is unambiguous and auto-backfilled.
        // - Missing marker + multiple superusers is ambiguous; fail fast by disabling
        //   protections until an explicit marker is manually set.
        if let Some(user) = Self::decode_bootstrap_user(existing_bootstrap)? {
            return Ok((Some(user), false, None));
        }

        match superusers {
            [] => Ok((None, false, None)),
            [single] => Ok((Some(single.clone()), true, None)),
            many => {
                let mut superuser_names = many.to_vec();
                superuser_names.sort_unstable();
                Ok((None, false, Some(superuser_names)))
            }
        }
    }

    fn decode_bootstrap_user(value: Option<Vec<u8>>) -> Result<Option<String>> {
        match value {
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| anyhow!("invalid UTF-8 value in _sys_bootstrap_user")),
            None => Ok(None),
        }
    }

    fn superuser_names<'a>(users: impl IntoIterator<Item = &'a User>) -> Vec<String> {
        users
            .into_iter()
            .filter(|user| user.is_superuser)
            .map(|user| user.name.clone())
            .collect()
    }

    /// Resolves bootstrap credentials from environment variables.
    /// Returns `(username, password)` on success.
    ///
    /// In DB9_DEV mode: requires DB9_DEV_ADMIN_PASSWORD (rejects hardcoded defaults).
    /// In production mode: requires DB9_BOOTSTRAP_ADMIN_PASSWORD.
    fn resolve_bootstrap_credentials() -> Result<(String, String)> {
        if config::env_bool("DB9_DEV") {
            let dev_password =
                config::env_string("DB9_DEV_ADMIN_PASSWORD").ok_or_else(|| {
                    SqlError::InvalidAuthorizationSpecification {
                        message: "DB9_DEV=1 requires DB9_DEV_ADMIN_PASSWORD to be set. Hardcoded credentials are not allowed.".into(),
                    }
                })?;
            return Ok((DEFAULT_ADMIN_USER.to_string(), dev_password));
        }

        let bootstrap_user = config::env_string("DB9_BOOTSTRAP_ADMIN_USER")
            .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());
        let bootstrap_password =
            config::env_string("DB9_BOOTSTRAP_ADMIN_PASSWORD").ok_or_else(|| {
                SqlError::InvalidAuthorizationSpecification {
                    message: "No superuser exists yet. Set DB9_BOOTSTRAP_ADMIN_PASSWORD to bootstrap the initial superuser (optionally DB9_BOOTSTRAP_ADMIN_USER), or set DB9_DEV=1 for local development.".into(),
                }
            })?;

        Ok((bootstrap_user, bootstrap_password))
    }

    /// Checks whether an existing user is compatible with bootstrap.
    /// Returns `Ok(true)` if the user is already a superuser (skip creation),
    /// `Ok(false)` if no user exists (proceed with creation),
    /// or `Err` if the user exists but is not a superuser.
    fn validate_existing_bootstrap_user(existing: Option<&User>, username: &str) -> Result<bool> {
        match existing {
            Some(user) if user.is_superuser => Ok(true),
            Some(_) => Err(SqlError::InvalidAuthorizationSpecification {
                message: format!(
                    "Bootstrap user '{}' already exists but is not a superuser",
                    username
                ),
            }
            .into()),
            None => Ok(false),
        }
    }

    async fn has_any_superuser(&self, txn: &mut Transaction) -> Result<bool> {
        let prefix = USER_KEY_PREFIX.to_vec();
        let mut end = prefix.clone();
        end.push(0xFF);

        let range: tikv_client::BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        for pair in pairs {
            let user: User = deserialize_user(pair.value())?;
            if user.is_superuser {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn create_user(&self, txn: &mut Transaction, user: User) -> Result<()> {
        let key = self.user_key(&user.name);
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("User '{}' already exists", user.name));
        }
        let data = bincode::serialize(&user)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_user(&self, txn: &mut Transaction, username: &str) -> Result<Option<User>> {
        let key = self.user_key(username);
        match txn.get(key).await? {
            Some(data) => Ok(Some(deserialize_user(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn update_user(&self, txn: &mut Transaction, user: User) -> Result<()> {
        let key = self.user_key(&user.name);
        let data = bincode::serialize(&user)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_user(&self, txn: &mut Transaction, username: &str) -> Result<bool> {
        let key = self.user_key(username);
        if txn.get(key.clone()).await?.is_none() {
            return Ok(false);
        }
        txn_delete(txn, key).await?;
        Ok(true)
    }

    pub async fn authenticate(
        &self,
        txn: &mut Transaction,
        username: &str,
        password: &str,
    ) -> Result<Option<User>> {
        match self.get_user(txn, username).await? {
            Some(user) => {
                if !user.can_login {
                    return Err(SqlError::InvalidAuthorizationSpecification {
                        message: format!("role \"{}\" is not permitted to log in", username),
                    }
                    .into());
                }
                if user.verify_password(password) {
                    Ok(Some(user))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    #[allow(dead_code)] // forward-compat: RBAC enforcement planned
    pub async fn create_role(&self, txn: &mut Transaction, role: Role) -> Result<()> {
        let key = self.role_key(&role.name);
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Role '{}' already exists", role.name));
        }
        let data = bincode::serialize(&role)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_role(&self, txn: &mut Transaction, rolename: &str) -> Result<Option<Role>> {
        let key = self.role_key(rolename);
        match txn.get(key).await? {
            Some(data) => Ok(Some(deserialize_role(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn update_role(&self, txn: &mut Transaction, role: Role) -> Result<()> {
        let key = self.role_key(&role.name);
        let data = bincode::serialize(&role)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_role(&self, txn: &mut Transaction, rolename: &str) -> Result<bool> {
        let key = self.role_key(rolename);
        if txn.get(key.clone()).await?.is_none() {
            return Ok(false);
        }
        txn_delete(txn, key).await?;
        Ok(true)
    }

    pub async fn grant_role_to_user(
        &self,
        txn: &mut Transaction,
        username: &str,
        rolename: &str,
    ) -> Result<()> {
        let mut user = self
            .get_user(txn, username)
            .await?
            .ok_or_else(|| anyhow!("User '{}' does not exist", username))?;

        if self.get_role(txn, rolename).await?.is_none() {
            return Err(anyhow!("Role '{}' does not exist", rolename));
        }

        user.roles.insert(rolename.to_string());
        self.update_user(txn, user).await
    }

    pub async fn revoke_role_from_user(
        &self,
        txn: &mut Transaction,
        username: &str,
        rolename: &str,
    ) -> Result<()> {
        let mut user = self
            .get_user(txn, username)
            .await?
            .ok_or_else(|| anyhow!("User '{}' does not exist", username))?;

        user.roles.remove(rolename);
        self.update_user(txn, user).await
    }

    pub async fn check_privilege(
        &self,
        txn: &mut Transaction,
        username: &str,
        privilege: &Privilege,
        object: &PrivilegeObject,
    ) -> Result<bool> {
        self.check_privilege_internal(txn, username, privilege, object, false)
            .await
    }

    pub async fn check_privilege_with_grant_option(
        &self,
        txn: &mut Transaction,
        username: &str,
        privilege: &Privilege,
        object: &PrivilegeObject,
    ) -> Result<bool> {
        self.check_privilege_internal(txn, username, privilege, object, true)
            .await
    }

    async fn check_privilege_internal(
        &self,
        txn: &mut Transaction,
        username: &str,
        privilege: &Privilege,
        object: &PrivilegeObject,
        require_grant_option: bool,
    ) -> Result<bool> {
        let user = self
            .get_user(txn, username)
            .await?
            .ok_or_else(|| anyhow!("User '{}' does not exist", username))?;

        if user.is_superuser {
            return Ok(true);
        }

        match privilege {
            Privilege::CreateDB if user.can_create_db => return Ok(true),
            Privilege::CreateRole if user.can_create_role => return Ok(true),
            _ => {}
        }

        let has_user_privilege = if require_grant_option {
            user.has_privilege_with_grant_option(privilege, object)
        } else {
            user.has_privilege(privilege, object)
        };
        if has_user_privilege {
            return Ok(true);
        }

        for role_name in &user.roles {
            if let Some(role) = self.get_role(txn, role_name).await? {
                if role.is_superuser {
                    return Ok(true);
                }
                match privilege {
                    Privilege::CreateDB if role.can_create_db => return Ok(true),
                    Privilege::CreateRole if role.can_create_role => return Ok(true),
                    _ => {}
                }
                for granted in &role.privileges {
                    if User::granted_matches(granted, privilege, object, require_grant_option) {
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }

    pub async fn list_users(&self, txn: &mut Transaction) -> Result<Vec<User>> {
        let prefix = USER_KEY_PREFIX.to_vec();
        let mut end = prefix.clone();
        end.push(0xFF);

        let range: tikv_client::BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut users = Vec::new();
        for pair in pairs {
            let user: User = deserialize_user(pair.value())?;
            users.push(user);
        }
        Ok(users)
    }

    pub async fn list_roles(&self, txn: &mut Transaction) -> Result<Vec<Role>> {
        let prefix = ROLE_KEY_PREFIX.to_vec();
        let mut end = prefix.clone();
        end.push(0xFF);

        let range: tikv_client::BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut roles = Vec::new();
        for pair in pairs {
            let role: Role = deserialize_role(pair.value())?;
            roles.push(role);
        }
        Ok(roles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn save_env_vars(keys: &[&str]) -> Vec<(String, Option<String>)> {
        keys.iter()
            .map(|k| (k.to_string(), env::var(k).ok()))
            .collect()
    }

    fn restore_env_vars(saved: Vec<(String, Option<String>)>) {
        for (key, value) in saved {
            match value {
                Some(v) => unsafe {
                    env::set_var(key, v);
                },
                None => unsafe {
                    env::remove_var(key);
                },
            }
        }
    }

    #[test]
    fn test_user_password() {
        let user = User::new("test", "password123");
        assert!(user.verify_password("password123"));
        assert!(!user.verify_password("wrong"));
    }

    #[test]
    fn test_user_set_password() {
        let mut user = User::new("test", "old");
        user.set_password("new");
        assert!(user.verify_password("new"));
        assert!(!user.verify_password("old"));
    }

    #[test]
    fn test_superuser_has_all_privileges() {
        let user = User::new_superuser("admin", "admin");
        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("any")));
        assert!(user.has_privilege(&Privilege::Delete, &PrivilegeObject::database("any")));
    }

    #[test]
    fn test_grant_privilege() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::table("users"), false);

        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("users")));
        assert!(!user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("users")));
        assert!(!user.has_privilege(&Privilege::Select, &PrivilegeObject::table("orders")));
    }

    #[test]
    fn test_grant_all_tables_in_schema() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::all_tables(), false);

        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("orders")));
        assert!(!user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("users")));
    }

    #[test]
    fn test_grant_all_privileges() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::All, PrivilegeObject::table("users"), false);

        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Delete, &PrivilegeObject::table("users")));
    }

    #[test]
    fn test_revoke_privilege() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::table("users"), false);
        user.grant_privilege(Privilege::Insert, PrivilegeObject::table("users"), false);

        user.revoke_privilege(&Privilege::Select, &PrivilegeObject::table("users"));

        assert!(!user.has_privilege(&Privilege::Select, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("users")));
    }

    #[test]
    fn test_privilege_from_str() {
        assert_eq!(Privilege::from_str("SELECT"), Some(Privilege::Select));
        assert_eq!(Privilege::from_str("select"), Some(Privilege::Select));
        assert_eq!(Privilege::from_str("ALL"), Some(Privilege::All));
        assert_eq!(Privilege::from_str("ALL PRIVILEGES"), Some(Privilege::All));
        assert_eq!(Privilege::from_str("INVALID"), None);
    }

    #[test]
    fn test_role_creation() {
        let role = Role::new("readonly");
        assert_eq!(role.name, "readonly");
        assert!(role.privileges.is_empty());
        assert!(!role.is_superuser);
    }

    #[test]
    fn test_deserialize_legacy_user_defaults_bypass_rls_false() {
        let data = bincode::serialize(&UserLegacy {
            name: "legacy_user".into(),
            password_hash: "hash".into(),
            password_salt: "salt".into(),
            roles: HashSet::new(),
            privileges: Vec::new(),
            is_superuser: false,
            can_login: true,
            can_create_db: false,
            can_create_role: false,
            connection_limit: -1,
            valid_until: None,
        })
        .unwrap();

        let user = deserialize_user(&data).unwrap();
        assert_eq!(user.name, "legacy_user");
        assert!(!user.bypass_rls);
    }

    #[test]
    fn test_deserialize_legacy_role_defaults_bypass_rls_false() {
        let data = bincode::serialize(&RoleLegacy {
            name: "legacy_role".into(),
            privileges: Vec::new(),
            member_of: HashSet::new(),
            is_superuser: false,
            can_create_db: false,
            can_create_role: false,
        })
        .unwrap();

        let role = deserialize_role(&data).unwrap();
        assert_eq!(role.name, "legacy_role");
        assert!(!role.bypass_rls);
    }

    #[test]
    fn test_user_key() {
        let mgr = AuthManager::new();
        let key = mgr.user_key("admin");
        assert!(key.starts_with(USER_KEY_PREFIX));
    }

    #[test]
    fn test_privilege_from_str_all_types() {
        assert_eq!(Privilege::from_str("INSERT"), Some(Privilege::Insert));
        assert_eq!(Privilege::from_str("UPDATE"), Some(Privilege::Update));
        assert_eq!(Privilege::from_str("DELETE"), Some(Privilege::Delete));
        assert_eq!(Privilege::from_str("TRUNCATE"), Some(Privilege::Truncate));
        assert_eq!(
            Privilege::from_str("REFERENCES"),
            Some(Privilege::References)
        );
        assert_eq!(Privilege::from_str("TRIGGER"), Some(Privilege::Trigger));
        assert_eq!(Privilege::from_str("CREATE"), Some(Privilege::CreateTable));
        assert_eq!(Privilege::from_str("CONNECT"), Some(Privilege::Connect));
        assert_eq!(Privilege::from_str("TEMPORARY"), Some(Privilege::Temporary));
        assert_eq!(Privilege::from_str("TEMP"), Some(Privilege::Temporary));
        assert_eq!(Privilege::from_str("EXECUTE"), Some(Privilege::Execute));
        assert_eq!(Privilege::from_str("USAGE"), Some(Privilege::Usage));
        assert_eq!(Privilege::from_str("SUPERUSER"), Some(Privilege::SuperUser));
        assert_eq!(Privilege::from_str("CREATEDB"), Some(Privilege::CreateDB));
        assert_eq!(
            Privilege::from_str("CREATEROLE"),
            Some(Privilege::CreateRole)
        );
    }

    #[test]
    fn test_user_new_defaults() {
        let user = User::new("testuser", "testpass");
        assert_eq!(user.name, "testuser");
        assert!(!user.is_superuser);
        assert!(user.can_login);
        assert!(!user.can_create_db);
        assert!(!user.can_create_role);
        assert_eq!(user.connection_limit, -1);
        assert!(user.valid_until.is_none());
        assert!(user.roles.is_empty());
        assert!(user.privileges.is_empty());
    }

    #[test]
    fn test_user_new_superuser_defaults() {
        let user = User::new_superuser("admin", "admin");
        assert_eq!(user.name, "admin");
        assert!(user.is_superuser);
        assert!(user.can_login);
        assert!(user.can_create_db);
        assert!(user.can_create_role);
    }

    #[test]
    fn test_privilege_object_table() {
        let obj = PrivilegeObject::table("users");
        match obj {
            PrivilegeObject::Table { schema, name } => {
                assert_eq!(schema, "public");
                assert_eq!(name, "users");
            }
            _ => panic!("Expected Table variant"),
        }
    }

    #[test]
    fn test_privilege_object_all_tables() {
        let obj = PrivilegeObject::all_tables();
        match obj {
            PrivilegeObject::AllTablesInSchema(schema) => {
                assert_eq!(schema, "public");
            }
            _ => panic!("Expected AllTablesInSchema variant"),
        }
    }

    #[test]
    fn test_privilege_object_database() {
        let obj = PrivilegeObject::database("mydb");
        match obj {
            PrivilegeObject::Database(name) => {
                assert_eq!(name, "mydb");
            }
            _ => panic!("Expected Database variant"),
        }
    }

    #[test]
    fn test_grant_replaces_existing() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::table("t1"), false);
        assert_eq!(user.privileges.len(), 1);
        assert!(!user.privileges[0].with_grant_option);

        user.grant_privilege(Privilege::Select, PrivilegeObject::table("t1"), true);
        assert_eq!(user.privileges.len(), 1);
        assert!(user.privileges[0].with_grant_option);
    }

    #[test]
    fn test_revoke_nonexistent_privilege() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::table("t1"), false);
        assert_eq!(user.privileges.len(), 1);

        user.revoke_privilege(&Privilege::Insert, &PrivilegeObject::table("t1"));
        assert_eq!(user.privileges.len(), 1);

        user.revoke_privilege(&Privilege::Select, &PrivilegeObject::table("t2"));
        assert_eq!(user.privileges.len(), 1);
    }

    #[test]
    fn test_has_privilege_global_object() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::Global, false);

        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("any")));
        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::database("any")));
    }

    #[test]
    fn test_all_privilege_matches_specific() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::All, PrivilegeObject::table("users"), false);

        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Update, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Delete, &PrivilegeObject::table("users")));
        assert!(user.has_privilege(&Privilege::Truncate, &PrivilegeObject::table("users")));
    }

    #[test]
    fn test_all_tables_in_schema_matches_table() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(
            Privilege::Select,
            PrivilegeObject::AllTablesInSchema("public".to_string()),
            false,
        );

        assert!(user.has_privilege(
            &Privilege::Select,
            &PrivilegeObject::Table {
                schema: "public".to_string(),
                name: "users".to_string(),
            }
        ));
        assert!(user.has_privilege(
            &Privilege::Select,
            &PrivilegeObject::Table {
                schema: "public".to_string(),
                name: "orders".to_string(),
            }
        ));
        assert!(!user.has_privilege(
            &Privilege::Select,
            &PrivilegeObject::Table {
                schema: "private".to_string(),
                name: "secrets".to_string(),
            }
        ));
    }

    #[test]
    fn test_role_key() {
        let mgr = AuthManager::new();
        let key = mgr.role_key("writer");
        assert!(key.starts_with(ROLE_KEY_PREFIX));
        assert!(key.ends_with(b"writer"));
    }

    #[test]
    fn test_user_roles_management() {
        let mut user = User::new("test", "pass");
        assert!(user.roles.is_empty());

        user.roles.insert("reader".to_string());
        user.roles.insert("writer".to_string());
        assert_eq!(user.roles.len(), 2);
        assert!(user.roles.contains("reader"));
        assert!(user.roles.contains("writer"));

        user.roles.remove("reader");
        assert_eq!(user.roles.len(), 1);
        assert!(!user.roles.contains("reader"));
    }

    #[test]
    fn test_granted_privilege_with_grant_option() {
        let gp = GrantedPrivilege {
            privilege: Privilege::Select,
            object: PrivilegeObject::table("users"),
            with_grant_option: true,
        };
        assert_eq!(gp.privilege, Privilege::Select);
        assert!(gp.with_grant_option);
    }

    #[test]
    fn test_password_empty_string() {
        let user = User::new("test", "");
        assert!(user.verify_password(""));
        assert!(!user.verify_password("notempty"));
    }

    #[test]
    fn test_password_special_chars() {
        let user = User::new("test", "p@ss!w0rd#$%^&*()");
        assert!(user.verify_password("p@ss!w0rd#$%^&*()"));
        assert!(!user.verify_password("p@ss!w0rd"));
    }

    #[test]
    fn test_password_unicode() {
        let user = User::new("test", "密码测试123");
        assert!(user.verify_password("密码测试123"));
        assert!(!user.verify_password("密码测试"));
    }

    #[test]
    fn test_privilege_expand_all() {
        let expanded = Privilege::expand_all();
        assert!(expanded.contains(&Privilege::Select));
        assert!(expanded.contains(&Privilege::Insert));
        assert!(expanded.contains(&Privilege::Update));
        assert!(expanded.contains(&Privilege::Delete));
        assert!(expanded.contains(&Privilege::Truncate));
        assert!(expanded.contains(&Privilege::References));
        assert!(expanded.contains(&Privilege::Trigger));
        assert!(!expanded.contains(&Privilege::SuperUser));
        assert!(!expanded.contains(&Privilege::CreateDB));
    }

    #[test]
    fn test_multiple_privileges_same_object() {
        let mut user = User::new("test", "pass");
        user.grant_privilege(Privilege::Select, PrivilegeObject::table("t1"), false);
        user.grant_privilege(Privilege::Insert, PrivilegeObject::table("t1"), false);
        user.grant_privilege(Privilege::Update, PrivilegeObject::table("t1"), false);

        assert_eq!(user.privileges.len(), 3);
        assert!(user.has_privilege(&Privilege::Select, &PrivilegeObject::table("t1")));
        assert!(user.has_privilege(&Privilege::Insert, &PrivilegeObject::table("t1")));
        assert!(user.has_privilege(&Privilege::Update, &PrivilegeObject::table("t1")));
        assert!(!user.has_privilege(&Privilege::Delete, &PrivilegeObject::table("t1")));
    }

    #[test]
    fn test_role_member_of() {
        let mut role = Role::new("admin");
        assert!(role.member_of.is_empty());

        role.member_of.insert("superadmin".to_string());
        assert!(role.member_of.contains("superadmin"));
    }

    #[test]
    fn test_role_options() {
        let mut role = Role::new("dba");
        assert!(!role.is_superuser);
        assert!(!role.can_create_db);
        assert!(!role.can_create_role);

        role.is_superuser = true;
        role.can_create_db = true;
        role.can_create_role = true;

        assert!(role.is_superuser);
        assert!(role.can_create_db);
        assert!(role.can_create_role);
    }

    #[test]
    fn bootstrap_rejects_db9_dev_without_dev_admin_password() {
        let env_keys = [
            "DB9_DEV",
            "DB9_DEV_ADMIN_PASSWORD",
            "DB9_BOOTSTRAP_ADMIN_USER",
            "DB9_BOOTSTRAP_ADMIN_PASSWORD",
        ];
        let result = {
            let _guard = env_lock().lock().unwrap();
            let saved = save_env_vars(&env_keys);
            unsafe {
                env::set_var("DB9_DEV", "1");
                env::remove_var("DB9_DEV_ADMIN_PASSWORD");
                env::remove_var("DB9_BOOTSTRAP_ADMIN_USER");
                env::remove_var("DB9_BOOTSTRAP_ADMIN_PASSWORD");
            }
            let result = AuthManager::resolve_bootstrap_credentials();
            restore_env_vars(saved);
            result
        };

        let err = result.expect_err("must reject DB9_DEV=1 without DB9_DEV_ADMIN_PASSWORD");
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error should be SqlError::InvalidAuthorizationSpecification");
        match sql_err {
            SqlError::InvalidAuthorizationSpecification { message } => {
                assert!(message.contains("DB9_DEV=1 requires DB9_DEV_ADMIN_PASSWORD"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_rejects_existing_non_superuser_bootstrap_user() {
        let non_super = User::new("bootstrap_user", "pass");
        let result =
            AuthManager::validate_existing_bootstrap_user(Some(&non_super), "bootstrap_user");

        let err = result.expect_err("must reject existing non-superuser bootstrap account");
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("error should be SqlError::InvalidAuthorizationSpecification");
        match sql_err {
            SqlError::InvalidAuthorizationSpecification { message } => {
                assert!(message.contains("already exists but is not a superuser"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_accepts_existing_superuser() {
        let superuser = User::new_superuser("admin", "pass");
        let result = AuthManager::validate_existing_bootstrap_user(Some(&superuser), "admin");
        assert!(result.unwrap(), "existing superuser should be accepted");
    }

    #[test]
    fn bootstrap_accepts_no_existing_user() {
        let result = AuthManager::validate_existing_bootstrap_user(None, "admin");
        assert!(
            !result.unwrap(),
            "no existing user means proceed with creation"
        );
    }

    #[test]
    fn backfill_prefers_existing_bootstrap_user_key() {
        let result = AuthManager::resolve_bootstrap_user_for_backfill(
            Some(b"admin".to_vec()),
            &["legacy_admin".to_string()],
        )
        .unwrap();
        assert_eq!(result, (Some("admin".to_string()), false, None));
    }

    #[test]
    fn backfill_persists_single_superuser_when_key_missing() {
        let users = [
            User::new("app_user", "pass"),
            User::new_superuser("admin", "pass"),
        ];
        let superusers = AuthManager::superuser_names(users.iter());
        let result = AuthManager::resolve_bootstrap_user_for_backfill(None, &superusers).unwrap();
        assert_eq!(result, (Some("admin".to_string()), true, None));
    }

    #[test]
    fn backfill_disables_protections_when_key_missing_and_multiple_superusers() {
        let users = [
            User::new_superuser("zebra", "pass"),
            User::new("app_user", "pass"),
            User::new_superuser("postgres", "pass"),
            User::new_superuser("admin", "pass"),
        ];
        let superusers = AuthManager::superuser_names(users.iter());
        let result = AuthManager::resolve_bootstrap_user_for_backfill(None, &superusers).unwrap();
        assert_eq!(
            result,
            (
                None,
                false,
                Some(vec![
                    "admin".to_string(),
                    "postgres".to_string(),
                    "zebra".to_string()
                ])
            )
        );
    }

    #[test]
    fn backfill_returns_none_when_no_superuser_exists() {
        let users = [User::new("u1", "pass"), User::new("u2", "pass")];
        let superusers = AuthManager::superuser_names(users.iter());
        let result = AuthManager::resolve_bootstrap_user_for_backfill(None, &superusers).unwrap();
        assert_eq!(result, (None, false, None));
    }

    #[test]
    fn backfill_rejects_invalid_utf8_bootstrap_key() {
        let err = AuthManager::resolve_bootstrap_user_for_backfill(Some(vec![0xff]), &[])
            .expect_err("invalid UTF-8 bootstrap key should fail");
        assert!(err
            .to_string()
            .contains("invalid UTF-8 value in _sys_bootstrap_user"));
    }
}
