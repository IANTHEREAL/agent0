/// Count the number of parameter placeholders ($1, $2, ...) in a SQL query.
/// Returns the maximum placeholder number found, which indicates how many parameters are expected.
pub(in crate::protocol::handler) fn count_sql_parameters(sql: &str) -> usize {
    crate::sql::scanner::count_sql_parameters(sql)
}
