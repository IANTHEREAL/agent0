//! Maps `fsplane.v2.FsError` (handler-level domain errors) and gRPC
//! `tonic::Status` (interceptor / transport errors) onto db9-server's
//! `EmbeddedFsError`.
//!
//! Two distinct surfaces, mirroring fs9's hybrid error contract
//! (DESIGN.md §Q1):
//!
//! - **Handler errors:** a successful gRPC `Status` (code=OK) with the
//!   response message carrying `oneof result { Success | FsError }`.
//!   The handler ran; the error rides the envelope.
//! - **Interceptor / transport errors:** non-OK `tonic::Status` carrying
//!   `AuthFailureDetail` in `Status.details`. Includes auth failures
//!   (A1-A5) and transport problems (deadline, connection reset).
//!
//! Both surfaces eventually surface to db9-server callers as
//! `anyhow::Error` wrapping `EmbeddedFsError`. That keeps the upstream
//! `is_not_found_error` etc. helpers working without touching them.

#![cfg(fsplane_v2_generated)]

use anyhow::anyhow;

use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::grpc::proto::{AuthFailureDetail, FsError as ProtoFsError, FsErrorCode};

/// Convert a domain-level `FsError` (the envelope payload) into an
/// `anyhow::Error` carrying a typed `EmbeddedFsError`. The `context`
/// argument is appended to the message to preserve caller intent
/// (`stat /foo` vs `readdir /foo`).
pub(crate) fn fs_error_to_anyhow(err: ProtoFsError, context: &str) -> anyhow::Error {
    // prost 0.12 keeps the proto enum prefix on Rust variants. The
    // proto package and the variants are namespaced together as
    // `FsErrorCode::FsError<Name>`, mirroring the on-wire enum names.
    let code = FsErrorCode::try_from(err.code).unwrap_or(FsErrorCode::FsErrorUnspecified);
    let msg = format_message(context, &err.message);
    match code {
        FsErrorCode::FsErrorNotFound => anyhow!(EmbeddedFsError::NotFound(msg)),
        FsErrorCode::FsErrorPermissionDenied => anyhow!(EmbeddedFsError::PermissionDenied(msg)),
        FsErrorCode::FsErrorAlreadyExists => anyhow!(EmbeddedFsError::AlreadyExists(msg)),
        FsErrorCode::FsErrorNotDirectory => anyhow!(EmbeddedFsError::NotDirectory(msg)),
        FsErrorCode::FsErrorIsDirectory => anyhow!(EmbeddedFsError::IsDirectory(msg)),
        FsErrorCode::FsErrorInvalidArgument => anyhow!(EmbeddedFsError::InvalidInput(msg)),
        FsErrorCode::FsErrorNoSpace | FsErrorCode::FsErrorTooLarge => {
            anyhow!(EmbeddedFsError::TooLarge(msg))
        }
        FsErrorCode::FsErrorNotEmpty => anyhow!(EmbeddedFsError::DirectoryNotEmpty(msg)),
        FsErrorCode::FsErrorVolumeNotAccessible => {
            anyhow!(EmbeddedFsError::PermissionDenied(msg))
        }
        // Upload-state errors and integrity failures don't map cleanly to
        // the embedded surface. Treat as Internal so callers see a
        // typed-but-opaque error; the message preserves the proto code
        // for log inspection.
        FsErrorCode::FsErrorUploadBusy
        | FsErrorCode::FsErrorUploadTerminal
        | FsErrorCode::FsErrorWrongOwner
        | FsErrorCode::FsErrorOffsetMismatch
        | FsErrorCode::FsErrorDataLoss
        | FsErrorCode::FsErrorIo
        | FsErrorCode::FsErrorUnspecified => {
            anyhow!(EmbeddedFsError::Internal(format!("[{:?}] {}", code, msg)))
        }
    }
}

