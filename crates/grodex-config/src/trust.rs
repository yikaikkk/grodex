use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct WorkspaceTrustBinding {
    pub workspace_canonical_path: String,
    pub config_fingerprint: String,
    pub high_risk_keys: Vec<String>,
    pub repository_remote_fingerprint: Option<String>,
    pub binding_hash: String,
}

impl WorkspaceTrustBinding {
    pub fn compute(
        workspace_path: &Path,
        config_fingerprint: String,
        high_risk_keys: Vec<String>,
    ) -> Self {
        let workspace_canonical_path = workspace_path
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| workspace_path.to_string_lossy().to_string());

        let repository_remote_fingerprint = git_remote_origin(workspace_path);

        let remote_str = repository_remote_fingerprint.clone().unwrap_or_default();
        let high_risk_joined = high_risk_keys.join("|");

        let binding_input = format!(
            "WSBINDv1\n{canonical}\n{fingerprint}\n{high_risk}\n{remote}",
            canonical = workspace_canonical_path,
            fingerprint = config_fingerprint,
            high_risk = high_risk_joined,
            remote = remote_str,
        );

        let mut hasher = Sha256::new();
        hasher.update(binding_input.as_bytes());
        let full_hash = format!("{:x}", hasher.finalize());
        let binding_hash = full_hash.chars().take(16).collect();

        Self {
            workspace_canonical_path,
            config_fingerprint,
            high_risk_keys,
            repository_remote_fingerprint,
            binding_hash,
        }
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn git_remote_origin(workspace_path: &Path) -> Option<String> {
    let repo_root = find_git_root(workspace_path).unwrap_or_else(|| workspace_path.to_path_buf());
    let output = std::process::Command::new("git")
        .args([
            "-C",
            repo_root.to_string_lossy().as_ref(),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                None
            } else {
                Some(stdout)
            }
        }
        _ => None,
    }
}
