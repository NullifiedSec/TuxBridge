use std::{fs, process::Command};

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{security::SecurityProfile, state::AppState, system::command_exists};

#[derive(Debug, Serialize)]
pub struct DoctorResponse {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    name: String,
    status: CheckStatus,
    message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus { Ok, Warning, Error }

pub async fn doctor(State(state): State<AppState>) -> Json<DoctorResponse> {
    let mut checks = Vec::new();

    for tool in ["git", "rustc", "cargo", "node", "npm", "bun", "ss", "df"] {
        let available = command_exists(tool);
        checks.push(DoctorCheck {
            name: format!("tool:{tool}"),
            status: if available { CheckStatus::Ok } else { CheckStatus::Warning },
            message: if available { format!("{tool} is available") } else { format!("{tool} is not available on PATH") },
        });
    }

    let (profile_status, profile_message) = match state.config.security.profile {
        SecurityProfile::Default => (
            CheckStatus::Ok,
            format!("default profile active; constrained commands: {}", state.config.security.default_command_allowlist.join(", ")),
        ),
        SecurityProfile::Loose => (
            CheckStatus::Warning,
            "loose profile active; raw shell and general command APIs have the full permissions of the TuxBridge Unix account".into(),
        ),
        SecurityProfile::IWantToNukeMyServer => (
            CheckStatus::Warning,
            "i_want_to_nuke_my_server profile active; installer is expected to grant passwordless sudo, so the API key must be treated as a root credential".into(),
        ),
    };
    checks.push(DoctorCheck {
        name: "security-profile".into(),
        status: profile_status,
        message: profile_message,
    });

    if effective_uid().is_some_and(|uid| uid == 0) {
        checks.push(DoctorCheck {
            name: "service-user".into(),
            status: CheckStatus::Warning,
            message: "TuxBridge is running as root; use the dedicated tuxbridge service account".into(),
        });
    } else {
        checks.push(DoctorCheck {
            name: "service-user".into(),
            status: CheckStatus::Ok,
            message: "TuxBridge is not running as root".into(),
        });
    }

    checks.push(DoctorCheck {
        name: "limits".into(),
        status: CheckStatus::Ok,
        message: format!(
            "body={}B in_flight={} command_timeout={}..{}s command_output={}B jobs={} retention={}s",
            state.config.limits.max_body_bytes,
            state.config.limits.max_in_flight,
            state.config.limits.command_timeout_seconds,
            state.config.limits.max_command_timeout_seconds,
            state.config.limits.command_output_bytes,
            state.config.limits.max_jobs,
            state.config.limits.job_retention_seconds,
        ),
    });

    for (name, workspace) in &state.config.workspaces {
        match fs::canonicalize(&workspace.root) {
            Ok(root) if root.is_dir() => {
                checks.push(DoctorCheck {
                    name: format!("workspace:{name}"),
                    status: CheckStatus::Ok,
                    message: format!("{} is accessible", root.display()),
                });
                if workspace.capabilities.git_read || workspace.capabilities.git_write || workspace.capabilities.git_network {
                    checks.push(check_git_workspace(name, &root));
                }
                if workspace.capabilities.git_write || workspace.capabilities.git_network {
                    checks.push(DoctorCheck {
                        name: format!("workspace:{name}:git-execution-risk"),
                        status: CheckStatus::Warning,
                        message: "Git write/network operations may invoke repository filters, credential helpers, SSH, or remote helpers; expose only trusted repositories to the service user".into(),
                    });
                }
            }
            Ok(root) => checks.push(DoctorCheck {
                name: format!("workspace:{name}"),
                status: CheckStatus::Error,
                message: format!("{} is not a directory", root.display()),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: format!("workspace:{name}"),
                status: CheckStatus::Error,
                message: format!("workspace root is not accessible: {error}"),
            }),
        }
    }

    for (name, mount) in &state.config.user_files {
        match fs::canonicalize(&mount.root) {
            Ok(root) if root.is_dir() => checks.push(DoctorCheck {
                name: format!("user-files:{name}"),
                status: CheckStatus::Ok,
                message: format!("{} is accessible (read={}, write={})", root.display(), mount.read, mount.write),
            }),
            Ok(root) => checks.push(DoctorCheck {
                name: format!("user-files:{name}"),
                status: CheckStatus::Error,
                message: format!("{} is not a directory", root.display()),
            }),
            Err(error) => checks.push(DoctorCheck {
                name: format!("user-files:{name}"),
                status: CheckStatus::Error,
                message: format!("mount root is not accessible: {error}"),
            }),
        }
    }

    let ok = !checks.iter().any(|check| matches!(check.status, CheckStatus::Error));
    Json(DoctorResponse { ok, checks })
}

fn check_git_workspace(name: &str, root: &std::path::Path) -> DoctorCheck {
    if !command_exists("git") {
        return DoctorCheck {
            name: format!("workspace:{name}:git"),
            status: CheckStatus::Error,
            message: "Git capabilities are enabled but git is not available".into(),
        };
    }

    match Command::new("git").arg("-C").arg(root).args(["rev-parse", "--is-inside-work-tree"]).output() {
        Ok(output) if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true" => DoctorCheck {
            name: format!("workspace:{name}:git"),
            status: CheckStatus::Ok,
            message: "workspace is a Git work tree".into(),
        },
        Ok(output) => DoctorCheck {
            name: format!("workspace:{name}:git"),
            status: CheckStatus::Error,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        },
        Err(error) => DoctorCheck {
            name: format!("workspace:{name}:git"),
            status: CheckStatus::Error,
            message: format!("failed to execute git: {error}"),
        },
    }
}

fn effective_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}
