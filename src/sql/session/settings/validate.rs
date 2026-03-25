use super::*;

pub(super) fn validate_timezone(value: &str) -> Result<String> {
    crate::model::timestamp::TimeZoneSpec::try_parse(value)?;
    Ok(value.to_string())
}

pub(super) fn validate_client_encoding(value: &str) -> Result<String> {
    let enc = value.trim();
    if enc.eq_ignore_ascii_case("utf8") || enc.eq_ignore_ascii_case("utf-8") {
        Ok("UTF8".to_string())
    } else {
        Err(SqlError::Unsupported(format!(
            "unsupported client_encoding '{}'; only UTF8 is supported",
            value
        ))
        .into())
    }
}

pub(super) fn validate_standard_conforming_strings(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => Ok("on".to_string()),
        "off" | "false" | "no" | "0" => Err(SqlError::Unsupported(
            "standard_conforming_strings = off is not supported; \
             the parser always treats backslashes literally"
                .to_string(),
        )
        .into()),
        _ => Err(SqlError::InvalidParameterValue {
            message: "parameter \"standard_conforming_strings\" requires a Boolean value".into(),
        }
        .into()),
    }
}

pub(super) fn validate_transaction_isolation(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "read uncommitted" | "read committed" => {
            tracing::warn!(
                requested = normalized.as_str(),
                actual = "repeatable read",
                "TiKV provides snapshot isolation (REPEATABLE READ); \
                 the requested isolation level has been upgraded"
            );
            Ok(normalized)
        }
        "repeatable read" => Ok("repeatable read".to_string()),
        "serializable" => {
            tracing::warn!(
                requested = "serializable",
                actual = "repeatable read",
                "TiKV cannot provide PostgreSQL SERIALIZABLE semantics; \
                 the requested isolation level has been downgraded"
            );
            Ok(normalized)
        }
        _ => Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"transaction_isolation\": \"{}\"",
                value
            ),
        }
        .into()),
    }
}

pub(super) fn validate_default_tablespace(value: &str) -> Result<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("pg_default") {
        Ok(String::new())
    } else {
        Err(SqlError::UndefinedObject(format!("tablespace \"{}\" does not exist", v)).into())
    }
}

pub(super) fn validate_datestyle(value: &str) -> Result<String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("ISO, MDY") || v.eq_ignore_ascii_case("ISO") {
        Ok("ISO, MDY".to_string())
    } else {
        Err(SqlError::InvalidParameterValue {
            message: format!("invalid value for parameter \"DateStyle\": \"{}\"", v),
        }
        .into())
    }
}

pub(super) fn validate_intervalstyle(value: &str) -> Result<String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("postgres") {
        Ok("postgres".to_string())
    } else {
        Err(SqlError::InvalidParameterValue {
            message: format!("invalid value for parameter \"IntervalStyle\": \"{}\"", v),
        }
        .into())
    }
}

pub(super) fn validate_session_replication_role(value: &str) -> Result<String> {
    let _ = value;
    Err(SqlError::Unsupported(
        "session_replication_role is not supported; \
         triggers always fire as in \"origin\" mode"
            .to_string(),
    )
    .into())
}

pub(super) fn validate_default_transaction_deferrable(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "off" | "false" | "no" | "0" => Ok("off".to_string()),
        "on" | "true" | "yes" | "1" => Err(SqlError::Unsupported(
            "DEFERRABLE transactions are not supported \
             (requires SERIALIZABLE isolation)"
                .to_string(),
        )
        .into()),
        _ => Err(SqlError::InvalidParameterValue {
            message: "parameter \"default_transaction_deferrable\" requires a Boolean value".into(),
        }
        .into()),
    }
}

pub(super) fn validate_transaction_deferrable(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "off" | "false" | "no" | "0" => Ok("off".to_string()),
        "on" | "true" | "yes" | "1" => Err(SqlError::Unsupported(
            "DEFERRABLE transactions are not supported \
             (requires SERIALIZABLE isolation)"
                .to_string(),
        )
        .into()),
        _ => Err(SqlError::InvalidParameterValue {
            message: "parameter \"transaction_deferrable\" requires a Boolean value".into(),
        }
        .into()),
    }
}

