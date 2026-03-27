//! PL/pgSQL function parsing and execution.
//!
//! Supports: DECLARE, BEGIN/END, RETURN, IF/THEN/ELSIF/ELSE/END IF, RAISE, assignment (:=),
//! SELECT INTO, FOR loops (query and range), PERFORM, EXIT, and user function dispatch.

pub(super) mod ast_bind;
mod executor;
mod parser;
pub(crate) mod utils;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::model::Value;

// Re-export the public API surface.
#[allow(unused_imports)]
pub use executor::execute_plpgsql_function;
pub use executor::try_execute_user_function;
pub(crate) use utils::replace_identifier;

/// Runtime context for a PL/pgSQL function execution, holding variables
/// and accumulated RAISE NOTICE messages.
pub struct PlpgsqlContext {
    pub variables: HashMap<String, Value>,
    pub variable_types: HashMap<String, crate::model::DataType>,
    pub notices: Vec<String>,
    /// When set, all statements within this function execute using the given
    /// role identity (SECURITY DEFINER). `None` means SECURITY INVOKER (default).
    pub security_definer_role: Option<String>,
}

impl PlpgsqlContext {
    pub fn new() -> Self {
        Self {
            variables: HashMap::new(),
            variable_types: HashMap::new(),
            notices: Vec::new(),
            security_definer_role: None,
        }
    }

    pub fn set_var(&mut self, name: &str, value: Value) {
        let name_lower = name.to_lowercase();
        let key = self
            .variables
            .keys()
            .find(|k| k.to_lowercase() == name_lower)
            .cloned()
            .unwrap_or(name_lower);
        self.variables.insert(key, value);
    }
}

/// Validate that a PL/pgSQL function body can be parsed without executing it.
pub fn validate_plpgsql_body(body: &str) -> Result<()> {
    let _ = parser::parse_declare_block(body)?;
    let empty_vars = HashSet::new();
    let _ = parser::parse_begin_block(body, &empty_vars)?;
    Ok(())
}
