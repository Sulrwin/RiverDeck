//! High-level lifecycle helpers (startup cleanup, shutdown).
//!
//! Frontends should call these to ensure RiverDeck doesn't leave subprocesses running.

/// Best-effort cleanup of any orphaned subprocesses from a previous crashed instance.
///
/// Call this early during startup (after holding the single-instance lock).
pub fn startup_cleanup() {
    crate::runtime_processes::cleanup_orphaned_processes();
}

/// Best-effort global shutdown routine.
///
/// This focuses on subprocesses (plugins, helpers), since background tasks die with the runtime.
pub async fn shutdown_all() {
    // Kill any plugin instances we know about.
    let _ = crate::plugins::deactivate_all_plugins().await;

    // As a last-resort, try to kill anything recorded in the process registry too.
    // (This helps for SIGTERM shutdown paths where frontends might not drop cleanly.)
    crate::runtime_processes::cleanup_orphaned_processes();
}
