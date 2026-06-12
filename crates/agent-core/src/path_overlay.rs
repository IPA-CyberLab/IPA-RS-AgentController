use anyhow::{anyhow, Context, Result};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySource {
    Upper,
    Lower,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub visible_path: PathBuf,
    pub storage_path: PathBuf,
    pub source: OverlaySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayLookup {
    Found(ResolvedPath),
    Whiteout(PathBuf),
    Missing,
}

#[derive(Debug, Clone)]
pub struct PathOverlay {
    lower: PathBuf,
    upper: PathBuf,
    whiteouts: PathBuf,
}

impl PathOverlay {
    pub fn new(lower: PathBuf, upper: PathBuf, whiteouts: PathBuf) -> Self {
        Self {
            lower,
            upper,
            whiteouts,
        }
    }

    pub fn lower_path(&self, visible_path: &Path) -> Result<PathBuf> {
        self.storage_path(&self.lower, visible_path)
    }

    pub fn upper_path(&self, visible_path: &Path) -> Result<PathBuf> {
        self.storage_path(&self.upper, visible_path)
    }

    pub fn whiteout_path(&self, visible_path: &Path) -> Result<PathBuf> {
        self.storage_path(&self.whiteouts, visible_path)
    }

    pub fn resolve(&self, visible_path: &Path) -> Result<OverlayLookup> {
        let whiteout = self.whiteout_path(visible_path)?;
        if whiteout.exists() {
            return Ok(OverlayLookup::Whiteout(whiteout));
        }
        let upper = self.upper_path(visible_path)?;
        if upper.exists() {
            return Ok(OverlayLookup::Found(ResolvedPath {
                visible_path: visible_path.to_path_buf(),
                storage_path: upper,
                source: OverlaySource::Upper,
            }));
        }
        let lower = self.lower_path(visible_path)?;
        if lower.exists() {
            return Ok(OverlayLookup::Found(ResolvedPath {
                visible_path: visible_path.to_path_buf(),
                storage_path: lower,
                source: OverlaySource::Lower,
            }));
        }
        Ok(OverlayLookup::Missing)
    }

    pub fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from_upper = self.upper_path(from)?;
        let to_upper = self.upper_path(to)?;
        if let Some(parent) = to_upper.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if from_upper.exists() {
            std::fs::rename(&from_upper, &to_upper).with_context(|| {
                format!(
                    "failed to rename overlay upper {} to {}",
                    from_upper.display(),
                    to_upper.display()
                )
            })?;
            self.create_whiteout(from)?;
            return Ok(());
        }
        match self.resolve(from)? {
            OverlayLookup::Found(found) if found.source == OverlaySource::Lower => {
                copy_tree(&found.storage_path, &to_upper)?;
                self.create_whiteout(from)?;
                Ok(())
            }
            OverlayLookup::Found(found) => {
                std::fs::rename(&found.storage_path, &to_upper)?;
                self.create_whiteout(from)?;
                Ok(())
            }
            OverlayLookup::Whiteout(_) | OverlayLookup::Missing => Err(anyhow!(
                "cannot rename missing overlay path {}",
                from.display()
            )),
        }
    }

    pub fn create_whiteout(&self, visible_path: &Path) -> Result<()> {
        let whiteout = self.whiteout_path(visible_path)?;
        if let Some(parent) = whiteout.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&whiteout, b"")
            .with_context(|| format!("failed to create whiteout {}", whiteout.display()))
    }

    fn storage_path(&self, root: &Path, visible_path: &Path) -> Result<PathBuf> {
        let rel = absolute_path_as_overlay_relative(visible_path)?;
        Ok(root.join(rel))
    }
}

