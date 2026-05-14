//! fs9 v2 gRPC backend — connects to the fs9 v2 public listener over TLS.
//!
//! Implements `FsBackend` by translating each trait method into a
//! `fsplane.v2.FsPlane` (or `FsPlaneAdmin`) RPC. The contract differs from
//! the deleted v1 backend in three ways that shape this module:
//!
//! 1. **Authentication:** every RPC carries an `authorization: Bearer <jwt>`
//!    metadata, where the JWT has `aud="fs-plane"`, `tid=<tenant>`, and a
//!    `scp` covering the requested mode. db9-server asks auth9
//!    `POST /v1/jwt/sign` for the token (see `auth::fs_plane_token` and
//!    `docs/design/fs9_auth9_direct_mint.md`); it is NEVER an issuer.
//! 2. **Response envelope:** every unary response has the shape
//!    `oneof result { <Success> success; FsError error }`. Domain failures
//!    ride the `error` arm with a typed `FsErrorCode`. gRPC `Status` is
//!    reserved for interceptor rejections (auth) and transport errors.
//! 3. **FileMeta:** Stat/Readdir/Mkdir/PutFile/… nest a `FileMeta` payload
//!    with `mtime_ns` (not v1's `mtime_ms`) and an opaque CAS `version`
//!    field. The adapter sits in `client.rs` and is the single conversion
//!    point to db9-server's `FsFileInfo`.

#[allow(clippy::all)]
#[cfg(fsplane_v2_generated)]
pub(crate) mod proto {
    tonic::include_proto!("fsplane.v2");
}

#[cfg(fsplane_v2_generated)]
pub(crate) mod client;

#[cfg(fsplane_v2_generated)]
pub(crate) mod connector;

#[cfg(fsplane_v2_generated)]
pub(crate) mod errors;

#[cfg(fsplane_v2_generated)]
pub(crate) mod meta;
