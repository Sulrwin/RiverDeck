//! Best-effort registry for subprocesses spawned by RiverDeck.
//!
//! Motivation:
//! - When the host is force-killed (IDE stop button, SIGKILL), child plugin processes can outlive
//!   the parent and keep ports/files/devices busy.
//! - We can't intercept SIGKILL, so we persist PIDs + lightweight verification hints and reap them
//!   on next startup.
//!
//! This is intentionally conservative: on Linux we verify `/proc/<pid>/cmdline` contains all
//! configured substrings before sending signals.

use crate::shared::config_dir;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessRecord {
    pid: u32,
    kind: String,
    verify_substrings: Vec<String>,
    added_at_unix_secs: u64,
}

fn registry_path() -> PathBuf {
    config_dir().join("runtime").join("processes.json")
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_registry() -> Vec<ProcessRecord> {
    let path = registry_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return vec![];
    };
    serde_json::from_slice::<Vec<ProcessRecord>>(&bytes).unwrap_or_default()
}

fn write_registry(records: &[ProcessRecord]) {
    let path = registry_path();
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let tmp = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec_pretty(records) else {
        return;
    };
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Record a subprocess PID to the registry so it can be cleaned up on shutdown/startup.
pub fn record_process(pid: u32, kind: &str, verify_substrings: Vec<String>) {
    if pid == 0 {
        return;
    }
    let mut records = read_registry();
    records.retain(|r| r.pid != pid);
    records.push(ProcessRecord {
        pid,
        kind: kind.to_owned(),
        verify_substrings,
        added_at_unix_secs: now_unix_secs(),
    });
    write_registry(&records);
}

/// Remove a PID from the registry (best-effort).
pub fn unrecord_process(pid: u32) {
    if pid == 0 {
        return;
    }
    let mut records = read_registry();
    let before = records.len();
    records.retain(|r| r.pid != pid);
    if records.len() != before {
        write_registry(&records);
    }
}

#[cfg(target_os = "linux")]
fn linux_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    // cmdline is NUL-separated args.
    let s = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    Some(s)
}

#[cfg(unix)]
fn unix_signal_pid(pid: u32, sig: i32) {
    if pid == 0 {
        return;
    }
    let _ = unsafe { libc::kill(pid as i32, sig) };
}

/// Best-effort cleanup of any recorded subprocesses.
///
/// Intended to be called at startup, after the single-instance lock is held.
pub fn cleanup_orphaned_processes() {
    let current_pid = std::process::id();
    let mut records = read_registry();
    if records.is_empty() {
        return;
    }

    let mut kept: Vec<ProcessRecord> = Vec::new();
    for rec in records.drain(..) {
        if rec.pid == current_pid {
            kept.push(rec);
            continue;
        }

        // Only attempt to kill if the target still exists and (on Linux) matches our verification.
        #[cfg(target_os = "linux")]
        {
            let Some(cmdline) = linux_cmdline(rec.pid) else {
                // Process already gone.
                continue;
            };
            if !rec
                .verify_substrings
                .iter()
                .all(|needle| cmdline.contains(needle))
            {
                // PID reused or not ours; keep the record so we don't churn the file.
                kept.push(rec);
                continue;
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            // On non-Linux platforms we don't have a safe universal verification mechanism here.
            // Keep the record rather than risking killing an unrelated process.
            kept.push(rec);
            continue;
        }

        #[cfg(unix)]
        {
            // SIGTERM then SIGKILL.
            unix_signal_pid(rec.pid, libc::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(250));
            unix_signal_pid(rec.pid, libc::SIGKILL);
        }
    }

    write_registry(&kept);
}

