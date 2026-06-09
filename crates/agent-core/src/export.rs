use crate::command::CommandRunner;
use crate::model::Env;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
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

    pub fn artifact_name(self) -> &'static str {
        match self {
            Self::WorkspacePatch => "workspace-patch.patch",
            Self::RootfsChangedPaths => "rootfs-changed-paths.txt",
            Self::DpkgDelta => "dpkg-delta.txt",
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
            .run_checked(
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
        for entry in WalkDir::new(env) {
            let entry = entry.with_context(|| format!("failed to walk {}", env.display()))?;
            if entry.depth() == 0 {
                continue;
            }
            let rel = entry.path().strip_prefix(env)?;
            if is_runtime_only_path(rel) {
                continue;
            }
            let base_path = base.join(rel);
            if path_changed(&base_path, entry.path())? {
                changed.insert(format!("/{}", rel.display()));
            }
        }
        for entry in WalkDir::new(base) {
            let entry = entry.with_context(|| format!("failed to walk {}", base.display()))?;
            if entry.depth() == 0 {
                continue;
            }
            let rel = entry.path().strip_prefix(base)?;
            if is_runtime_only_path(rel) {
                continue;
            }
            if symlink_metadata_if_exists(&env.join(rel))?.is_none() {
                changed.insert(format!("deleted /{}", rel.display()));
            }
        }
        Ok(changed.into_iter().collect::<Vec<_>>().join("\n"))
    }
}

fn path_changed(base: &Path, env: &Path) -> Result<bool> {
    let Some(base_meta) = symlink_metadata_if_exists(base)? else {
        return Ok(true);
    };
    let env_meta = fs::symlink_metadata(env)?;
    let base_type = base_meta.file_type();
    let env_type = env_meta.file_type();
    if metadata_changed(&base_meta, &env_meta) {
        return Ok(true);
    }
    if base_type.is_file() && env_type.is_file() {
        return files_differ(base, env);
    }
    if base_type.is_symlink() && env_type.is_symlink() {
        return Ok(fs::read_link(base)? != fs::read_link(env)?);
    }
    Ok(base_type.is_dir() != env_type.is_dir()
        || base_type.is_file() != env_type.is_file()
        || base_type.is_symlink() != env_type.is_symlink())
}

fn metadata_changed(base: &fs::Metadata, env: &fs::Metadata) -> bool {
    base.mode() != env.mode() || base.uid() != env.uid() || base.gid() != env.gid()
}

fn symlink_metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
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

fn is_runtime_only_path(path: &Path) -> bool {
    matches!(
        path.components()
            .next()
            .and_then(|component| component.as_os_str().to_str()),
        Some("proc" | "sys" | "dev" | "run" | "tmp")
    )
}

#[cfg(test)]
mod tests {
    use super::{is_runtime_only_path, ExportType, Exporter};
    use crate::model::{machine_name, Env, EnvState, Limits};
    use chrono::Utc;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    #[test]
    fn export_types_have_stable_artifact_names() {
        assert_eq!(
            ExportType::WorkspacePatch.artifact_name(),
            "workspace-patch.patch"
        );
        assert_eq!(
            ExportType::RootfsChangedPaths.artifact_name(),
            "rootfs-changed-paths.txt"
        );
        assert_eq!(ExportType::DpkgDelta.artifact_name(), "dpkg-delta.txt");
    }

    #[tokio::test]
    async fn workspace_patch_returns_git_diff() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("rootfs/workspace");
        fs::create_dir_all(&workspace).unwrap();
        run_git(&workspace, &["init", "--quiet"]);
        run_git(
            &workspace,
            &["config", "user.email", "test@example.invalid"],
        );
        run_git(&workspace, &["config", "user.name", "Test User"]);
        fs::write(workspace.join("README.md"), "old\n").unwrap();
        run_git(&workspace, &["add", "README.md"]);
        run_git(&workspace, &["commit", "--quiet", "-m", "initial"]);
        fs::write(workspace.join("README.md"), "new\n").unwrap();

        let patch = Exporter::default()
            .workspace_patch(&test_env(dir.path().join("rootfs")))
            .await
            .unwrap();

        assert!(patch.contains("-old"));
        assert!(patch.contains("+new"));
    }

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
        fs::create_dir_all(env.join("root/new-dir")).unwrap();
        fs::write(base.join("root/old.txt"), "old").unwrap();
        fs::write(base.join("root/same.txt"), "same").unwrap();
        fs::write(base.join("root/delete.txt"), "delete").unwrap();
        fs::write(env.join("root/old.txt"), "new").unwrap();
        fs::write(env.join("root/same.txt"), "same").unwrap();
        fs::write(env.join("root/add.txt"), "add").unwrap();
        symlink("/old-target", base.join("root/link")).unwrap();
        symlink("/new-target", env.join("root/link")).unwrap();
        symlink("/added-target", env.join("root/added-link")).unwrap();

        let changed = Exporter::changed_paths_by_walk(&base, &env).unwrap();
        assert!(changed.contains("/root/add.txt"));
        assert!(changed.contains("/root/old.txt"));
        assert!(changed.contains("/root/new-dir"));
        assert!(changed.contains("/root/link"));
        assert!(changed.contains("/root/added-link"));
        assert!(changed.contains("deleted /root/delete.txt"));
        assert!(!changed.contains("/root/same.txt"));
    }

    #[test]
    fn changed_paths_reports_metadata_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env = dir.path().join("env");
        fs::create_dir_all(base.join("root/mode-dir")).unwrap();
        fs::create_dir_all(env.join("root/mode-dir")).unwrap();
        fs::write(base.join("root/mode-file"), "same").unwrap();
        fs::write(env.join("root/mode-file"), "same").unwrap();

        fs::set_permissions(
            env.join("root/mode-file"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::set_permissions(env.join("root/mode-dir"), fs::Permissions::from_mode(0o700)).unwrap();

        let changed = Exporter::changed_paths_by_walk(&base, &env).unwrap();

        assert!(changed.contains("/root/mode-file"));
        assert!(changed.contains("/root/mode-dir"));
    }

    #[test]
    fn changed_paths_ignores_runtime_only_trees() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let env = dir.path().join("env");
        fs::create_dir_all(base.join("run")).unwrap();
        fs::create_dir_all(env.join("run/systemd")).unwrap();
        fs::create_dir_all(env.join("proc/1")).unwrap();
        fs::create_dir_all(env.join("tmp")).unwrap();
        fs::write(env.join("run/systemd/machine-id"), "runtime").unwrap();
        fs::write(env.join("tmp/scratch"), "runtime").unwrap();

        let changed = Exporter::changed_paths_by_walk(&base, &env).unwrap();

        assert!(changed.is_empty(), "{changed}");
        assert!(is_runtime_only_path(std::path::Path::new("run/systemd")));
        assert!(!is_runtime_only_path(std::path::Path::new(
            "root/marker.txt"
        )));
    }

    fn run_git(workdir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(workdir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn test_env(rootfs_path: std::path::PathBuf) -> Env {
        Env {
            id: "codex-1".to_string(),
            base_id: "base-001".to_string(),
            rootfs_path,
            machine_name: machine_name("codex-1"),
            state: EnvState::Created,
            profile: "privileged-dev".to_string(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            network_policy: Default::default(),
            limits: Limits::default(),
            sessions: Vec::new(),
        }
    }
}