pub(super) fn validate_default_transaction_read_only(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => Ok("on".to_string()),
        "off" | "false" | "no" | "0" => Ok("off".to_string()),
        _ => Err(SqlError::InvalidParameterValue {
            message: "parameter \"default_transaction_read_only\" requires a Boolean value".into(),
        }
        .into()),
    }
}

pub(super) fn validate_bytea_output(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "hex" | "escape" => Ok(normalized),
        _ => Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"bytea_output\": \"{}\"; \
                 available values: \"hex\", \"escape\"",
                value
            ),
        }
        .into()),
    }
}

pub(super) fn validate_db9_use_optimizer(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => Ok(value.to_string()),
        "off" | "false" | "no" | "0" => {
            tracing::info!(
                "NOTICE: optimizer cannot be disabled; \
                 db9.use_optimizer setting ignored"
            );
            Ok(value.to_string())
        }
        _ => Err(SqlError::InvalidParameterValue {
            message: "parameter \"db9.use_optimizer\" requires a Boolean value".into(),
        }
        .into()),
    }
}

pub(super) fn validate_embedding_dimensions(value: &str) -> Result<String> {
    let v: u32 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"embedding.dimensions\": \"{}\"",
                value
            ),
        })?;
    if v == 0 {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"embedding.dimensions\": \"{}\" must be at least 1",
                value
            ),
        }
        .into());
    }
    Ok(v.to_string())
}

pub(super) fn validate_embedding_model(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding.model must not be empty".into(),
        }
        .into());
    }
    Ok(trimmed.to_string())
}

pub(super) fn validate_embedding_provider(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "openai" | "openai_compatible" | "openai-compatible" => Ok("openai".to_string()),
        "bedrock" | "aws_bedrock" | "aws-bedrock" => Ok("bedrock".to_string()),
        _ => Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"embedding.provider\": \"{}\"; \
                 expected 'openai' or 'bedrock'",
                value
            ),
        }
        .into()),
    }
}

pub(super) fn validate_embedding_endpoint(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding.endpoint must not be empty".into(),
        }
        .into());
    }
    Ok(trimmed.to_string())
}

pub(super) fn validate_embedding_api_key(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SqlError::InvalidParameterValue {
            message: "embedding.api_key must not be empty".into(),
        }
        .into());
    }
    Ok(trimmed.to_string())
}

pub(super) fn validate_embedding_positive_u32(value: &str) -> Result<String> {
    let v: u32 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!("invalid value for parameter: \"{}\"", value),
        })?;
    if v == 0 {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter: \"{}\" must be at least 1",
                value
            ),
        }
        .into());
    }
    Ok(v.to_string())
}

pub(super) fn validate_hnsw_ef_search(value: &str) -> Result<String> {
    let v: u16 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"hnsw.ef_search\": \"{}\"",
                value
            ),
        })?;
    if !(1..=1000).contains(&v) {
        return Err(SqlError::InvalidParameterValue {
            message: format!("hnsw.ef_search must be between 1 and 1000, got {}", v),
        }
        .into());
    }
    Ok(v.to_string())
}

pub(super) fn validate_db9_retry_max_attempts(value: &str) -> Result<String> {
    let v: u64 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"db9.retry_max_attempts\": \"{}\"",
                value
            ),
        })?;
    if v == 0 {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"db9.retry_max_attempts\": \"{}\" must be at least 1",
                value
            ),
        }
        .into());
    }
    Ok(v.to_string())
}

pub(super) fn validate_db9_dml_table_scan_max_rows(value: &str) -> Result<String> {
    let v: u128 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"db9.dml_table_scan_max_rows\": \"{}\"",
                value
            ),
        })?;
    if v > DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND as u128 {
        return Err(SqlError::InvalidParameterValue {
            message: format!(
                "invalid value for parameter \"db9.dml_table_scan_max_rows\": \"{}\" (must be between 0 and {})",
                value, DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND
            ),
        }
        .into());
    }
    Ok(v.to_string())
}

pub(super) fn validate_db9_positive_u64(value: &str) -> Result<String> {
    let v: u64 = value
        .trim()
        .parse()
        .map_err(|_| SqlError::InvalidParameterValue {
            message: format!("invalid value for parameter: \"{}\"", value),
        })?;
    Ok(v.to_string())
}
