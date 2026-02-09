const KEYSPACE_PREFIX: &str = "tipg_tenant_";

/// Parse username in format "tenant_id.user" or "tenant_id:user" into (keyspace, actual_user).
/// The tenant_id is mapped to a TiKV keyspace by prepending KEYSPACE_PREFIX.
/// If no separator found, returns (None, username) - no keyspace override.
pub(super) fn parse_tenant_username(username: &str) -> (Option<String>, String) {
    if let Some((tenant_id, user)) = username
        .find('.')
        .map(|pos| (&username[..pos], &username[pos + 1..]))
        .or_else(|| {
            username
                .find(':')
                .map(|pos| (&username[..pos], &username[pos + 1..]))
        })
    {
        if !tenant_id.is_empty() && !user.is_empty() {
            let keyspace = format!("{KEYSPACE_PREFIX}{tenant_id}");
            return (Some(keyspace), user.to_string());
        }
    }
    (None, username.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dot_separator() {
        let (ks, user) = parse_tenant_username("abc123.admin");
        assert_eq!(ks.unwrap(), "tipg_tenant_abc123");
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_colon_separator() {
        let (ks, user) = parse_tenant_username("abc123:admin");
        assert_eq!(ks.unwrap(), "tipg_tenant_abc123");
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_no_separator() {
        let (ks, user) = parse_tenant_username("admin");
        assert!(ks.is_none());
        assert_eq!(user, "admin");
    }
}
