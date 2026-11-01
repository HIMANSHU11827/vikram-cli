//! Installing a skill from a directory or a git repository.
//!
//! Installation is a three-step move that can fail at any point without damaging `root`:
//!
//! 1. everything is assembled in `<state_dir>/staging/...`,
//! 2. the assembled skill is validated exactly like a discovered one,
//! 3. it is renamed into place; an existing copy is moved to the trash first, not overwritten.
//!
//! Git is the only thing here that touches the network, and only when `allow_network` is set.
//! The default is `false`: `install` never reaches out on its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Result, SkillError};
use crate::frontmatter::{self, Frontmatter, Yaml};
use crate::history::{History, Manifest, hash_dir};
use crate::manager::{Manager, SkillSource, read_version};
use crate::paths;
use crate::skill::SkillId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallSource {
    Directory(PathBuf),
    Git {
        url: String,
        rev: Option<String>,
        subdir: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub name: SkillId,
    pub path: PathBuf,
    pub source: String,
    pub rev: Option<String>,
    pub content_hash: String,
    pub replaced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installer {
    pub manager: Manager,
    pub allow_network: bool,
}

impl Installer {
    /// Network access is off until the caller turns it on, which is what explicit user consent
    /// looks like in code.
    pub fn new(manager: Manager) -> Installer {
        Installer {
            manager,
            allow_network: false,
        }
    }

    pub fn install(&self, source: &InstallSource) -> Result<InstallOutcome> {
        match source {
            InstallSource::Directory(dir) => {
                let fallback = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.install_tree(dir, fallback, dir.display().to_string(), None)
            }
            InstallSource::Git { url, rev, subdir } => {
                self.install_git(url, rev.as_deref(), subdir.as_deref())
            }
        }
    }

    fn install_git(
        &self,
        url: &str,
        rev: Option<&str>,
        subdir: Option<&str>,
    ) -> Result<InstallOutcome> {
        if !self.allow_network {
            return Err(SkillError::Denied {
                reason: format!(
                    "network access is disabled; refusing to clone `{url}` without explicit consent"
                ),
            });
        }
        let url = url.trim();
        if url.is_empty() {
            return Err(SkillError::Install("empty git url".to_string()));
        }
        let fallback = match subdir {
            Some(rel) => rel
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(rel)
                .to_string(),
            None => repo_name(url),
        };

        let staging = self.staging_dir("git")?;
        let checkout = staging.join("checkout");
        let mut command = Command::new("git");
        command.arg("clone").arg("--depth").arg("1").arg("--quiet");
        if let Some(rev) = rev {
            command.arg("--branch").arg(rev);
        }
        command.arg("--").arg(url).arg(&checkout);
        // Never let git block the caller waiting for credentials on a terminal it does not have.
        command.env("GIT_TERMINAL_PROMPT", "0");

        let output = command.output().map_err(|err| SkillError::Io {
            path: PathBuf::from("git"),
            reason: err.to_string(),
        })?;
        if !output.status.success() {
            paths::remove_path(&staging);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("git clone failed");
            return Err(SkillError::Install(format!("git clone failed: {message}")));
        }
        let resolved_rev = git_rev(&checkout);

        let skill_root = match subdir {
            Some(rel) => match paths::resolve_within(&checkout, rel) {
                Ok(root) => root,
                Err(err) => {
                    paths::remove_path(&staging);
                    return Err(err);
                }
            },
            None => checkout.clone(),
        };

        let outcome = self.install_tree(&skill_root, fallback, url.to_string(), resolved_rev);
        paths::remove_path(&staging);
        outcome
    }

    /// Copies `dir` into staging, normalises the frontmatter name, validates, and moves it into
    /// the root. `fallback` names the skill when the frontmatter has no `name`.
    fn install_tree(
        &self,
        dir: &Path,
        fallback: String,
        source: String,
        rev: Option<String>,
    ) -> Result<InstallOutcome> {
        if !dir.is_dir() {
            return Err(SkillError::Io {
                path: dir.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        let skill_file = SkillSource::locate(dir)?;
        let text = fs::read_to_string(&skill_file.file)
            .map_err(|err| SkillError::io(&skill_file.file, &err))?;
        let fm = Frontmatter::parse_at(&text, &skill_file.file)?;

        let declared = fm
            .get("name")
            .and_then(Yaml::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let id = SkillId::new(declared.as_deref().unwrap_or(fallback.as_str()))?;
        let target = paths::resolve_within(&self.manager.root, id.as_str())?;
        let document = frontmatter::render_document(
            &fm.with("name", Yaml::Str(id.as_str().to_string())),
            &fm.body,
        );
        self.manager
            .check_document(&document, &skill_file.file, id.as_str())?;

        let replaced = target.exists();
        let staging = self.staging_dir("install")?;
        let build = (|| -> Result<()> {
            let assembled = staging.join("skill");
            paths::copy_dir(dir, &assembled, &paths::default_excludes())?;
            paths::atomic_write(
                &assembled.join(skill_file.rel.as_str()),
                document.as_bytes(),
            )?;
            self.place(&assembled, &target)
        })();
        if let Err(err) = build {
            paths::remove_path(&staging);
            return Err(err);
        }
        paths::remove_path(&staging);

        self.record(id, target, source, rev, replaced)
    }

    /// Renames `assembled` into `target`. An existing skill is moved to the trash first so a
    /// failed rename can put it back and a successful one still leaves the old copy recoverable.
    fn place(&self, assembled: &Path, target: &Path) -> Result<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| SkillError::io(parent, &err))?;
        }
        if !target.exists() {
            return match fs::rename(assembled, target) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let collision = target.exists();
                    if collision {
                        let name = target
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        Err(SkillError::AlreadyExists {
                            name,
                            path: target.to_path_buf(),
                        })
                    } else {
                        Err(SkillError::io(target, &err))
                    }
                }
            };
        }

        let trash = self.manager.state_dir().join("trash");
        fs::create_dir_all(&trash).map_err(|err| SkillError::io(&trash, &err))?;
        let backup = paths::unique_path(&trash, "replaced");
        paths::move_dir(target, &backup)?;
        match fs::rename(assembled, target) {
            Ok(()) => Ok(()),
            Err(err) => {
                // Put the previous skill back: the failed install must be a no-op.
                let restored = paths::move_dir(&backup, target);
                if restored.is_err() {
                    return Err(SkillError::Io {
                        path: target.to_path_buf(),
                        reason: format!(
                            "install failed ({err}) and the previous copy could not be restored from {}",
                            backup.display()
                        ),
                    });
                }
                Err(SkillError::io(target, &err))
            }
        }
    }

    fn record(
        &self,
        id: SkillId,
        target: PathBuf,
        source: String,
        rev: Option<String>,
        replaced: bool,
    ) -> Result<InstallOutcome> {
        let content_hash = hash_dir(&target)?;
        let installed_ms = paths::now_ms();
        let version = read_version(&target);
        let history = History::load_in(&self.manager.state_dir(), &id)?;
        let files = history.snapshot(&target, installed_ms)?;
        history.record(Manifest {
            name: id.clone(),
            version,
            content_hash: content_hash.clone(),
            source: Some(source.clone()),
            rev: rev.clone(),
            installed_ms,
            files,
        })?;
        Ok(InstallOutcome {
            name: id,
            path: target,
            source,
            rev,
            content_hash,
            replaced,
        })
    }

    fn staging_dir(&self, prefix: &str) -> Result<PathBuf> {
        let root = self.manager.state_dir().join("staging");
        fs::create_dir_all(&root).map_err(|err| SkillError::io(&root, &err))?;
        let staging = paths::unique_path(&root, prefix);
        fs::create_dir_all(&staging).map_err(|err| SkillError::io(&staging, &err))?;
        Ok(staging)
    }
}

/// The last path or `host:owner/name` segment of a URL, without a `.git` suffix.
fn repo_name(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

fn git_rev(checkout: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if rev.is_empty() { None } else { Some(rev) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_installer() -> (tempfile::TempDir, Installer) {
        let tmp = tempfile::tempdir().expect("temp");
        let manager = Manager::at(tmp.path().join("skills"));
        (tmp, Installer::new(manager))
    }

    fn source_skill(root: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("references")).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .expect("write");
        fs::write(dir.join("references/api.md"), "docs").expect("write");
        dir
    }

    #[test]
    fn network_is_refused_by_default() {
        let (_tmp, installer) = temp_installer();
        assert!(!installer.allow_network);
    }

    #[test]
    fn a_git_install_is_denied_when_network_is_disabled() {
        let (_tmp, installer) = temp_installer();
        let source = InstallSource::Git {
            url: "https://example.test/skills.git".to_string(),
            rev: None,
            subdir: None,
        };
        let err = installer.install(&source).expect_err("network off");
        match err {
            SkillError::Denied { reason } => {
                assert!(reason.contains("network access is disabled"), "{reason}");
                assert!(reason.contains("example.test"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!installer.manager.root.exists());
        assert!(!installer.manager.state_dir().exists());
    }

    #[test]
    fn a_directory_install_copies_the_tree_and_reports_its_hash() {
        let (tmp, installer) = temp_installer();
        let src = source_skill(tmp.path(), "alpha", "Does alpha", "# Alpha\n");
        let outcome = installer
            .install(&InstallSource::Directory(src.clone()))
            .expect("install");

        assert_eq!(outcome.name.as_str(), "alpha");
        assert_eq!(outcome.path, installer.manager.root.join("alpha"));
        assert!(!outcome.replaced);
        assert!(outcome.path.join("SKILL.md").is_file());
        assert!(outcome.path.join("references/api.md").is_file());
        assert_eq!(outcome.source, src.display().to_string());
        assert!(outcome.rev.is_none());
        assert_eq!(outcome.content_hash, hash_dir(&outcome.path).expect("hash"));
        assert_eq!(outcome.content_hash.len(), 16);
    }

    #[test]
    fn the_declared_name_wins_over_the_folder_name() {
        let (tmp, installer) = temp_installer();
        let dir = tmp.path().join("some-checkout");
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: real-name\ndescription: desc\n---\nbody",
        )
        .expect("write");
        let outcome = installer
            .install(&InstallSource::Directory(dir))
            .expect("install");
        assert_eq!(outcome.name.as_str(), "real-name");
        assert!(installer.manager.root.join("real-name/SKILL.md").is_file());
        assert!(!installer.manager.root.join("some-checkout").exists());
    }

    #[test]
    fn installing_without_a_skill_file_is_refused() {
        let (tmp, installer) = temp_installer();
        let dir = tmp.path().join("empty");
        fs::create_dir_all(&dir).expect("dirs");
        let err = installer
            .install(&InstallSource::Directory(dir))
            .expect_err("no skill file");
        assert!(matches!(err, SkillError::NotFound { .. }));
        assert!(!installer.manager.root.exists());
    }

    #[test]
    fn installing_a_broken_skill_is_refused_before_it_reaches_the_root() {
        let (tmp, installer) = temp_installer();
        let dir = tmp.path().join("broken");
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: broken\n---\nno description",
        )
        .expect("write");
        let err = installer
            .install(&InstallSource::Directory(dir))
            .expect_err("missing description");
        assert!(matches!(
            err,
            SkillError::MissingField {
                field: "description",
                ..
            }
        ));
        assert!(!installer.manager.root.join("broken").exists());
    }

    #[test]
    fn installing_an_invalid_declared_name_is_refused() {
        let (tmp, installer) = temp_installer();
        let dir = tmp.path().join("whatever");
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: Bad Name\ndescription: desc\n---\nbody",
        )
        .expect("write");
        let err = installer
            .install(&InstallSource::Directory(dir))
            .expect_err("invalid name");
        assert!(matches!(err, SkillError::InvalidName { .. }));
    }

    #[test]
    fn installation_records_a_manifest_and_a_snapshot() {
        let (tmp, installer) = temp_installer();
        let src = source_skill(tmp.path(), "alpha", "Does alpha", "body");
        let outcome = installer
            .install(&InstallSource::Directory(src))
            .expect("install");

        let history =
            History::load_in(&installer.manager.state_dir(), &outcome.name).expect("history");
        let latest = history.latest().expect("manifest");
        assert_eq!(latest.content_hash, outcome.content_hash);
        assert_eq!(latest.name, outcome.name);
        assert_eq!(latest.files.len(), 2);
        assert_eq!(latest.files[0].0, "SKILL.md");
        assert!(latest.installed_ms > 0);
        assert!(latest.source.is_some());
    }

    #[test]
    fn reinstalling_replaces_the_copy_and_keeps_the_old_one_in_the_trash() {
        let (tmp, installer) = temp_installer();
        let src = source_skill(tmp.path(), "alpha", "Does alpha", "v1");
        installer
            .install(&InstallSource::Directory(src.clone()))
            .expect("first install");

        fs::write(
            src.join("SKILL.md"),
            "---\nname: alpha\ndescription: Does alpha\n---\nv2",
        )
        .expect("write");
        let outcome = installer
            .install(&InstallSource::Directory(src))
            .expect("reinstall");
        assert!(outcome.replaced);

        let text = fs::read_to_string(outcome.path.join("SKILL.md")).expect("read");
        assert!(text.ends_with("v2"), "{text}");

        let trash = installer.manager.state_dir().join("trash");
        let left: Vec<PathBuf> = fs::read_dir(&trash)
            .expect("trash")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(left.len(), 1);
        let old = fs::read_to_string(left[0].join("SKILL.md")).expect("old read");
        assert!(old.ends_with("v1"), "{old}");
    }

    #[test]
    fn two_installs_of_the_same_source_hash_identically() {
        let (tmp, installer) = temp_installer();
        let src = source_skill(tmp.path(), "alpha", "Does alpha", "body");
        let first = installer
            .install(&InstallSource::Directory(src.clone()))
            .expect("first");
        let second = installer
            .install(&InstallSource::Directory(src))
            .expect("second");
        assert_eq!(first.content_hash, second.content_hash);
        assert!(second.replaced);
    }

    #[test]
    fn installing_creates_a_missing_root() {
        let (tmp, _) = temp_installer();
        let nested = tmp.path().join("deep/nested/skills");
        let installer = Installer::new(Manager::at(nested.clone()));
        let src = source_skill(tmp.path(), "alpha", "desc", "body");
        let outcome = installer
            .install(&InstallSource::Directory(src))
            .expect("install");
        assert!(nested.is_dir());
        assert_eq!(outcome.path, nested.join("alpha"));
    }

    #[test]
    fn staging_is_cleaned_up_after_success_and_failure() {
        let (tmp, installer) = temp_installer();
        let good = source_skill(tmp.path(), "alpha", "desc", "body");
        installer
            .install(&InstallSource::Directory(good))
            .expect("install");

        let broken = tmp.path().join("broken");
        fs::create_dir_all(broken.join("references")).expect("dirs");
        fs::write(broken.join("SKILL.md"), "no frontmatter at all").expect("write");
        let err = installer
            .install(&InstallSource::Directory(broken))
            .expect_err("no description");
        assert!(matches!(err, SkillError::MissingField { .. }));

        let staging = installer.manager.state_dir().join("staging");
        let leftovers: Vec<PathBuf> = match fs::read_dir(&staging) {
            Ok(reader) => reader.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
            Err(_) => Vec::new(),
        };
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn repo_names_come_from_the_url() {
        for (url, expected) in [
            ("https://github.com/owner/skills.git", "skills"),
            ("https://github.com/owner/skills/", "skills"),
            ("git@github.com:owner/thing.git", "thing"),
            ("/srv/repos/local", "local"),
        ] {
            assert_eq!(repo_name(url), expected, "{url}");
        }
    }
}
