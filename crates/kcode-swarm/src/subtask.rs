use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SubtaskValidationError {
    #[error("missing or blank field: {0}")]
    BlankField(&'static str),
    #[error("path '{0}' contains forbidden component (.., root, .git, .kcode, target)")]
    ForbiddenPath(String),
    #[error("path '{0}' escapes workspace root {1}")]
    OutsideWorkspaceRoot(String, String),
    #[error("path '{0}' not allowed for task kind {1:?} (extension rule)")]
    DisallowedExtension(String, TaskKind),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// src/ implementation work (default).
    Implement,
    /// May write under tests/ — the ONLY kind allowed to touch tests.
    WriteTests,
    /// May update *.snap files after verifier approval.
    UpdateSnapshots,
    /// Explicit refactor: public interface changes authorized.
    Refactor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSubtaskPayload {
    pub target_file: String,
    pub task_description: String,
    pub focal_interface: String,
    #[serde(default)]
    pub task_kind: Option<TaskKind>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub allow_interface_changes: bool,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

fn default_max_turns() -> usize {
    6
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedSubtaskSpec {
    pub canonical_target_file: PathBuf,
    pub relative_target_file: PathBuf,
    pub task_description: String,
    pub focal_interface: String,
    pub task_kind: TaskKind,
    pub constraints: Vec<String>,
    pub allow_interface_changes: bool,
    pub max_turns: usize,
}

impl RawSubtaskPayload {
    pub fn validate(self, workspace_root: &Path) -> Result<ValidatedSubtaskSpec, SubtaskValidationError> {
        let task_description = non_empty(&self.task_description, "task_description")?;
        let focal_interface = non_empty(&self.focal_interface, "focal_interface")?;
        let target_str = non_empty(&self.target_file, "target_file")?;
        let task_kind = self.task_kind.unwrap_or(TaskKind::Implement);

        let raw_path = Path::new(target_str);
        for component in raw_path.components() {
            match component {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(SubtaskValidationError::ForbiddenPath(target_str.to_string()));
                }
                Component::Normal(part) => {
                    if matches!(part.to_str(), Some(".git" | ".kcode" | "target")) {
                        return Err(SubtaskValidationError::ForbiddenPath(target_str.to_string()));
                    }
                }
                Component::CurDir => {}
            }
        }

        // Path + extension rules per task kind: WriteTests is the ONLY kind
        // allowed under tests/; Implement/Refactor may never touch tests/ or
        // benches/ (kills the "delete the failing test" failure mode).
        let components: Vec<String> = raw_path
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => p.to_str().map(str::to_string),
                _ => None,
            })
            .collect();
        let under_tests = components.windows(1).any(|w| w[0] == "tests");
        let under_benches = components.windows(1).any(|w| w[0] == "benches");
        match task_kind {
            TaskKind::Implement | TaskKind::Refactor | TaskKind::UpdateSnapshots if under_tests || under_benches => {
                return Err(SubtaskValidationError::ForbiddenPath(target_str.to_string()));
            }
            TaskKind::WriteTests if !under_tests => {
                return Err(SubtaskValidationError::ForbiddenPath(target_str.to_string()));
            }
            _ => {}
        }

        // Extension rules per task kind. NOTE (v1.1 fix): `Path::extension()`
        // only returns the substring after the FINAL dot, so `foo.snap.new`
        // yields "new" — snapshot targets are therefore matched by full
        // suffix, not by `extension()`.
        let extension_ok = match task_kind {
            TaskKind::Implement | TaskKind::Refactor => {
                matches!(raw_path.extension().and_then(|e| e.to_str()), Some("rs" | "toml"))
            }
            TaskKind::WriteTests => {
                matches!(raw_path.extension().and_then(|e| e.to_str()), Some("rs"))
            }
            TaskKind::UpdateSnapshots => {
                target_str.ends_with(".snap") || target_str.ends_with(".snap.new")
            }
        };
        if !extension_ok {
            return Err(SubtaskValidationError::DisallowedExtension(
                target_str.to_string(),
                task_kind,
            ));
        }

        // Structural containment (works for not-yet-existing files), canonicalized
        // when possible for symlink defense.
        let canonical_root =
            workspace_root.canonicalize().unwrap_or_else(|_| workspace_root.to_path_buf());
        let resolved = canonical_root.join(raw_path);
        if let Ok(canon) = resolved.parent().unwrap_or(&canonical_root).canonicalize() {
            if !canon.starts_with(&canonical_root) {
                return Err(SubtaskValidationError::OutsideWorkspaceRoot(
                    target_str.to_string(),
                    canonical_root.display().to_string(),
                ));
            }
        }

        let mut constraints: Vec<String> =
            self.constraints.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        constraints.dedup();

        Ok(ValidatedSubtaskSpec {
            canonical_target_file: resolved,
            relative_target_file: raw_path.to_path_buf(),
            task_description: task_description.to_string(),
            focal_interface: focal_interface.to_string(),
            task_kind,
            constraints,
            allow_interface_changes: self.allow_interface_changes,
            max_turns: self.max_turns.clamp(1, 10),
        })
    }
}

fn non_empty<'a>(s: &'a str, field: &'static str) -> Result<&'a str, SubtaskValidationError> {
    let t = s.trim();
    if t.is_empty() {
        Err(SubtaskValidationError::BlankField(field))
    } else {
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_json(target: &str) -> String {
        format!(
            r#"{{
                "target_file": "{target}",
                "task_description": "Implement SessionTokenTracker",
                "focal_interface": "pub struct SessionTokenTracker;",
                "constraints": ["No unwrap"],
                "max_turns": 4
            }}"#
        )
    }

    #[test]
    fn valid_payload_passes() {
        let raw: RawSubtaskPayload = serde_json::from_str(&base_json("crates/blockcell-daemon/src/session.rs")).unwrap();
        let spec = raw.validate(Path::new("/tmp/fake-root")).unwrap();
        assert_eq!(spec.max_turns, 4);
        assert_eq!(spec.task_kind, TaskKind::Implement);
    }

    #[test]
    fn rejects_traversal() {
        let raw: RawSubtaskPayload = serde_json::from_str(&base_json("../../etc/passwd.rs")).unwrap();
        assert!(matches!(raw.validate(Path::new("/tmp/r")), Err(SubtaskValidationError::ForbiddenPath(_))));
    }

    #[test]
    fn rejects_git_internals_even_with_allowed_extension() {
        // .git/config has extension "config" (rejected by allowlist anyway),
        // but .git/anything must fail on the component rule, not the extension.
        let raw: RawSubtaskPayload = serde_json::from_str(&base_json(".git/hooks/post-commit.rs")).unwrap();
        assert!(matches!(raw.validate(Path::new("/tmp/r")), Err(SubtaskValidationError::ForbiddenPath(_))));
    }

    #[test]
    fn write_tests_kind_must_target_tests_dir() {
        let json = base_json("crates/core/src/lib.rs")
            .replace("focal_interface", "task_kind\": \"write_tests\",\n            \"focal_interface");
        let raw: RawSubtaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(raw.validate(Path::new("/tmp/r")), Err(SubtaskValidationError::ForbiddenPath(_))));

        let ok = base_json("crates/core/tests/session_test.rs")
            .replace("focal_interface", "task_kind\": \"write_tests\",\n            \"focal_interface");
        let raw: RawSubtaskPayload = serde_json::from_str(&ok).unwrap();
        assert!(raw.validate(Path::new("/tmp/r")).is_ok());
    }

    #[test]
    fn implement_kind_may_not_touch_tests() {
        let raw: RawSubtaskPayload = serde_json::from_str(&base_json("crates/core/tests/regression.rs")).unwrap();
        assert!(matches!(raw.validate(Path::new("/tmp/r")), Err(SubtaskValidationError::ForbiddenPath(_))));
    }

    #[test]
    fn update_snapshots_accepts_snap_new_suffix() {
        // v1.1 regression: extension() of `out.snap.new` is "new", which the old
        // allowlist ["snap", "snap.new"] could never match.
        let json = base_json("crates/core/src/snapshots/out.snap.new")
            .replace("focal_interface", "task_kind\": \"update_snapshots\",\n            \"focal_interface");
        let raw: RawSubtaskPayload = serde_json::from_str(&json).unwrap();
        assert!(raw.validate(Path::new("/tmp/r")).is_ok());
    }

    #[test]
    fn clamps_max_turns() {
        let json = base_json("src/lib.rs").replace("\"max_turns\": 4", "\"max_turns\": 100");
        let raw: RawSubtaskPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(raw.validate(Path::new("/tmp/r")).unwrap().max_turns, 10);
    }
}
