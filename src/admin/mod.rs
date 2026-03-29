// Layer 2 (external API/auth entrypoint) will consume the full public surface.
// Suppress dead_code warnings for types/methods that are tested but not yet
// wired to an external entrypoint.
#[allow(dead_code)]
pub mod audit;
#[allow(dead_code)]
pub mod control;
#[allow(dead_code)]
pub mod session_registry;

pub use session_registry::{global_session_registry, SessionState};
