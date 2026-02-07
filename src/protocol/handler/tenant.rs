/// Parse username in format "tenant.user" or "tenant:user" into (keyspace, actual_user).
/// If no separator found, returns (None, username) - no keyspace override.
pub(super) fn parse_tenant_username(username: &str) -> (Option<String>, String) {
    // Try dot separator first: "tenant_a.admin" -> keyspace=tenant_a, user=admin
    if let Some(pos) = username.find('.') {
        let tenant = &username[..pos];
        let user = &username[pos + 1..];
        if !tenant.is_empty() && !user.is_empty() {
            return (Some(tenant.to_string()), user.to_string());
        }
    }
    // Try colon separator: "tenant_a:admin" -> keyspace=tenant_a, user=admin
    if let Some(pos) = username.find(':') {
        let tenant = &username[..pos];
        let user = &username[pos + 1..];
        if !tenant.is_empty() && !user.is_empty() {
            return (Some(tenant.to_string()), user.to_string());
        }
    }
    // No separator or invalid format - use as-is without keyspace override
    (None, username.to_string())
}