/// Convert a `tonic::Status` (interceptor reject or transport failure)
/// into an `anyhow::Error`. AuthFailureDetail rides as a `prost-encoded`
/// blob in `status.details()`; decoding is best-effort and only used to
/// pick a more specific `EmbeddedFsError` variant.
pub(crate) fn status_to_anyhow(status: tonic::Status, context: &str) -> anyhow::Error {
    let detail = decode_auth_failure_detail(&status);
    let code_label = detail
        .as_ref()
        .map(|d| format!("{:?}", d.code()))
        .unwrap_or_else(|| format!("{:?}", status.code()));
    let reason_class = detail
        .as_ref()
        .map(|d| d.reason_class.as_str())
        .unwrap_or("");
    let suffix = if reason_class.is_empty() {
        String::new()
    } else {
        format!(" [reason_class={reason_class}]")
    };
    let msg = format!(
        "{}: {} [code={code_label}]{suffix}",
        context,
        status.message()
    );

    match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
            anyhow!(EmbeddedFsError::PermissionDenied(msg))
        }
        tonic::Code::NotFound => anyhow!(EmbeddedFsError::NotFound(msg)),
        tonic::Code::AlreadyExists => anyhow!(EmbeddedFsError::AlreadyExists(msg)),
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => {
            anyhow!(EmbeddedFsError::InvalidInput(msg))
        }
        tonic::Code::ResourceExhausted => anyhow!(EmbeddedFsError::TooLarge(msg)),
        _ => anyhow!(EmbeddedFsError::Internal(msg)),
    }
}

/// Decode `AuthFailureDetail` if any `prost-types::Any` in
/// `status.details()` carries one. Returns `None` for non-auth errors.
///
/// `tonic::Status::details()` returns raw bytes (`Vec<u8>`) which are the
/// prost-encoded value of `google.rpc.Status` OR a single Any —
/// implementations differ. We try the simplest path: treat the bytes
/// directly as an `AuthFailureDetail`. fs9's interceptor sends the
/// detail as a top-level Any whose value is the encoded
/// `AuthFailureDetail`, so this single decode covers it; non-matching
/// payloads fall through to `None` without panicking.
fn decode_auth_failure_detail(status: &tonic::Status) -> Option<AuthFailureDetail> {
    use prost::Message;
    let bytes = status.details();
    if bytes.is_empty() {
        return None;
    }
    AuthFailureDetail::decode(bytes).ok()
}

