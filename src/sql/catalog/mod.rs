mod check_constraints;
mod columns;
mod constraint_column_usage;
mod cron_job;
mod cron_job_run_details;
mod cron_running_jobs;
pub(crate) mod helpers;
mod key_column_usage;
mod pg_am;
mod pg_attrdef;
mod pg_attribute;
mod pg_class;
mod pg_collation;
mod pg_constraint;
mod pg_database;
mod pg_db_role_setting;
mod pg_depend;
mod pg_description;
mod pg_enum;
mod pg_extension;
mod pg_index;
mod pg_indexes;
mod pg_inherits;
mod pg_namespace;
mod pg_opclass;
mod pg_policy;
mod pg_proc;
mod pg_publication;
mod pg_publication_namespace;
mod pg_publication_rel;
mod pg_range;
mod pg_roles;
mod pg_sequence;
mod pg_stat_user_tables;
mod pg_statistic_ext;
mod pg_tables;
mod pg_trigger;
mod pg_type;
mod pg_views;
mod referential_constraints;
mod routines;
mod schemata;
mod sequences;
mod table_constraints;
mod table_privileges;
mod tables;
pub(crate) mod virtual_tables;

use crate::model::{Row, TableSchema};
use crate::storage::TikvStore;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

pub struct ScanContext<'a> {
    pub store: &'a Arc<TikvStore>,
    pub txn: &'a mut Transaction,
    pub db_id: u64,
    pub database_name: &'a str,
    pub user_tables: &'a [String],
    pub schemas: &'a [String],
    pub schema_oids: &'a HashMap<String, u32>,
    pub current_user: &'a str,
    pub is_superuser: bool,
}

#[async_trait]
pub trait VirtualTable: Send + Sync {
    fn name(&self) -> &str;
    #[allow(dead_code)] // framework: virtual table trait API
    fn schema_name(&self) -> &str;
    fn schema(&self) -> TableSchema;
    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>>;
}

pub struct CatalogRegistry {
    tables: HashMap<String, Box<dyn VirtualTable>>,
}

impl CatalogRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tables: HashMap::new(),
        };
        registry.register(Box::new(check_constraints::CheckConstraints));
        registry.register(Box::new(columns::Columns));
        registry.register(Box::new(constraint_column_usage::ConstraintColumnUsage));
        registry.register(Box::new(key_column_usage::KeyColumnUsage));
        registry.register(Box::new(pg_am::PgAm));
        registry.register(Box::new(pg_collation::PgCollation));
        registry.register(Box::new(pg_attrdef::PgAttrdef));
        registry.register(Box::new(pg_attribute::PgAttribute));
        registry.register(Box::new(pg_class::PgClass));
        registry.register(Box::new(pg_constraint::PgConstraint));
        registry.register(Box::new(pg_db_role_setting::PgDbRoleSetting));
        registry.register(Box::new(pg_database::PgDatabase));
        registry.register(Box::new(pg_depend::PgDepend));
        registry.register(Box::new(pg_description::PgDescription));
        registry.register(Box::new(pg_enum::PgEnum));
        registry.register(Box::new(pg_extension::PgExtension));
        registry.register(Box::new(pg_inherits::PgInherits));
        registry.register(Box::new(pg_index::PgIndex));
        registry.register(Box::new(pg_indexes::PgIndexes));
        registry.register(Box::new(pg_namespace::PgNamespace));
        registry.register(Box::new(pg_opclass::PgOpclass));
        registry.register(Box::new(pg_policy::PgPolicy));
        registry.register(Box::new(pg_publication::PgPublication));
        registry.register(Box::new(pg_publication_namespace::PgPublicationNamespace));
        registry.register(Box::new(pg_publication_rel::PgPublicationRel));
        registry.register(Box::new(pg_proc::PgProc));
        registry.register(Box::new(pg_range::PgRange));
        registry.register(Box::new(pg_roles::PgRoles));
        registry.register(Box::new(pg_sequence::PgSequence));
        registry.register(Box::new(pg_statistic_ext::PgStatisticExt));
        registry.register(Box::new(pg_stat_user_tables::PgStatUserTables));
        registry.register(Box::new(pg_tables::PgTables));
        registry.register(Box::new(pg_trigger::PgTrigger));
        registry.register(Box::new(pg_type::PgType));
        registry.register(Box::new(pg_views::PgViews));
        registry.register(Box::new(referential_constraints::ReferentialConstraints));
        registry.register(Box::new(routines::Routines));
        registry.register(Box::new(schemata::Schemata));
        registry.register(Box::new(sequences::Sequences));
        registry.register(Box::new(table_privileges::TablePrivileges));
        registry.register(Box::new(table_constraints::TableConstraints));
        registry.register(Box::new(tables::Tables));
        registry.register(Box::new(cron_job::CronJobTable));
        registry.register(Box::new(cron_job_run_details::CronJobRunDetailsTable));
        registry.register(Box::new(cron_running_jobs::CronRunningJobsTable));
        registry
    }

    fn register(&mut self, table: Box<dyn VirtualTable>) {
        self.tables.insert(table.name().to_string(), table);
    }

    pub fn get(&self, name: &str) -> Option<&dyn VirtualTable> {
        self.tables.get(name).map(|t| t.as_ref())
    }
}

