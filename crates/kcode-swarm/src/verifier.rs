use crate::subtask::{TaskKind, ValidatedSubtaskSpec};
use crate::worktree::EphemeralWorktree;
use anyhow::{Context, Result};
use command_group::AsyncCommandGroup;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command as AsyncCommand;

// ---------------------------------------------------------------------------
// Structured Verifier Output (fed back to Architect on rejection)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "UPPERCASE")]
pub enum VerifierOutcome {
    Passed,
    Rejected(VerifierRejection),
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifierRejection {
    pub gate: &'static str,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_tests: Vec<FailedTest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uncovered_lines: Vec<UncoveredLine>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub surviving_mutants: Vec<SurvivingMutant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
}

// ---------------------------------------------------------------------------
// Gate 1: Snapshot & Scope Guard
// ---------------------------------------------------------------------------

/// Scan `git status --porcelain` output: reject modified/added snapshot files
/// unless the subtask is TaskKind::UpdateSnapshots.
pub fn snapshot_violations(git_status_porcelain: &str, allow_updates: bool) -> Vec<String> {
    if allow_updates {
        return Vec::new();
    }
    git_status_porcelain
        .lines()
        .filter_map(|line| line.get(3..))
        .map(str::trim)
        // Handle rename syntax `R  old -> new`
        .map(|p| p.rsplit(" -> ").next().unwrap_or(p))
        .filter(|p| p.ends_with(".snap") || p.ends_with(".snap.new"))
        .map(|p| format!("snapshot file `{p}` changed without UpdateSnapshots authorization"))
        .collect()
}

/// Files the grunt may never touch: generated lockfile churn is ignored, and
/// rename syntax (`R  old -> new`) is resolved to the new path first.
const OUT_OF_SCOPE_IGNORED: &[&str] = &["Cargo.lock"];

pub fn out_of_scope_files(git_status_porcelain: &str, allowed_rel_path: &Path) -> Vec<String> {
    git_status_porcelain
        .lines()
        .filter_map(|line| line.get(3..))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.rsplit(" -> ").next().unwrap_or(p))
        .filter(|p| !OUT_OF_SCOPE_IGNORED.contains(&p))
        .filter(|p| Path::new(p) != allowed_rel_path)
        .map(|p| format!("unauthorized modification outside target_file: `{p}`"))
        .collect()
}

// ---------------------------------------------------------------------------
// Gate 2: nextest (libtest-json, fail-closed)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TestEvent {
    event: String,
    name: String,
    #[serde(default)]
    exec_time: Option<f64>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailedTest {
    pub name: String,
    pub reason: String,
    pub output_excerpt: String,
    pub exec_time_secs: Option<f64>,
}

/// Fail-closed nextest parser (`--message-format libtest-json` +
/// `NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1`, run with `--no-fail-fast`):
///   - Any malformed JSON line returns `Err` (never silently skipped).
///   - Requires a terminal `{"type":"suite","event":"ok"|"failed"}` event so a
///     truncated stream cannot pass.
///   - Requires `total > 0` executed tests so deleting all tests cannot pass.
pub fn parse_nextest_json(stdout: &str) -> Result<(Vec<FailedTest>, usize)> {
    let mut failed = Vec::new();
    let mut total = 0usize;
    let mut suite_finished = false;

    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("malformed nextest JSON at line {}", idx + 1))?;

        match value["type"].as_str() {
            Some("suite") => {
                if matches!(value["event"].as_str(), Some("ok" | "failed")) {
                    suite_finished = true;
                }
            }
            Some("test") => {
                let ev: TestEvent = serde_json::from_value(value)
                    .with_context(|| format!("invalid test event at line {}", idx + 1))?;
                if ev.event == "started" || ev.event == "ignored" {
                    continue;
                }
                total += 1;
                if ev.event != "ok" {
                    failed.push(FailedTest {
                        name: ev.name,
                        // Timeouts surface as event=failed with reason="time limit exceeded".
                        reason: ev.reason.unwrap_or_else(|| ev.event.clone()),
                        output_excerpt: ev
                            .stdout
                            .unwrap_or_default()
                            .lines()
                            .take(20)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        exec_time_secs: ev.exec_time,
                    });
                }
            }
            _ => {}
        }
    }

    if !suite_finished {
        anyhow::bail!("nextest JSON stream truncated or missing terminal suite event");
    }
    if total == 0 {
        anyhow::bail!("nextest executed 0 tests (test suite missing or deleted)");
    }

    Ok((failed, total))
}

