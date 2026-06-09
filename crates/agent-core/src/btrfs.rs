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
    pub async fn ensure_filesystem(&self, path: &Path) -> Result<()> {
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
        if output.status != 0 || output.stdout.trim() != "btrfs" {
            return Err(anyhow!("{} is not on a Btrfs filesystem", path.display()));
        }
        Ok(())
    }

    pub async fn ensure_subvolume(&self, path: &Path) -> Result<()> {
        let output = self
            .runner
            .run("btrfs", ["subvolume", "show", &path.display().to_string()])
            .await?;
        if output.status != 0 {
            return Err(anyhow!("{} is not a Btrfs subvolume", path.display()));
        }
        Ok(())
    }

    pub async fn enable_quota(&self, path: &Path) -> Result<()> {
        let output = self
            .runner
            .run("btrfs", ["quota", "enable", &path.display().to_string()])
            .await?;
        if output.status == 0 || output.stderr.contains("File exists") {
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
        self.runner
            .run_checked(
                "btrfs",
                ["subvolume", "delete", &path.display().to_string()],
            )
            .await?;
        Ok(())
    }

    pub async fn qgroup_id(&self, path: &Path) -> Result<Option<String>> {
        let output = self
            .runner
            .run("btrfs", ["subvolume", "show", &path.display().to_string()])
            .await?;
        if output.status != 0 {
            return Ok(None);
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
        Err(anyhow!(
            "failed to destroy Btrfs qgroup {qgroup_id}: {}",
            output.stderr
        ))
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
                    "-b",
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
    for line in output.lines().skip(2) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 4 {
            continue;
        }
        let referenced = fields
            .get(1)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        let max_referenced = fields
            .last()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        if max_referenced > 0 && referenced >= max_referenced {
            return true;
        }
    }
    false
}

fn is_unlimited(value: &str) -> bool {
    let value = value.trim();
    value == "0" || value.eq_ignore_ascii_case("unlimited") || value.eq_ignore_ascii_case("none")
}

fn qgroup_destroy_reports_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such file or directory")
        || stderr.contains("no such process")
        || stderr.contains("not exist")
        || stderr.contains("not found")
}

#[cfg(test)]
mod tests {
    use super::{
        is_unlimited, qgroup_destroy_reports_missing, qgroup_show_reports_exceeded,
        qgroup_show_result,
    };
    use std::path::Path;

    #[test]
    fn qgroup_unlimited_values_are_recognized() {
        assert!(is_unlimited("0"));
        assert!(is_unlimited(" unlimited "));
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
}
