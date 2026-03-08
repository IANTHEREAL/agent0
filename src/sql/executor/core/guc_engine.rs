//! Centralized reserved pseudo-GUC classification and write guards.

use crate::sql::error::SqlError;
use crate::sql::session::settings::KNOWN_GUCS;
use crate::sql::session::SessionSettings;
use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GucValueInput {
    DefaultKeyword,
    Literal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GucKind {
    ReadOnlyPseudo,
    SessionAuthPseudo,
    SearchPath,
    Known,
    UnknownCompat,
}

pub(super) fn classify_guc(name: &str) -> GucKind {
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
        "is_superuser" => return GucKind::ReadOnlyPseudo,
        "session_authorization" => return GucKind::SessionAuthPseudo,
        _ => {}
    }

    let canonical = SessionSettings::canonical_setting_name(&lowered);
    if canonical == "search_path" {
        return GucKind::SearchPath;
    }
    if KNOWN_GUCS.iter().any(|g| g.name == canonical) {
        return GucKind::Known;
    }
    GucKind::UnknownCompat
}

pub(crate) fn check_reserved_guc_write(name: &str, input: &GucValueInput) -> Result<()> {
    match classify_guc(name) {
        GucKind::ReadOnlyPseudo => Err(SqlError::InvalidParameterValue {
            message: "parameter \"is_superuser\" cannot be changed".to_string(),
        }
        .into()),
        GucKind::SessionAuthPseudo => match input {
            GucValueInput::DefaultKeyword => Ok(()),
            GucValueInput::Literal(_) => Err(SqlError::InvalidParameterValue {
                message: "parameter \"session_authorization\" cannot be changed".to_string(),
            }
            .into()),
        },
        GucKind::SearchPath | GucKind::Known | GucKind::UnknownCompat => Ok(()),
    }
}

pub(super) fn check_reserved_guc_reset(name: &str) -> Result<()> {
    match classify_guc(name) {
        GucKind::ReadOnlyPseudo => Err(SqlError::InvalidParameterValue {
            message: "parameter \"is_superuser\" cannot be changed".to_string(),
        }
        .into()),
        GucKind::SessionAuthPseudo => Ok(()),
        GucKind::SearchPath | GucKind::Known | GucKind::UnknownCompat => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_reserved_guc_reset, check_reserved_guc_write, classify_guc, GucKind, GucValueInput,
    };

    #[test]
    fn classify_reserved_and_compat_names() {
        assert_eq!(classify_guc("is_superuser"), GucKind::ReadOnlyPseudo);
        assert_eq!(
            classify_guc("session_authorization"),
            GucKind::SessionAuthPseudo
        );
        assert_eq!(classify_guc("search_path"), GucKind::SearchPath);
        assert_eq!(classify_guc("statement_timeout"), GucKind::Known);
        assert_eq!(
            classify_guc("session.authorization"),
            GucKind::UnknownCompat
        );
    }

    #[test]
    fn reserved_pseudo_write_and_reset_rules_match_pg_compat() {
        let err = check_reserved_guc_write("is_superuser", &GucValueInput::Literal("on".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("parameter \"is_superuser\" cannot be changed"));

        check_reserved_guc_write("session_authorization", &GucValueInput::DefaultKeyword).unwrap();

        let err =
            check_reserved_guc_write("session_authorization", &GucValueInput::Literal("x".into()))
                .unwrap_err()
                .to_string();
        assert!(err.contains("parameter \"session_authorization\" cannot be changed"));

        check_reserved_guc_reset("session_authorization").unwrap();
    }
}
