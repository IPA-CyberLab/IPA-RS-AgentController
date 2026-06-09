use crate::command::CommandRunner;
use crate::model::Env;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
        let base = package_versions(base_manifest)?;
        let env = package_versions(env_manifest)?;
        let mut out = String::new();
        for (pkg, version) in &env {
            match base.get(pkg) {
                None => out.push_str(&format!("installed {pkg} {version}\n")),
                Some(base_version) if base_version != version => {
                    out.push_str(&format!("upgraded {pkg} {base_version} -> {version}\n"));
                }
                _ => {}
            }
        }
        for (pkg, version) in &base {
            if !env.contains_key(pkg) {
                out.push_str(&format!("removed {pkg} {version}\n"));
            }
        }
        Ok(out)
    }

    pub fn changed_paths_by_walk(base: &Path, env: &Path) -> Result<String> {
        let mut changed = BTreeSet::new();
        for entry in WalkDir::new(env).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(env)?;
            let base_path = base.join(rel);
            if !base_path.exists() || files_differ(&base_path, entry.path())? {
                changed.insert(format!("/{}", rel.display()));
            }
        }
        for entry in WalkDir::new(base).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(base)?;
            if !env.join(rel).exists() {
                changed.insert(format!("deleted /{}", rel.display()));
            }
        }
        Ok(changed.into_iter().collect::<Vec<_>>().join("\n"))
    }
}

fn files_differ(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = fs::metadata(left)?;
    let right_meta = fs::metadata(right)?;
    if left_meta.len() != right_meta.len() {
        return Ok(true);
    }
    Ok(fs::read(left)? != fs::read(right)?)
}

fn package_versions(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path)?;
    let mut packages = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let version = fields.next().unwrap_or("unknown");
        packages.insert(name.to_string(), version.to_string());
    }
    Ok(packages)
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
        fs::write(&base, "bash 1.0\ncurl 8.0\n").unwrap();
        fs::write(&env, "bash 1.0\nripgrep 14.0\n").unwrap();
        let delta = Exporter::dpkg_delta(&base, &env).unwrap();
        assert!(delta.contains("installed ripgrep 14.0"));
        assert!(delta.contains("removed curl 8.0"));
    }

    #[test]
    fn dpkg_delta_reports_upgrades() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env = dir.path().join("env");
        fs::write(&base, "bash 1.0\ncurl 8.0\n").unwrap();
        fs::write(&env, "bash 1.1\ncurl 8.0\n").unwrap();
        let delta = Exporter::dpkg_delta(&base, &env).unwrap();
        assert!(delta.contains("upgraded bash 1.0 -> 1.1"));
        assert!(!delta.contains("curl"));
    }

    #[test]
    fn changed_paths_reports_added_modified_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env = dir.path().join("env");
        fs::create_dir_all(base.join("root")).unwrap();
        fs::create_dir_all(env.join("root")).unwrap();
        fs::write(base.join("root/old.txt"), "old").unwrap();
        fs::write(base.join("root/same.txt"), "same").unwrap();
        fs::write(base.join("root/delete.txt"), "delete").unwrap();
        fs::write(env.join("root/old.txt"), "new").unwrap();
        fs::write(env.join("root/same.txt"), "same").unwrap();
        fs::write(env.join("root/add.txt"), "add").unwrap();

        let changed = Exporter::changed_paths_by_walk(&base, &env).unwrap();
        assert!(changed.contains("/root/add.txt"));
        assert!(changed.contains("/root/old.txt"));
        assert!(changed.contains("deleted /root/delete.txt"));
        assert!(!changed.contains("/root/same.txt"));
    }
}
