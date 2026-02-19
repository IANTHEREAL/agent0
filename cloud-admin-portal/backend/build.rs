fn main() {
    // Check env var first (set via Docker --build-arg), fall back to git command
    let git_hash = std::env::var("BUILD_GIT_HASH").ok().unwrap_or_else(|| {
        let hash = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let dirty = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);

        if dirty {
            format!("{}-dirty", hash)
        } else {
            hash
        }
    });

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
}