static CATALOG: std::sync::LazyLock<CatalogRegistry> =
    std::sync::LazyLock::new(CatalogRegistry::new);

pub fn global_catalog() -> &'static CatalogRegistry {
    &CATALOG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_pg_am() {
        let catalog = global_catalog();
        let pg_am = catalog.get("pg_am").unwrap();
        assert_eq!(pg_am.name(), "pg_am");
        assert_eq!(pg_am.schema_name(), "pg_catalog");
        assert_eq!(pg_am.schema().columns.len(), 2);
    }

    #[test]
    fn registry_contains_pg_namespace() {
        let catalog = global_catalog();
        let pg_ns = catalog.get("pg_namespace").unwrap();
        assert_eq!(pg_ns.name(), "pg_namespace");
        assert_eq!(pg_ns.schema_name(), "pg_catalog");
    }

    #[test]
    fn registry_contains_schemata() {
        let catalog = global_catalog();
        let schemata = catalog.get("schemata").unwrap();
        assert_eq!(schemata.name(), "schemata");
        assert_eq!(schemata.schema_name(), "information_schema");
    }

    #[test]
    fn registry_contains_pg_range() {
        let catalog = global_catalog();
        let t = catalog.get("pg_range").unwrap();
        assert_eq!(t.name(), "pg_range");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 6);
    }

    #[test]
    fn registry_contains_pg_roles() {
        let catalog = global_catalog();
        let t = catalog.get("pg_roles").unwrap();
        assert_eq!(t.name(), "pg_roles");
        assert_eq!(t.schema().columns.len(), 13);
    }

    #[test]
    fn registry_contains_pg_collation() {
        let catalog = global_catalog();
        let t = catalog.get("pg_collation").unwrap();
        assert_eq!(t.name(), "pg_collation");
        assert_eq!(t.schema().columns.len(), 12);
    }

    #[test]
    fn registry_contains_pg_database() {
        let catalog = global_catalog();
        let t = catalog.get("pg_database").unwrap();
        assert_eq!(t.name(), "pg_database");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 14);
    }

    #[test]
    fn registry_contains_pg_db_role_setting() {
        let catalog = global_catalog();
        let t = catalog.get("pg_db_role_setting").unwrap();
        assert_eq!(t.name(), "pg_db_role_setting");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 3);
    }

    #[test]
    fn registry_contains_pg_enum() {
        let catalog = global_catalog();
        let t = catalog.get("pg_enum").unwrap();
        assert_eq!(t.name(), "pg_enum");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 4);
    }

    #[test]
    fn registry_contains_sequences() {
        let catalog = global_catalog();
        let t = catalog.get("sequences").unwrap();
        assert_eq!(t.name(), "sequences");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 10);
    }

    #[test]
    fn registry_contains_routines() {
        let catalog = global_catalog();
        let t = catalog.get("routines").unwrap();
        assert_eq!(t.name(), "routines");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 6);
    }

    #[test]
    fn registry_contains_pg_sequence() {
        let catalog = global_catalog();
        let t = catalog.get("pg_sequence").unwrap();
        assert_eq!(t.name(), "pg_sequence");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 8);
    }

    #[test]
    fn registry_contains_pg_depend() {
        let catalog = global_catalog();
        let t = catalog.get("pg_depend").unwrap();
        assert_eq!(t.name(), "pg_depend");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 7);
    }

    #[test]
    fn registry_contains_pg_views() {
        let catalog = global_catalog();
        let t = catalog.get("pg_views").unwrap();
        assert_eq!(t.name(), "pg_views");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 4);
    }

    #[test]
    fn registry_contains_pg_tables() {
        let catalog = global_catalog();
        let t = catalog.get("pg_tables").unwrap();
        assert_eq!(t.name(), "pg_tables");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 8);
    }

    #[test]
    fn registry_contains_pg_attrdef() {
        let catalog = global_catalog();
        let t = catalog.get("pg_attrdef").unwrap();
        assert_eq!(t.name(), "pg_attrdef");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 5);
    }

    #[test]
    fn registry_contains_pg_indexes() {
        let catalog = global_catalog();
        let t = catalog.get("pg_indexes").unwrap();
        assert_eq!(t.name(), "pg_indexes");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 5);
    }

    #[test]
    fn registry_contains_pg_type() {
        let catalog = global_catalog();
        let t = catalog.get("pg_type").unwrap();
        assert_eq!(t.name(), "pg_type");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 16);
    }

    #[test]
    fn registry_contains_pg_class() {
        let catalog = global_catalog();
        let t = catalog.get("pg_class").unwrap();
        assert_eq!(t.name(), "pg_class");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 26);
    }

    #[test]
    fn registry_contains_pg_index() {
        let catalog = global_catalog();
        let t = catalog.get("pg_index").unwrap();
        assert_eq!(t.name(), "pg_index");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 17);
    }

    #[test]
    fn registry_contains_pg_attribute() {
        let catalog = global_catalog();
        let t = catalog.get("pg_attribute").unwrap();
        assert_eq!(t.name(), "pg_attribute");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 18);
    }

    #[test]
    fn registry_contains_pg_statistic_ext() {
        let catalog = global_catalog();
        let t = catalog.get("pg_statistic_ext").unwrap();
        assert_eq!(t.name(), "pg_statistic_ext");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 6);
    }

    #[test]
    fn registry_contains_pg_proc() {
        let catalog = global_catalog();
        let t = catalog.get("pg_proc").unwrap();
        assert_eq!(t.name(), "pg_proc");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 6);
    }

    #[test]
    fn registry_contains_pg_extension() {
        let catalog = global_catalog();
        let t = catalog.get("pg_extension").unwrap();
        assert_eq!(t.name(), "pg_extension");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 8);
    }

    #[test]
    fn registry_contains_pg_trigger() {
        let catalog = global_catalog();
        let t = catalog.get("pg_trigger").unwrap();
        assert_eq!(t.name(), "pg_trigger");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 7);
    }

    #[test]
    fn registry_contains_pg_description() {
        let catalog = global_catalog();
        let t = catalog.get("pg_description").unwrap();
        assert_eq!(t.name(), "pg_description");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 4);
    }

    #[test]
    fn registry_contains_pg_constraint() {
        let catalog = global_catalog();
        let t = catalog.get("pg_constraint").unwrap();
        assert_eq!(t.name(), "pg_constraint");
        assert_eq!(t.schema_name(), "pg_catalog");
        assert_eq!(t.schema().columns.len(), 20);
    }

    #[test]
    fn registry_contains_tables() {
        let catalog = global_catalog();
        let t = catalog.get("tables").unwrap();
        assert_eq!(t.name(), "tables");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 13);
    }

    #[test]
    fn registry_contains_columns() {
        let catalog = global_catalog();
        let t = catalog.get("columns").unwrap();
        assert_eq!(t.name(), "columns");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 44);
    }

    #[test]
    fn registry_contains_table_constraints() {
        let catalog = global_catalog();
        let t = catalog.get("table_constraints").unwrap();
        assert_eq!(t.name(), "table_constraints");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 10);
    }

    #[test]
    fn registry_contains_key_column_usage() {
        let catalog = global_catalog();
        let t = catalog.get("key_column_usage").unwrap();
        assert_eq!(t.name(), "key_column_usage");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 9);
    }

    #[test]
    fn registry_contains_referential_constraints() {
        let catalog = global_catalog();
        let t = catalog.get("referential_constraints").unwrap();
        assert_eq!(t.name(), "referential_constraints");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 9);
    }

    #[test]
    fn registry_contains_constraint_column_usage() {
        let catalog = global_catalog();
        let t = catalog.get("constraint_column_usage").unwrap();
        assert_eq!(t.name(), "constraint_column_usage");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 7);
    }

    #[test]
    fn registry_contains_check_constraints() {
        let catalog = global_catalog();
        let t = catalog.get("check_constraints").unwrap();
        assert_eq!(t.name(), "check_constraints");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 4);
    }

    #[test]
    fn registry_contains_table_privileges() {
        let catalog = global_catalog();
        let t = catalog.get("table_privileges").unwrap();
        assert_eq!(t.name(), "table_privileges");
        assert_eq!(t.schema_name(), "information_schema");
        assert_eq!(t.schema().columns.len(), 8);
    }

    #[test]
    fn registry_returns_none_for_unknown() {
        let catalog = global_catalog();
        assert!(catalog.get("nonexistent").is_none());
    }
}
