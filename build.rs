use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Compile fsplane.v2 proto into Rust client stubs. Mirrors db9-cli's
    // pattern: emit into OUT_DIR, advertise `fsplane_v2_generated` cfg so
    // the consuming module can `tonic::include_proto!("fsplane.v2")`.
    //
    // The proto file lives at proto/fsplane/v2/fsplane.proto and tracks
    // db9-ai/fs9's upstream copy; drift is a CI failure.
    //
    // `DB9_REQUIRE_PROTOC=1` makes proto-compile failure a hard error.
    // CI sets it so a CI box that loses protoc fails fast instead of
    // silently building without fs9 gRPC backend support. Dev builds
    // leave it unset, preserving graceful degradation for embedded-only
    // workstations.
    println!("cargo:rustc-check-cfg=cfg(fsplane_v2_generated)");
    println!("cargo:rerun-if-changed=proto/fsplane/v2/fsplane.proto");
    println!("cargo:rerun-if-changed=proto/fsplane/v2");
    println!("cargo:rerun-if-env-changed=DB9_REQUIRE_PROTOC");
    let require_protoc = std::env::var("DB9_REQUIRE_PROTOC")
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0");
    match tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(&["proto/fsplane/v2/fsplane.proto"], &["proto"])
    {
        Ok(()) => {
            println!("cargo:rustc-cfg=fsplane_v2_generated");
        }
        Err(e) => {
            if require_protoc {
                panic!(
                    "fsplane.v2 proto compile failed and DB9_REQUIRE_PROTOC is set: {e}\n\
                     install `protoc` (apt: `protobuf-compiler`) or unset DB9_REQUIRE_PROTOC \
                     to fall back to embedded-only builds."
                );
            }
            println!("cargo:warning=fsplane.v2 proto compile skipped: {e}");
            println!("cargo:warning=db9-server will build without fs9 gRPC backend support");
        }
    }

    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");

    let git_hash = std::env::var("BUILD_GIT_HASH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=8", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        });

    let build_date = std::env::var("BUILD_DATE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            Command::new("date")
                .args(["-u", "+%Y-%m-%d"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        });

    println!("cargo:rustc-env=BUILD_GIT_HASH={git_hash}");
    println!("cargo:rustc-env=BUILD_DATE={build_date}");
}
