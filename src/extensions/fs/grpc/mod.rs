/// fs9 gRPC backend — connects to the fs9 proxy (db9-ai/fs9) over Unix socket.
/// Implements FsBackend by translating each method to a gRPC call.
pub(crate) mod client;

#[allow(clippy::all)]
pub(crate) mod proto {
    include!("proto/fsplane.v1.rs");
}

pub(crate) use client::create_channel as create_grpc_channel;
pub(crate) use client::GrpcFsBackend;