// ---------------------------------------------------------------------------
// Gate 3: Diff-scoped coverage (cargo-llvm-cov --lcov)
// ---------------------------------------------------------------------------

/// Minimal unified-diff parser: added line numbers per file (new-side counter;
/// deletions do not advance it).
pub fn added_lines_from_diff(diff: &str) -> HashMap<PathBuf, Vec<u32>> {
    let mut result: HashMap<PathBuf, Vec<u32>> = HashMap::new();
    let mut current_file: Option<PathBuf> = None;
    let mut new_line: u32 = 0;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = Some(PathBuf::from(path.trim_end_matches('\r')));
        } else if line.starts_with("@@") {
            // @@ -a,b +c,d @@ -> new side starts at c.
            if let Some(rest) = line.split('+').nth(1) {
                new_line = rest
                    .split([',', ' '])
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if let (Some(file), true) = (&current_file, new_line > 0) {
                result.entry(file.clone()).or_default().push(new_line);
            }
            new_line += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            // deletions don't advance the new-side counter
        } else if !line.starts_with('\\') {
            new_line += 1;
        }
    }
    result
}

/// Parse LCOV `SF:`/`DA:line,hits` records into per-file line-hit maps.
/// `SF:` paths from cargo-llvm-cov are absolute; callers must match by suffix.
pub fn parse_lcov(lcov: &str) -> HashMap<PathBuf, HashMap<u32, u32>> {
    let mut files: HashMap<PathBuf, HashMap<u32, u32>> = HashMap::new();
    let mut current: Option<PathBuf> = None;

    for line in lcov.lines() {
        if let Some(sf) = line.strip_prefix("SF:") {
            current = Some(PathBuf::from(sf.trim()));
        } else if let Some(da) = line.strip_prefix("DA:") {
            if let Some(file) = &current {
                let mut parts = da.split(',');
                if let (Some(l), Some(h)) = (parts.next(), parts.next()) {
                    if let (Ok(line_no), Ok(hits)) = (l.parse::<u32>(), h.parse::<u32>()) {
                        files.entry(file.clone()).or_default().insert(line_no, hits);
                    }
                }
            }
        } else if line.starts_with("end_of_record") {
            current = None;
        }
    }
    files
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UncoveredLine {
    pub file: PathBuf,
    pub line: u32,
}

/// The coverage gate, v1.1 semantics:
///   * only `.rs` files participate;
///   * relative diff paths are matched against absolute LCOV `SF:` paths by
///     suffix;
///   * a line is flagged ONLY when LCOV explicitly instrumented it as
///     executable with 0 hits (`hits.get(&l) == Some(&0)`). Comments, blank
///     lines, imports and braces carry no `DA:` record and are never flagged;
///   * a matched file with NO executable lines at all, or a file that was
///     never executed, still rejects every added line (fail-closed).
pub fn uncovered_added_lines(diff: &str, lcov: &str) -> Vec<UncoveredLine> {
    let added = added_lines_from_diff(diff);
    let coverage = parse_lcov(lcov);
    let mut out = Vec::new();

    for (diff_file, lines) in added {
        if diff_file.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let matched_hits = coverage
            .iter()
            .find(|(sf_path, _)| sf_path.ends_with(&diff_file))
            .map(|(_, hits)| hits);

        match matched_hits {
            Some(hits) if !hits.is_empty() => {
                for l in lines {
                    // Non-executable lines (comments, imports, closing braces) have no
                    // DA: entry. Only flag lines LLVM explicitly instrumented with 0 hits.
                    if hits.get(&l) == Some(&0) {
                        out.push(UncoveredLine { file: diff_file.clone(), line: l });
                    }
                }
            }
            _ => {
                // File never instrumented / never executed: every added line uncovered.
                for l in lines {
                    out.push(UncoveredLine { file: diff_file.clone(), line: l });
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    out
}

// ---------------------------------------------------------------------------
// Gate 4: cargo-mutants (mutants.out/outcomes.json — NO streaming JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MutantsOutcomes {
    #[serde(default)]
    pub outcomes: Vec<OutcomeEntry>,
    #[serde(default)]
    pub total_mutants: usize,
    #[serde(default)]
    pub missed: usize,
    #[serde(default)]
    pub caught: usize,
    #[serde(default)]
    pub timeout: usize,
    #[serde(default)]
    pub unviable: usize,
}

#[derive(Debug, Deserialize)]
pub struct OutcomeEntry {
    #[serde(default)]
    pub scenario: serde_json::Value,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SurvivingMutant {
    pub scenario: String,
    pub log_path: String,
}

#[derive(Debug, Clone)]
pub struct MutantsReport {
    pub survivors: Vec<SurvivingMutant>,
    pub total: usize,
    pub missed: usize,
    pub caught: usize,
    pub unviable: usize,
    pub timeouts: usize,
}

/// Load + classify outcomes.json. `Unviable` is IGNORED (the mutator produced
/// something that doesn't compile — not a test gap).
pub fn load_mutants_report(mutants_out_dir: &Path) -> Result<MutantsReport> {
    let path = mutants_out_dir.join("outcomes.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let parsed: MutantsOutcomes =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    let survivors: Vec<SurvivingMutant> = parsed
        .outcomes
        .iter()
        .filter(|o| o.summary.as_deref() == Some("MissedMutant"))
        .map(|o| SurvivingMutant {
            scenario: o.scenario.to_string(),
            log_path: o.log_path.clone().unwrap_or_default(),
        })
        .collect();

    Ok(MutantsReport {
        survivors,
        total: parsed.total_mutants,
        missed: parsed.missed,
        caught: parsed.caught,
        unviable: parsed.unviable,
        timeouts: parsed.timeout,
    })
}

// ---------------------------------------------------------------------------
// Subprocess Runner with Process-Group Kill-on-Timeout
// ---------------------------------------------------------------------------

struct CmdOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_worktree_cmd_with_timeout(
    worktree: &EphemeralWorktree,
    program: &str,
    args: &[String],
    extra_env: &[(&str, &str)],
    timeout: Duration,
) -> Result<CmdOutput> {
    let mut cmd = AsyncCommand::new(program);
    cmd.current_dir(&worktree.path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    worktree.apply_env(&mut cmd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let mut group = cmd
        .group_spawn()
        .with_context(|| format!("spawning process group for {program}"))?;

    let mut stdout_pipe = group.inner().stdout.take().expect("stdout piped");
    let mut stderr_pipe = group.inner().stderr.take().expect("stderr piped");

    let read_stdout = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });
    let read_stderr = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    match tokio::time::timeout(timeout, group.wait()).await {
        Ok(Ok(status)) => Ok(CmdOutput {
            exit_code: status.code(),
            stdout: read_stdout.await.unwrap_or_default(),
            stderr: read_stderr.await.unwrap_or_default(),
        }),
        Ok(Err(e)) => {
            let _ = group.kill().await;
            Err(e).context(format!("waiting on {program}"))
        }
        Err(_) => {
            let _ = group.kill().await;
            anyhow::bail!("process group `{program}` timed out after {}s", timeout.as_secs())
        }
    }
}

// ---------------------------------------------------------------------------
// The 4-stage Verifier chain
// ---------------------------------------------------------------------------

/// Execute the deterministic Verifier gates inside the isolated worktree.
///
/// `affected_packages` should contain the modified crate and its transitive
/// dependents (from the krates graph) so nextest/llvm-cov/mutants only run the
/// affected DAG slice; pass empty to fall back to `--workspace`.
pub async fn run_verifier_chain(
    worktree: &EphemeralWorktree,
    spec: &ValidatedSubtaskSpec,
    base_ref: &str,
    affected_packages: &[String],
    stage_timeout: Duration,
) -> Result<VerifierOutcome> {
    // 1. Snapshot & Scope Guard (git status --porcelain) — runs BEFORE any
    //    commit: grunt's changes are still uncommitted working-tree edits.
    let status_out = run_worktree_cmd_with_timeout(
        worktree,
        "git",
        &["status".into(), "--porcelain".into()],
        &[],
        Duration::from_secs(10),
    )
    .await?;

    let allow_snaps = spec.task_kind == TaskKind::UpdateSnapshots;
    let mut violations = snapshot_violations(&status_out.stdout, allow_snaps);
    violations.extend(out_of_scope_files(&status_out.stdout, &spec.relative_target_file));

    if !violations.is_empty() {
        return Ok(VerifierOutcome::Rejected(VerifierRejection {
            gate: "snapshot_and_scope_guard",
            summary: "Unauthorized file or snapshot modifications detected.".into(),
            failed_tests: vec![],
            uncovered_lines: vec![],
            surviving_mutants: vec![],
            violations,
        }));
    }

    // 2. Empty-diff guard. `git add -N .` (intent-to-add) makes NEW files
    //    visible to `git diff <base_ref>`, which plain untracked files are not.
    let _ = run_worktree_cmd_with_timeout(
        worktree,
        "git",
        &["add".into(), "-N.".into()],
        &[],
        Duration::from_secs(10),
    )
    .await?;

    let diff_out = run_worktree_cmd_with_timeout(
        worktree,
        "git",
        &[
            "diff".into(),
            base_ref.into(),
            "--".into(),
            spec.relative_target_file.display().to_string(),
        ],
        &[],
        Duration::from_secs(10),
    )
    .await?;

    if diff_out.stdout.trim().is_empty() {
        return Ok(VerifierOutcome::Rejected(VerifierRejection {
            gate: "empty_diff",
            summary: "No changes were made to the target file.".into(),
            failed_tests: vec![],
            uncovered_lines: vec![],
            surviving_mutants: vec![],
            violations: vec![],
        }));
    }

    let pkg_flags: Vec<String> = if affected_packages.is_empty() {
        vec!["--workspace".into()]
    } else {
        affected_packages
            .iter()
            .flat_map(|p| ["-p".to_string(), p.clone()])
            .collect()
    };

    // 3. Gate: cargo-nextest (--no-fail-fast + libtest-json; fail-closed parse).
    let mut nextest_args = vec!["nextest".into(), "run".into()];
    nextest_args.extend(pkg_flags.clone());
    nextest_args.extend([
        "--no-fail-fast".into(),
        "--message-format".into(),
        "libtest-json".into(),
    ]);

    let nextest_out = run_worktree_cmd_with_timeout(
        worktree,
        "cargo",
        &nextest_args,
        &[("NEXTEST_EXPERIMENTAL_LIBTEST_JSON", "1")],
        stage_timeout,
    )
    .await?;

    match parse_nextest_json(&nextest_out.stdout) {
        Ok((failed, _total)) if !failed.is_empty() => {
            return Ok(VerifierOutcome::Rejected(VerifierRejection {
                gate: "nextest",
                summary: format!("{} test(s) failed during verification.", failed.len()),
                failed_tests: failed,
                uncovered_lines: vec![],
                surviving_mutants: vec![],
                violations: vec![],
            }));
        }
        Err(parse_err) => {
            // Fail-closed: surface parse error + stderr excerpt.
            return Ok(VerifierOutcome::Rejected(VerifierRejection {
                gate: "nextest",
                summary: format!(
                    "Fail-closed: nextest output unreadable ({parse_err}). stderr: {}",
                    nextest_out.stderr.chars().take(500).collect::<String>()
                ),
                failed_tests: vec![],
                uncovered_lines: vec![],
                surviving_mutants: vec![],
                violations: vec![],
            }));
        }
        Ok(_) => {}
    }

    if spec.task_kind == TaskKind::UpdateSnapshots {
        return Ok(VerifierOutcome::Passed);
    }

    // 4. Gate: Diff-scoped Coverage (cargo-llvm-cov --lcov).
    if spec.task_kind == TaskKind::Implement || spec.task_kind == TaskKind::Refactor {
        let lcov_file = worktree.target_dir.join("coverage.lcov");
        let mut cov_args = vec!["llvm-cov".into()];
        cov_args.extend(pkg_flags.clone());
        cov_args.extend(["--lcov".into(), "--output-path".into(), lcov_file.display().to_string()]);

        let cov_out =
            run_worktree_cmd_with_timeout(worktree, "cargo", &cov_args, &[], stage_timeout).await?;
        let lcov_raw = tokio::fs::read_to_string(&lcov_file).await.unwrap_or_default();

        if cov_out.exit_code != Some(0) || lcov_raw.is_empty() {
            return Ok(VerifierOutcome::Rejected(VerifierRejection {
                gate: "coverage",
                summary: format!(
                    "Fail-closed: cargo-llvm-cov failed (code {:?}): {}",
                    cov_out.exit_code,
                    cov_out.stderr.chars().take(400).collect::<String>()
                ),
                failed_tests: vec![],
                uncovered_lines: vec![],
                surviving_mutants: vec![],
                violations: vec![],
            }));
        }

        let uncovered = uncovered_added_lines(&diff_out.stdout, &lcov_raw);
        if !uncovered.is_empty() {
            return Ok(VerifierOutcome::Rejected(VerifierRejection {
                gate: "coverage",
                summary: format!("{} added executable line(s) have 0 test coverage.", uncovered.len()),
                failed_tests: vec![],
                uncovered_lines: uncovered,
                surviving_mutants: vec![],
                violations: vec![],
            }));
        }
    }

    // 5. Gate: Diff-scoped Mutation Testing (cargo-mutants --in-diff).
    if spec.task_kind == TaskKind::Implement || spec.task_kind == TaskKind::Refactor {
        let diff_file = worktree.target_dir.join("subtask.diff");
        let mutants_dir = worktree.target_dir.join("mutants_run");
        let _ = tokio::fs::remove_dir_all(&mutants_dir).await;
        tokio::fs::write(&diff_file, &diff_out.stdout).await?;

        let mut mutants_args = vec![
            "mutants".into(),
            "--in-diff".into(),
            diff_file.display().to_string(),
            "--output".into(),
            mutants_dir.display().to_string(),
        ];
        mutants_args.extend(pkg_flags);

        let mut_out =
            run_worktree_cmd_with_timeout(worktree, "cargo", &mutants_args, &[], stage_timeout)
                .await?;

        // cargo-mutants exit codes: 0=clean, 2=missed mutants, 3=timeout,
        // 4=baseline failed, 5/6=--in-diff usage errors.
        // Depending on the cargo-mutants version, outcomes.json lands in
        // <output>/mutants.out/ or directly in <output>/ — try both, fail closed.
        let report_dir_a = mutants_dir.join("mutants.out");
        match mut_out.exit_code {
            Some(0) => {}
            Some(2) | Some(3) => {
                let report = load_mutants_report(&report_dir_a)
                    .or_else(|_| load_mutants_report(&mutants_dir))
                    .with_context(|| {
                        format!(
                            "mutants exited {:?} but outcomes.json unreadable (tried {} and {})",
                            mut_out.exit_code,
                            report_dir_a.display(),
                            mutants_dir.display()
                        )
                    })?;
                return Ok(VerifierOutcome::Rejected(VerifierRejection {
                    gate: "mutants",
                    summary: format!(
                        "Mutation gate failed: {} missed mutant(s), {} timeout(s).",
                        report.missed, report.timeouts
                    ),
                    failed_tests: vec![],
                    uncovered_lines: vec![],
                    surviving_mutants: report.survivors,
                    violations: vec![],
                }));
            }
            other => {
                return Ok(VerifierOutcome::Rejected(VerifierRejection {
                    gate: "mutants",
                    summary: format!(
                        "Fail-closed: cargo-mutants exited with code {other:?}: {}",
                        mut_out.stderr.chars().take(400).collect::<String>()
                    ),
                    failed_tests: vec![],
                    uncovered_lines: vec![],
                    surviving_mutants: vec![],
                    violations: vec![],
                }));
            }
        }
    }

    Ok(VerifierOutcome::Passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEXT: &str = r#"{"type":"suite","event":"started","test_count":2}
{"type":"test","event":"ok","name":"blockcell-daemon::bin$token_basic","exec_time":0.01}
{"type":"test","event":"failed","name":"blockcell-daemon::bin$token_depletion","exec_time":0.2,"stdout":"assertion failed: left: 950, right: 900","reason":null}
{"type":"test","event":"failed","name":"blockcell-daemon::bin$drain_loop","exec_time":60.0,"stdout":"","reason":"time limit exceeded"}
{"type":"suite","event":"failed","passed":1,"failed":2}"#;

    #[test]
    fn nextest_parse_extracts_failures_and_timeouts() {
        let (failed, total) = parse_nextest_json(NEXT).unwrap();
        // total counts executed tests (1 ok + 2 failed), not failures.
        assert_eq!(total, 3);
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].name, "blockcell-daemon::bin$token_depletion");
        assert!(failed[0].output_excerpt.contains("950"));
        assert_eq!(failed[1].reason, "time limit exceeded");
    }

    #[test]
    fn nextest_parse_fails_closed_on_truncated_or_empty_runs() {
        // Truncated stream: no terminal suite event.
        let truncated = r#"{"type":"suite","event":"started","test_count":1}
{"type":"test","event":"ok","name":"a::b","exec_time":0.01}"#;
        assert!(parse_nextest_json(truncated).is_err());
        // Malformed line: hard error, never skipped.
        let malformed = format!("{NEXT}\nnot-json-at-all");
        assert!(parse_nextest_json(&malformed).is_err());
        // Deleted suite: terminal event present but 0 tests executed.
        let empty = r#"{"type":"suite","event":"started","test_count":0}
{"type":"suite","event":"ok","passed":0,"failed":0}"#;
        assert!(parse_nextest_json(empty).is_err());
        assert!(parse_nextest_json(NEXT).is_ok());
    }

    #[test]
    fn coverage_gate_matches_absolute_lcov_and_ignores_comments() {
        // Line 2 is a comment (no DA:), line 3 is executable with 0 hits,
        // line 4 has 2 hits. SF: is absolute, diff path is relative.
        let diff = "--- a/crates/core/src/session.rs\n+++ b/crates/core/src/session.rs\n@@ -1,1 +1,4 @@\n kept\n+// comment\n+untested_call();\n+tested_call();\n";
        let lcov = "SF:/abs/workspace/crates/core/src/session.rs\nDA:1,5\nDA:3,0\nDA:4,2\nend_of_record\n";
        let out = uncovered_added_lines(diff, lcov);
        assert_eq!(
            out,
            vec![UncoveredLine { file: PathBuf::from("crates/core/src/session.rs"), line: 3 }]
        );
    }

    #[test]
    fn coverage_gate_flags_uncovered_additions() {
        let diff = "--- a/src/session.rs\n+++ b/src/session.rs\n@@ -1,1 +1,2 @@\n a\n+never_run\n";
        let lcov = "SF:src/session.rs\nDA:1,5\nDA:2,0\nend_of_record\n";
        let out = uncovered_added_lines(diff, lcov);
        assert_eq!(out, vec![UncoveredLine { file: PathBuf::from("src/session.rs"), line: 2 }]);
    }

    #[test]
    fn diff_added_lines_are_extracted() {
        let diff = "--- a/src/session.rs\n+++ b/src/session.rs\n@@ -10,3 +10,4 @@\n kept\n+added_one\n+added_two\n ctx\n";
        let m = added_lines_from_diff(diff);
        assert_eq!(m[Path::new("src/session.rs")], vec![11, 12]);
    }

    #[test]
    fn mutants_report_ignores_unviable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("outcomes.json"),
            r#"{"outcomes":[
                {"scenario":"mutant #1","summary":"MissedMutant","log_path":"log1"},
                {"scenario":"mutant #2","summary":"CaughtMutant"},
                {"scenario":"mutant #3","summary":"Unviable"}],
               "total_mutants":3,"missed":1,"caught":1,"timeout":0,"unviable":1}"#,
        )
        .unwrap();
        let report = load_mutants_report(dir.path()).unwrap();
        assert_eq!(report.survivors.len(), 1);
        assert_eq!(report.survivors[0].log_path, "log1");
        assert_eq!(report.unviable, 1);
    }

    #[test]
    fn snapshot_guard_blocks_unauthorized_updates() {
        let status = " M crates/core/src/lib.rs\n?? crates/core/src/snapshots/out.snap.new\n";
        assert_eq!(snapshot_violations(status, false).len(), 1);
        assert!(snapshot_violations(status, true).is_empty());
    }

    #[test]
    fn snapshot_guard_resolves_rename_syntax() {
        let status = "R  crates/core/src/snapshots/old.snap -> crates/core/src/snapshots/new.snap\n";
        let v = snapshot_violations(status, false);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("new.snap"), "{v:?}");
    }

    #[test]
    fn scope_guard_flags_out_of_scope_edits_only() {
        let status = " M crates/core/src/session.rs\n M Cargo.lock\n?? crates/other/src/evil.rs\n";
        let allowed = Path::new("crates/core/src/session.rs");
        let v = out_of_scope_files(status, allowed);
        assert_eq!(v.len(), 1, "Cargo.lock ignored, session.rs allowed, evil.rs flagged");
        assert!(v[0].contains("evil.rs"), "{v:?}");
    }
}