fn format_message(context: &str, server_message: &str) -> String {
    if server_message.is_empty() {
        context.to_string()
    } else {
        format!("{context}: {server_message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_error(code: FsErrorCode, msg: &str) -> ProtoFsError {
        ProtoFsError {
            code: code as i32,
            message: msg.to_string(),
            context: Default::default(),
        }
    }

    // Aliases so the assertion-side reads naturally. prost 0.12 names
    // each variant `FsError<Name>`; spelling that out at every site below
    // would obscure the intent.
    const NF: FsErrorCode = FsErrorCode::FsErrorNotFound;
    const ISDIR: FsErrorCode = FsErrorCode::FsErrorIsDirectory;
    const INVALID: FsErrorCode = FsErrorCode::FsErrorInvalidArgument;
    const NOT_EMPTY: FsErrorCode = FsErrorCode::FsErrorNotEmpty;
    const TOO_LARGE: FsErrorCode = FsErrorCode::FsErrorTooLarge;
    const NO_SPACE: FsErrorCode = FsErrorCode::FsErrorNoSpace;
    const VOL_NA: FsErrorCode = FsErrorCode::FsErrorVolumeNotAccessible;
    const UP_TERM: FsErrorCode = FsErrorCode::FsErrorUploadTerminal;
    const DATA_LOSS: FsErrorCode = FsErrorCode::FsErrorDataLoss;

    fn downcast(err: &anyhow::Error) -> &EmbeddedFsError {
        err.downcast_ref::<EmbeddedFsError>()
            .expect("expected typed EmbeddedFsError")
    }

    #[test]
    fn not_found_maps_to_embedded_not_found() {
        let err = fs_error_to_anyhow(fs_error(NF, "no such file"), "stat /a");
        assert!(matches!(downcast(&err), EmbeddedFsError::NotFound(_)));
        assert!(err.to_string().contains("stat /a"));
        assert!(err.to_string().contains("no such file"));
    }

    #[test]
    fn is_directory_maps_to_embedded_is_directory() {
        let err = fs_error_to_anyhow(fs_error(ISDIR, "/dir"), "read_file /dir");
        assert!(matches!(downcast(&err), EmbeddedFsError::IsDirectory(_)));
    }

    #[test]
    fn invalid_argument_maps_to_invalid_input() {
        let err = fs_error_to_anyhow(fs_error(INVALID, "bad offset"), "write_at /a");
        assert!(matches!(downcast(&err), EmbeddedFsError::InvalidInput(_)));
    }

    #[test]
    fn not_empty_maps_to_directory_not_empty() {
        let err = fs_error_to_anyhow(fs_error(NOT_EMPTY, "/dir"), "remove /dir");
        assert!(matches!(
            downcast(&err),
            EmbeddedFsError::DirectoryNotEmpty(_)
        ));
    }

    #[test]
    fn too_large_maps_to_too_large() {
        let err = fs_error_to_anyhow(fs_error(TOO_LARGE, "4 MiB cap"), "put /a");
        assert!(matches!(downcast(&err), EmbeddedFsError::TooLarge(_)));
    }

    #[test]
    fn no_space_also_maps_to_too_large_for_embedded() {
        // EmbeddedFsError has no dedicated NoSpace variant; v1 collapsed
        // both into TooLarge. v2 separates them on the wire but we map
        // them to the same Rust variant for caller compatibility.
        let err = fs_error_to_anyhow(fs_error(NO_SPACE, "full"), "put /a");
        assert!(matches!(downcast(&err), EmbeddedFsError::TooLarge(_)));
    }

    #[test]
    fn volume_not_accessible_maps_to_permission_denied() {
        let err = fs_error_to_anyhow(fs_error(VOL_NA, ""), "stat /a");
        assert!(matches!(
            downcast(&err),
            EmbeddedFsError::PermissionDenied(_)
        ));
    }

    #[test]
    fn upload_state_codes_map_to_internal_with_label() {
        let err = fs_error_to_anyhow(fs_error(UP_TERM, "rebegin"), "commit upload-123");
        let s = err.to_string();
        assert!(matches!(downcast(&err), EmbeddedFsError::Internal(_)));
        assert!(s.contains("UploadTerminal"));
    }

    #[test]
    fn data_loss_maps_to_internal() {
        let err = fs_error_to_anyhow(fs_error(DATA_LOSS, "sha mismatch"), "commit upload-1");
        assert!(matches!(downcast(&err), EmbeddedFsError::Internal(_)));
    }

    #[test]
    fn empty_server_message_falls_back_to_context() {
        let err = fs_error_to_anyhow(fs_error(NF, ""), "stat /a");
        assert_eq!(err.to_string(), "fs: NotFound: stat /a");
    }

    #[test]
    fn status_unauthenticated_maps_to_permission_denied() {
        let st = tonic::Status::unauthenticated("fs-plane: token missing");
        let err = status_to_anyhow(st, "stat /a");
        assert!(matches!(
            downcast(&err),
            EmbeddedFsError::PermissionDenied(_)
        ));
        assert!(err.to_string().contains("Unauthenticated"));
    }

    #[test]
    fn status_resource_exhausted_maps_to_too_large() {
        let st = tonic::Status::resource_exhausted("quota");
        let err = status_to_anyhow(st, "batch_inline_read");
        assert!(matches!(downcast(&err), EmbeddedFsError::TooLarge(_)));
    }

    #[test]
    fn status_unknown_maps_to_internal() {
        let st = tonic::Status::unknown("connection reset");
        let err = status_to_anyhow(st, "readdir /");
        assert!(matches!(downcast(&err), EmbeddedFsError::Internal(_)));
    }
}
