use std::{collections::BTreeMap, env, fs, path::PathBuf};

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct SystemInfo {
    os: &'static str,
    arch: &'static str,
    hostname: Option<String>,
    kernel: Option<String>,
    logical_cpus: Option<usize>,
    memory_total_kib: Option<u64>,
    tools: BTreeMap<&'static str, bool>,
}

pub async fn system_info() -> Json<SystemInfo> {
    let mut tools = BTreeMap::new();
    for tool in ["git", "rustc", "cargo", "node", "npm", "bun"] {
        tools.insert(tool, command_exists(tool));
    }

    Json(SystemInfo {
        os: env::consts::OS,
        arch: env::consts::ARCH,
        hostname: read_trimmed("/etc/hostname").or_else(|| env::var("HOSTNAME").ok()),
        kernel: read_trimmed("/proc/sys/kernel/osrelease"),
        logical_cpus: std::thread::available_parallelism().ok().map(usize::from),
        memory_total_kib: read_memory_total_kib(),
        tools,
    })
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_memory_total_kib() -> Option<u64> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    raw.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?.trim();
        rest.split_whitespace().next()?.parse().ok()
    })
}

pub(crate) fn command_exists(command: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path).any(|directory| {
        let candidate = directory.join(command);
        is_executable_file(candidate)
    })
}

#[cfg(unix)]
fn is_executable_file(path: PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: PathBuf) -> bool {
    path.is_file()
}
