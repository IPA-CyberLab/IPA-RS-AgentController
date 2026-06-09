use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Default)]
pub struct CommandRunner;

impl CommandRunner {
    pub async fn run<I, S>(&self, program: &str, args: I) -> Result<CmdOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new(program)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to execute {program}"))?;
        Ok(CmdOutput {
            status: output.status.code().unwrap_or(128),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    pub async fn run_checked<I, S>(&self, program: &str, args: I) -> Result<CmdOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(program, args).await?;
        if output.status != 0 {
            return Err(anyhow!(
                "{program} exited with {}: {}{}",
                output.status,
                output.stdout,
                output.stderr
            ));
        }
        Ok(output)
    }

    pub async fn append_to_file(path: &Path, content: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        Ok(())
    }
}

pub(crate) fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./:=+".contains(&b))
            {
                arg.clone()
            } else {
                shell_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{shell_join, shell_quote, CommandRunner};

    #[test]
    fn shell_join_quotes_spaces_and_quotes() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "echo 'hello world'".into()]),
            "bash -lc 'echo '\\''hello world'\\'''"
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("shell's dev"), "'shell'\\''s dev'");
    }

    #[test]
    fn shell_join_preserves_empty_arguments() {
        assert_eq!(
            shell_join(&["bash".into(), "-lc".into(), "".into()]),
            "bash -lc ''"
        );
    }

    #[tokio::test]
    async fn append_to_file_creates_parent_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/session.log");

        CommandRunner::append_to_file(&path, "first\n")
            .await
            .unwrap();
        CommandRunner::append_to_file(&path, "second\n")
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "first\nsecond\n"
        );
    }
}
