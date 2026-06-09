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
                ["qgroup", "show", "-reF", &path.display().to_string()],
            )
            .await?;
        if output.status != 0 {
            return Ok(false);
        }
        for line in output.stdout.lines().skip(2) {
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
                return Ok(true);
            }
        }
        Ok(false)
    }
}