pub fn absolute_path_as_overlay_relative(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(anyhow!("path-preserving overlay path must be absolute"));
    }
    let mut rel = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {}
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(anyhow!(
                    "path-preserving overlay path must not contain parent components: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(rel)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)
        .with_context(|| format!("failed to stat overlay source {}", src.display()))?;
    if metadata.is_dir() {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create overlay dir {}", dst.display()))?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        std::fs::copy(src, dst).with_context(|| {
            format!(
                "failed to copy overlay lower {} to upper {}",
                src.display(),
                dst.display()
            )
        })?;
    } else if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dst)?;
        #[cfg(not(unix))]
        return Err(anyhow!(
            "symlink copy-up is unsupported on this platform for {}",
            src.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{OverlayLookup, OverlaySource, PathOverlay};
    use std::fs;
    use std::path::Path;

    #[test]
    fn resolver_prefers_whiteout_over_upper_and_lower() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = fixture_overlay(dir.path());
        let path = Path::new("/Users/mizuame/project/a.txt");
        write(&overlay.lower_path(path).unwrap(), "lower");
        write(&overlay.upper_path(path).unwrap(), "upper");
        overlay.create_whiteout(path).unwrap();

        assert!(matches!(
            overlay.resolve(path).unwrap(),
            OverlayLookup::Whiteout(_)
        ));
    }

    #[test]
    fn resolver_reads_upper_before_lower() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = fixture_overlay(dir.path());
        let path = Path::new("/Users/mizuame/project/a.txt");
        write(&overlay.lower_path(path).unwrap(), "lower");
        write(&overlay.upper_path(path).unwrap(), "upper");

        let OverlayLookup::Found(found) = overlay.resolve(path).unwrap() else {
            panic!("expected found path");
        };

        assert_eq!(found.source, OverlaySource::Upper);
        assert_eq!(fs::read_to_string(found.storage_path).unwrap(), "upper");
    }

    #[test]
    fn resolver_preserves_absolute_path_string_while_mapping_storage_path() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = fixture_overlay(dir.path());
        let visible = Path::new("/Users/mizuame/Desktop/script/example/file.rs");
        write(&overlay.lower_path(visible).unwrap(), "lower");

        let OverlayLookup::Found(found) = overlay.resolve(visible).unwrap() else {
            panic!("expected found path");
        };

        assert_eq!(found.visible_path, visible);
        assert_ne!(found.storage_path, visible);
        assert!(found
            .storage_path
            .ends_with("lower/Users/mizuame/Desktop/script/example/file.rs"));
    }

    #[test]
    fn rename_lower_file_creates_upper_copy_and_source_whiteout() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = fixture_overlay(dir.path());
        let old = Path::new("/Users/mizuame/project/old.txt");
        let new = Path::new("/Users/mizuame/project/new.txt");
        write(&overlay.lower_path(old).unwrap(), "lower");

        overlay.rename(old, new).unwrap();

        assert!(matches!(
            overlay.resolve(old).unwrap(),
            OverlayLookup::Whiteout(_)
        ));
        let OverlayLookup::Found(found) = overlay.resolve(new).unwrap() else {
            panic!("expected renamed path");
        };
        assert_eq!(found.source, OverlaySource::Upper);
        assert_eq!(fs::read_to_string(found.storage_path).unwrap(), "lower");
        assert_eq!(
            fs::read_to_string(overlay.lower_path(old).unwrap()).unwrap(),
            "lower"
        );
    }

    #[test]
    fn safe_save_rename_replaces_target_in_upper_without_touching_lower() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = fixture_overlay(dir.path());
        let target = Path::new("/Users/mizuame/project/file.rs");
        let tmp = Path::new("/Users/mizuame/project/.file.rs.tmp");
        write(&overlay.lower_path(target).unwrap(), "old");
        write(&overlay.upper_path(tmp).unwrap(), "new");

        overlay.rename(tmp, target).unwrap();

        let OverlayLookup::Found(found) = overlay.resolve(target).unwrap() else {
            panic!("expected target path");
        };
        assert_eq!(found.source, OverlaySource::Upper);
        assert_eq!(fs::read_to_string(found.storage_path).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(overlay.lower_path(target).unwrap()).unwrap(),
            "old"
        );
        assert!(matches!(
            overlay.resolve(tmp).unwrap(),
            OverlayLookup::Whiteout(_)
        ));
    }

    #[test]
    fn multiple_envs_share_preserved_path_but_isolate_upper_layers() {
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path().join("lower");
        let env_a = PathOverlay::new(
            lower.clone(),
            dir.path().join("env-a/upper"),
            dir.path().join("env-a/whiteouts"),
        );
        let env_b = PathOverlay::new(
            lower,
            dir.path().join("env-b/upper"),
            dir.path().join("env-b/whiteouts"),
        );
        let path = Path::new("/Users/mizuame/project/file.txt");
        write(&env_a.lower_path(path).unwrap(), "lower");
        write(&env_a.upper_path(path).unwrap(), "A");

        let OverlayLookup::Found(a) = env_a.resolve(path).unwrap() else {
            panic!("expected env A path");
        };
        let OverlayLookup::Found(b) = env_b.resolve(path).unwrap() else {
            panic!("expected env B path");
        };

        assert_eq!(fs::read_to_string(a.storage_path).unwrap(), "A");
        assert_eq!(fs::read_to_string(b.storage_path).unwrap(), "lower");
        assert_ne!(
            env_a.upper_path(path).unwrap(),
            env_b.upper_path(path).unwrap()
        );
    }

    fn fixture_overlay(root: &Path) -> PathOverlay {
        PathOverlay::new(
            root.join("lower"),
            root.join("upper"),
            root.join("whiteouts"),
        )
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}
