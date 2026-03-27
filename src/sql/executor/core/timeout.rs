//! Statement timeout helpers

#[derive(Debug)]
pub(crate) struct StatementTimeoutError;

impl std::fmt::Display for StatementTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "canceling statement due to statement timeout")
    }
}

impl std::error::Error for StatementTimeoutError {}
