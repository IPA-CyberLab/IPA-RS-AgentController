use crate::command::CommandRunner;
use crate::model::Env;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportType {
    WorkspacePatch,
    RootfsChangedPaths,
    DpkgDelta,
}

impl ExportType {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "workspace-patch" => Ok(Self::WorkspacePatch),
            "rootfs-changed-paths" => Ok(Self::RootfsChangedPaths),
            "dpkg-delta" => Ok(Self::DpkgDelta),
            other => anyhow::bail!("unsupported export type {other}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Exporter {
    runner: CommandRunner,
}

impl Exporter {
    pub async fn workspace_patch(&self, env: &Env) -> Result<String> {
        let workspace = env.rootfs_path.join("workspace");
        if !workspace.join(".git").exists() {
            return Ok(String::new());
        }
        let output = self
            .runner
            .run(
                "git",
                vec![
                    "-C".to_string(),
                    workspace.display().to_string(),
                    "diff".to_string(),
                    "--binary".to_string(),
                ],
            )
            .await?;
        Ok(output.stdout)
    }

    pub fn dpkg_delta(base_manifest: &Path, env_manifest: &Path) -> Result<String> {
        let base = package_names(base_manifest)?;
        let env = package_names(env_manifest)?;
        let installed = env.difference(&base).cloned().collect::<Vec<_>>();
        let removed = base.difference(&env).cloned().collect::<Vec<_>>();
        let mut out = String::new();
        for pkg in installed {
            out.push_str(&format!("installed {pkg}\n"));
        }
        for pkg in removed {
            out.push_str(&format!("removed {pkg}\n"));
        }
        Ok(out)
    }

    pub fn changed_paths_by_walk(base: &Path, env: &Path) -> Result<String> {
        let mut paths = Vec::new();
        for entry in WalkDir::new(env).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(env)?;
            let base_path = base.join(rel);
            if !base_path.exists() {
                paths.push(format!("/{}", rel.display()));
            }
        }
        paths.sort();
        Ok(paths.join("\n"))
    }
}

fn package_names(path: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::Exporter;
    use std::fs;

    #[test]
    fn dpkg_delta_reports_installed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env = dir.path().join("env");
        fs::write(&base, "bash install\ncurl install\n").unwrap();
        fs::write(&env, "bash install\nripgrep install\n").unwrap();
        let delta = Exporter::dpkg_delta(&base, &env).unwrap();
        assert!(delta.contains("installed ripgrep"));
        assert!(delta.contains("removed curl"));
    }
}
