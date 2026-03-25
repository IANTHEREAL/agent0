use serde::{Deserialize, Serialize};

use super::data_type::DataType;
use super::default_owner;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserTypeKind {
    Enum { labels: Vec<String> },
    Composite { fields: Vec<(String, DataType)> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTypeDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: UserTypeKind,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceState {
    pub last_value: i64,
    pub is_called: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SequenceBacking {
    /// Bridge to the existing per-table autoincrement key (`_sys_seq_ + table_id`).
    TableId(u64),
    /// A standalone sequence whose state is stored in `SequenceState`.
    Standalone(SequenceState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub start_value: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    #[serde(default)]
    pub cache_size: i64,
    pub is_cycled: bool,
    pub owned_by: Option<(String, String)>,
    pub owner: String,
    pub backing: SequenceBacking,
}

impl SequenceDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub arg_types: Vec<String>,
    pub return_type: String,
    pub language: String,
    pub body: String,
    /// Owner role/user name for this function (metadata only).
    #[serde(default = "default_owner")]
    pub owner: String,
    /// Whether this function runs with the privileges of the definer (owner)
    /// rather than the invoker (caller). Corresponds to PostgreSQL's
    /// `SECURITY DEFINER` attribute (pg_proc.prosecdef).
    #[serde(default)]
    pub security_definer: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub table: String,
    pub timing: String,
    pub events: Vec<String>,
    pub function: String,
}

/// RLS policy command scope (matches PostgreSQL `pg_policy.polcmd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RlsCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl RlsCommand {
    /// Return the single-character code used by `pg_policy.polcmd`.
    pub fn pg_polcmd(&self) -> &'static str {
        match self {
            RlsCommand::All => "*",
            RlsCommand::Select => "r",
            RlsCommand::Insert => "a",
            RlsCommand::Update => "w",
            RlsCommand::Delete => "d",
        }
    }

    /// Return the human-readable command name used by `pg_policies` view.
    pub fn pg_cmd_display(&self) -> &'static str {
        match self {
            RlsCommand::All => "ALL",
            RlsCommand::Select => "SELECT",
            RlsCommand::Insert => "INSERT",
            RlsCommand::Update => "UPDATE",
            RlsCommand::Delete => "DELETE",
        }
    }

    /// Whether this command scope applies to a given DML operation.
    #[allow(dead_code)]
    pub fn applies_to_select(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Select)
    }

    #[allow(dead_code)]
    pub fn applies_to_insert(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Insert)
    }

    #[allow(dead_code)]
    pub fn applies_to_update(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Update)
    }

    #[allow(dead_code)]
    pub fn applies_to_delete(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Delete)
    }
}

/// A row-level security policy stored in TiKV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RlsPolicy {
    /// Unique OID for `pg_policy.oid` catalog compatibility.
    pub oid: u32,
    /// Policy name (unique per table).
    pub name: String,
    /// Table this policy applies to (by table_id).
    pub table_id: u64,
    /// Which DML command(s) the policy applies to.
    pub command: RlsCommand,
    /// `true` = PERMISSIVE (OR'd), `false` = RESTRICTIVE (AND'd).
    pub permissive: bool,
    /// Roles this policy applies to. Empty or `["public"]` means all roles.
    pub roles: Vec<String>,
    /// SQL expression for row visibility (SELECT/UPDATE/DELETE).
    pub using_expr: Option<String>,
    /// SQL expression for new-row validation (INSERT/UPDATE).
    pub with_check_expr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    #[serde(default = "default_owner")]
    pub owner: String,
    pub query: String,
    /// Fully-qualified names of relations this view depends on.
    /// Resolved at CREATE time using the active search_path.
    pub deps: Vec<String>,
    /// Whether this view uses SECURITY DEFINER semantics: RLS policies
    /// are evaluated using the view owner's identity, not the caller's.
    #[serde(default)]
    pub security_definer: bool,
}

impl ViewDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatViewDef {
    pub schema: String,
    pub name: String,
    pub query: String,
    /// Fully-qualified names of relations this materialized view depends on.
    pub deps: Vec<String>,
}

impl MatViewDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}
