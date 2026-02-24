use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use super::portal::Portal;
use super::stmt::StoredStatement;
use crate::error::{ErrorInfo, PgWireError, PgWireResult};

pub trait PortalStore: Send + Sync {
    type Statement;

    fn put_statement(&self, statement: Arc<StoredStatement<Self::Statement>>) -> PgWireResult<()>;

    fn rm_statement(&self, name: &str);

    fn get_statement(&self, name: &str) -> Option<Arc<StoredStatement<Self::Statement>>>;

    fn put_portal(&self, portal: Arc<Portal<Self::Statement>>) -> PgWireResult<()>;

    fn rm_portal(&self, name: &str);

    fn get_portal(&self, name: &str) -> Option<Arc<Portal<Self::Statement>>>;
}

const DEFAULT_MAX_STATEMENTS: usize = 1024;
const DEFAULT_MAX_PORTALS: usize = 1024;

fn env_limit(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[derive(Debug)]
pub struct MemPortalStore<S> {
    statements: RwLock<BTreeMap<String, Arc<StoredStatement<S>>>>,
    portals: RwLock<BTreeMap<String, Arc<Portal<S>>>>,
    max_statements: usize,
    max_portals: usize,
}

impl<S> Default for MemPortalStore<S> {
    fn default() -> Self {
        let max_statements = env_limit("DB9_MAX_STATEMENTS", DEFAULT_MAX_STATEMENTS);
        let max_portals = env_limit("DB9_MAX_PORTALS", DEFAULT_MAX_PORTALS);
        Self::new_with_limits(max_statements, max_portals)
    }
}

impl<S> MemPortalStore<S> {
    pub fn new_with_limits(max_statements: usize, max_portals: usize) -> Self {
        Self {
            statements: RwLock::new(BTreeMap::new()),
            portals: RwLock::new(BTreeMap::new()),
            max_statements: max_statements.max(1),
            max_portals: max_portals.max(1),
        }
    }

    fn statements_limit_exceeded_error(&self) -> PgWireError {
        PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_string(),
            "54000".to_string(),
            format!(
                "too many prepared statements (limit={}); set DB9_MAX_STATEMENTS to override",
                self.max_statements
            ),
        )))
    }

    fn portals_limit_exceeded_error(&self) -> PgWireError {
        PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_string(),
            "54000".to_string(),
            format!(
                "too many portals (limit={}); set DB9_MAX_PORTALS to override",
                self.max_portals
            ),
        )))
    }
}

impl<S: Clone + Send + Sync> PortalStore for MemPortalStore<S> {
    type Statement = S;

    fn put_statement(&self, statement: Arc<StoredStatement<Self::Statement>>) -> PgWireResult<()> {
        let mut guard = self.statements.write().unwrap();
        if !guard.contains_key(&statement.id) && guard.len() >= self.max_statements {
            return Err(self.statements_limit_exceeded_error());
        }
        guard.insert(statement.id.to_owned(), statement);
        Ok(())
    }

    fn rm_statement(&self, name: &str) {
        let mut guard = self.statements.write().unwrap();
        guard.remove(name);
    }

    fn get_statement(&self, name: &str) -> Option<Arc<StoredStatement<Self::Statement>>> {
        let guard = self.statements.read().unwrap();
        guard.get(name).cloned()
    }

    fn put_portal(&self, portal: Arc<Portal<Self::Statement>>) -> PgWireResult<()> {
        let mut guard = self.portals.write().unwrap();
        if !guard.contains_key(&portal.name) && guard.len() >= self.max_portals {
            return Err(self.portals_limit_exceeded_error());
        }
        guard.insert(portal.name.to_owned(), portal);
        Ok(())
    }

    fn rm_portal(&self, name: &str) {
        let mut guard = self.portals.write().unwrap();
        guard.remove(name);
    }

    fn get_portal(&self, name: &str) -> Option<Arc<Portal<Self::Statement>>> {
        let guard = self.portals.read().unwrap();
        guard.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::portal::Portal;
    use crate::api::stmt::StoredStatement;

    #[test]
    fn mem_portal_store_enforces_statement_limit() {
        let store = MemPortalStore::<String>::new_with_limits(2, 2);
        store
            .put_statement(Arc::new(StoredStatement::new(
                "s1".to_string(),
                "SELECT 1".to_string(),
                vec![],
            )))
            .unwrap();
        store
            .put_statement(Arc::new(StoredStatement::new(
                "s2".to_string(),
                "SELECT 1".to_string(),
                vec![],
            )))
            .unwrap();

        let err = store
            .put_statement(Arc::new(StoredStatement::new(
                "s3".to_string(),
                "SELECT 1".to_string(),
                vec![],
            )))
            .unwrap_err();

        match err {
            PgWireError::UserError(info) => assert_eq!(info.code, "54000"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn mem_portal_store_enforces_portal_limit() {
        let store = MemPortalStore::<String>::new_with_limits(2, 2);
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT 1".to_string(),
            vec![],
        ));

        store
            .put_portal(Arc::new(Portal {
                name: "p1".to_string(),
                statement: stmt.clone(),
                ..Portal::default()
            }))
            .unwrap();
        store
            .put_portal(Arc::new(Portal {
                name: "p2".to_string(),
                statement: stmt.clone(),
                ..Portal::default()
            }))
            .unwrap();

        let err = store
            .put_portal(Arc::new(Portal {
                name: "p3".to_string(),
                statement: stmt,
                ..Portal::default()
            }))
            .unwrap_err();

        match err {
            PgWireError::UserError(info) => assert_eq!(info.code, "54000"),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
