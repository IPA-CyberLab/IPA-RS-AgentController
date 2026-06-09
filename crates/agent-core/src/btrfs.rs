use crate::command::CommandRunner;
use anyhow::{anyhow, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Btrfs {
    runner: CommandRunner,
}

impl Default for Btrfs {
    fn default() -> Self {
        Self {
            runner: CommandRunner,
        }
    }
}

impl Btrfs {
    pub async fn is_filesystem(&self, path: &Path) -> Result<bool> {
        let output = self
            .runner
            .run(
                "findmnt",
                [
                    "-n",
                    "-o",
                    "FSTYPE",
                    "--target",
                    &path.display().to_string(),
                ],
            )
            .await?;
        Ok(output.status == 0 && output.stdout.trim() == "btrfs")
    }

    pub async fn ensure_filesystem(&self, path: &Path) -> Result<()> {
        if !self.is_filesystem(path).await? {
            return Err(anyhow!("{} is not on a Btrfs filesystem", path.display()));
        }
        Ok(())
    }

    pub async fn is_subvolume(&self, path: &Path) -> Result<bool> {
        let output = self
            .runner
            .run("btrfs", ["subvolume", "show", &path.display().to_string()])
            .await?;
        Ok(output.status == 0)
    }

    pub async fn ensure_subvolume(&self, path: &Path) -> Result<()> {
        if !self.is_subvolume(path).await? {
            return Err(anyhow!("{} is not a Btrfs subvolume", path.display()));
        }
        Ok(())
    }

    pub async fn enable_quota(&self, path: &Path) -> Result<()> {
        let output = self
            .runner
            .run("btrfs", ["quota", "enable", &path.display().to_string()])
            .await?;
        if output.status == 0 || quota_enable_reports_already_enabled(&output.stderr) {
            return Ok(());
        }
        Err(anyhow!("failed to enable Btrfs quota: {}", output.stderr))
    }

    pub async fn snapshot_readonly(&self, from: &Path, to: &Path) -> Result<()> {
        self.runner
            .run_checked(
                "btrfs",
                [
                    "subvolume",
                    "snapshot",
                    "-r",
                    &from.display().to_string(),
                    &to.display().to_string(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_readonly(&self, path: &Path, readonly: bool) -> Result<()> {
        self.runner
            .run_checked(
                "btrfs",
                [
                    "property",
                    "set",
                    "-ts",
                    &path.display().to_string(),
                    "ro",
                    if readonly { "true" } else { "false" },
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn snapshot_writable(&self, from: &Path, to: &Path) -> Result<()> {
        self.runner
            .run_checked(
                "btrfs",
                [
                    "subvolume",
                    "snapshot",
                    &from.display().to_string(),
                    &to.display().to_string(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_limit(&self, size: &str, path: &Path) -> Result<()> {
        if is_unlimited(size) {
            return Ok(());
        }
        self.runner
            .run_checked(
                "btrfs",
                ["qgroup", "limit", size, &path.display().to_string()],
            )
            .await?;
        Ok(())
    }

    pub async fn delete_subvolume(&self, path: &Path) -> Result<()> {
        let output = self
            .runner
            .run(
                "btrfs",
                ["subvolume", "delete", &path.display().to_string()],
            )
            .await?;
        if output.status == 0 || subvolume_delete_reports_missing(&output.stderr) {
            return Ok(());
        }
        Err(anyhow!(
            "failed to delete Btrfs subvolume {}: {}{}",
            path.display(),
            output.stdout,
            output.stderr
        ))
    }

    pub async fn qgroup_id(&self, path: &Path) -> Result<Option<String>> {
        let output = self
            .runner
            .run("btrfs", ["subvolume", "show", &path.display().to_string()])
            .await?;
        if output.status != 0 {
            if subvolume_show_reports_missing(&output.stderr) {
                return Ok(None);
            }
            return Err(anyhow!(
                "failed to inspect Btrfs subvolume {} for qgroup cleanup: {}{}",
                path.display(),
                output.stdout,
                output.stderr
            ));
        }
        for line in output.stdout.lines() {
            let trimmed = line.trim();
            if let Some(id) = trimmed.strip_prefix("Subvolume ID:") {
                return Ok(Some(format!("0/{}", id.trim())));
            }
        }
        Ok(None)
    }

    pub async fn destroy_qgroup(&self, qgroup_id: &str, filesystem: &Path) -> Result<()> {
        for attempt in 0..50 {
            let output = self
                .runner
                .run(
                    "btrfs",
                    [
                        "qgroup",
                        "destroy",
                        qgroup_id,
                        &filesystem.display().to_string(),
                    ],
                )
                .await?;
            if output.status == 0 || qgroup_destroy_reports_missing(&output.stderr) {
                return Ok(());
            }
            if qgroup_destroy_reports_busy(&output.stderr) && attempt < 49 {
                let _ = self
                    .runner
                    .run(
                        "btrfs",
                        ["filesystem", "sync", &filesystem.display().to_string()],
                    )
                    .await?;
                let _ = self
                    .runner
                    .run(
                        "btrfs",
                        ["subvolume", "sync", &filesystem.display().to_string()],
                    )
                    .await?;
                let _ = self
                    .runner
                    .run(
                        "btrfs",
                        ["quota", "rescan", "-w", &filesystem.display().to_string()],
                    )
                    .await?;
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            return Err(anyhow!(
                "failed to destroy Btrfs qgroup {qgroup_id}: {}",
                output.stderr
            ));
        }
        Ok(())
    }

    pub async fn changed_paths(&self, base: &Path, env: &Path) -> Result<String> {
        let output = self
            .runner
            .run_checked(
                "btrfs",
                [
                    "send",
                    "--no-data",
                    "-p",
                    &base.display().to_string(),
                    &env.display().to_string(),
                ],
            )
            .await?;
        Ok(output.stdout)
    }

    pub async fn quota_exceeded(&self, path: &Path) -> Result<bool> {
        let output = self
            .runner
            .run(
                "btrfs",
                [
                    "qgroup",
                    "show",
                    "--raw",
                    "-r",
                    "-e",
                    "-F",
                    &path.display().to_string(),
                ],
            )
            .await?;
        qgroup_show_result(output.status, &output.stdout, &output.stderr, path)
    }
}

fn qgroup_show_result(status: i32, stdout: &str, stderr: &str, path: &Path) -> Result<bool> {
    if status != 0 {
        return Err(anyhow!(
            "failed to inspect Btrfs qgroup quota for {}: {}{}",
            path.display(),
            stdout,
            stderr
        ));
    }
    Ok(qgroup_show_reports_exceeded(stdout))
}

fn qgroup_show_reports_exceeded(output: &str) -> bool {
    let mut lines = output.lines();
    let Some(header) = lines.next() else {
        return false;
    };
    let headers = header.split_whitespace().collect::<Vec<_>>();
    let Some(rfer_index) = headers.iter().position(|header| *header == "rfer") else {
        return false;
    };
    let Some(max_rfer_index) = headers.iter().position(|header| *header == "max_rfer") else {
        return false;
    };
    for line in lines.skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() <= rfer_index || fields.len() <= max_rfer_index {
            continue;
        }
        let referenced = fields
            .get(rfer_index)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        let max_referenced = fields
            .get(max_rfer_index)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        if max_referenced > 0 && referenced >= max_referenced {
            return true;
        }
    }
    false
}

fn is_unlimited(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "unlimited" | "infinity" | "none"
    )
}

fn quota_enable_reports_already_enabled(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("file exists")
        || stderr.contains("quota support already enabled")
        || stderr.contains("quota already enabled")
        || stderr.contains("qgroups already enabled")
}

fn qgroup_destroy_reports_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such file or directory")
        || stderr.contains("no such process")
        || stderr.contains("not exist")
        || stderr.contains("not found")
}

fn qgroup_destroy_reports_busy(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("device or resource busy")
}

fn subvolume_delete_reports_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such file or directory")
        || stderr.contains("not exist")
        || stderr.contains("not found")
}

fn subvolume_show_reports_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such file or directory")
        || stderr.contains("not a btrfs subvolume")
        || stderr.contains("not exist")
        || stderr.contains("not found")
}

#[cfg(test)]
mod tests {
    use super::{
        is_unlimited, qgroup_destroy_reports_busy, qgroup_destroy_reports_missing,
        qgroup_show_reports_exceeded, qgroup_show_result, quota_enable_reports_already_enabled,
        subvolume_delete_reports_missing, subvolume_show_reports_missing,
    };
    use std::path::Path;

    #[test]
    fn qgroup_unlimited_values_are_recognized() {
        assert!(is_unlimited("0"));
        assert!(is_unlimited(" unlimited "));
        assert!(is_unlimited("infinity"));
        assert!(is_unlimited("none"));
        assert!(!is_unlimited("100G"));
    }

    #[test]
    fn qgroup_show_detects_referenced_limit_boundary() {
        let output = "\
qgroupid         rfer         excl     max_rfer
--------         ----         ----     --------
0/257            1024         1024     1024
";
        assert!(qgroup_show_reports_exceeded(output));
    }

    #[test]
    fn qgroup_show_ignores_unlimited_or_below_limit() {
        let below_limit = "\
qgroupid         rfer         excl     max_rfer
--------         ----         ----     --------
0/257            1023         1023     1024
";
        let unlimited = "\
qgroupid         rfer         excl     max_rfer
--------         ----         ----     --------
0/257            1024         1024     0
";
        assert!(!qgroup_show_reports_exceeded(below_limit));
        assert!(!qgroup_show_reports_exceeded(unlimited));
    }

    #[test]
    fn qgroup_show_uses_max_rfer_not_max_excl() {
        let output = "\
qgroupid         rfer         excl     max_rfer    max_excl
--------         ----         ----     --------    --------
0/257            1024         1024     2048        1024
";
        assert!(!qgroup_show_reports_exceeded(output));
    }

    #[test]
    fn qgroup_show_uses_header_positions() {
        let output = "\
qgroupid         excl         max_excl    rfer     max_rfer
--------         ----         --------    ----     --------
0/257            1            0           1024     1024
";
        assert!(qgroup_show_reports_exceeded(output));
    }

    #[test]
    fn qgroup_show_failure_is_not_treated_as_below_quota() {
        let error = qgroup_show_result(
            1,
            "",
            "ERROR: quota support disabled\n",
            Path::new("/agentfs/envs/codex-1/rootfs"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("failed to inspect Btrfs qgroup"));
    }

    #[test]
    fn qgroup_destroy_missing_errors_are_idempotent() {
        assert!(qgroup_destroy_reports_missing(
            "ERROR: unable to destroy quota group: No such file or directory"
        ));
        assert!(qgroup_destroy_reports_missing(
            "ERROR: unable to destroy quota group: No such process"
        ));
        assert!(qgroup_destroy_reports_missing(
            "ERROR: qgroup 0/257 does not exist"
        ));
        assert!(!qgroup_destroy_reports_missing(
            "ERROR: unable to destroy quota group: Permission denied"
        ));
    }

    #[test]
    fn qgroup_destroy_busy_errors_are_retriable() {
        assert!(qgroup_destroy_reports_busy(
            "ERROR: unable to destroy quota group: Device or resource busy"
        ));
        assert!(!qgroup_destroy_reports_busy(
            "ERROR: unable to destroy quota group: Permission denied"
        ));
    }

    #[test]
    fn subvolume_delete_missing_errors_are_idempotent() {
        assert!(subvolume_delete_reports_missing(
            "ERROR: Could not statfs: No such file or directory"
        ));
        assert!(subvolume_delete_reports_missing(
            "ERROR: subvolume /agentfs/envs/codex-1/rootfs does not exist"
        ));
        assert!(!subvolume_delete_reports_missing(
            "ERROR: failed to delete subvolume: Directory not empty"
        ));
    }

    #[test]
    fn subvolume_show_missing_errors_are_idempotent() {
        assert!(subvolume_show_reports_missing(
            "ERROR: Could not statfs: No such file or directory"
        ));
        assert!(subvolume_show_reports_missing(
            "ERROR: not a btrfs subvolume: /agentfs/envs/codex-1/rootfs"
        ));
        assert!(!subvolume_show_reports_missing(
            "ERROR: failed to inspect subvolume: Permission denied"
        ));
    }

    #[test]
    fn quota_enable_already_enabled_errors_are_idempotent() {
        assert!(quota_enable_reports_already_enabled(
            "ERROR: quota support already enabled"
        ));
        assert!(quota_enable_reports_already_enabled(
            "ERROR: cannot enable quota: File exists"
        ));
        assert!(quota_enable_reports_already_enabled(
            "ERROR: qgroups already enabled"
        ));
        assert!(!quota_enable_reports_already_enabled(
            "ERROR: quota support disabled"
        ));
    }
}
