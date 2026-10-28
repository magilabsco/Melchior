//! Facts fixed at `initialize`: workspace, model, permissions.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::rpc::Notifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// File tools are confined to the workspace; shell runs in a sandbox.
    Workspace,
    /// `MELCHIOR_PERMISSION_MODE=danger-full-access` (or DeepSeek Harness's
    /// `DSH_PERMISSION_MODE`): no confinement. Meant for trusted, supervised
    /// agents only.
    FullAccess,
}

pub struct Runtime {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub max_tokens: Option<i64>,
    pub permission: Permission,
    pub notifier: Arc<Notifier>,
    secret_envs: Mutex<Vec<String>>,
}

/// Variables that carry credentials for the harness itself and must never
/// reach a child process. Plugins add their own with `add_secret_env`.
const SECRET_ENVS: &[&str] = &["DEEPSEEK_API_KEY", "OPENROUTER_API_KEY", "LEVIATHAN_TOKEN", "DSH_SYSTEM_PROMPT"];

impl Runtime {
    pub fn new(
        cwd: PathBuf,
        home: PathBuf,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
        max_tokens: Option<i64>,
        notifier: Arc<Notifier>,
    ) -> Runtime {
        let mode = std::env::var("MELCHIOR_PERMISSION_MODE").or_else(|_| std::env::var("DSH_PERMISSION_MODE"));
        let permission = match mode.as_deref() {
            Ok("danger-full-access") => Permission::FullAccess,
            _ => Permission::Workspace,
        };
        Runtime {
            cwd,
            home,
            provider,
            model,
            reasoning_effort,
            max_tokens,
            permission,
            notifier,
            secret_envs: Mutex::new(SECRET_ENVS.iter().map(|s| s.to_string()).collect()),
        }
    }

    pub fn full_access(&self) -> bool {
        self.permission == Permission::FullAccess
    }

    pub fn add_secret_env(&self, name: &str) {
        let mut secrets = self.secret_envs.lock().unwrap();
        if !secrets.iter().any(|s| s == name) {
            secrets.push(name.to_string());
        }
    }

    /// Environment for tool subprocesses: the harness environment minus its credentials.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let secrets = self.secret_envs.lock().unwrap();
        std::env::vars().filter(|(k, _)| !secrets.iter().any(|s| s == k)).collect()
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() { path.to_path_buf() } else { self.cwd.join(path) }
    }
}
