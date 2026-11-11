//! T1.5 — Grunt Worker Turn Loop + Startup Orphan Sweeper + Safe Merge Coordinator.
//!
//! The v1.1 external review identified this as missing glue: nothing connected
//! the P0 modules into a single state machine. This module wires together
//! enforcement (T0.1), dedup (T0.2), ast_guard (T0.3), agent_check.sh (T0.5),
//! subtask (T1.1), worktree (T1.2) and verifier (T1.4):
//!   - Confines all writes to `spec.relative_target_file` inside the worktree
//!     (defense in depth: also re-checked by the verifier scope guard).
//!   - Runs `AstGuardrail::check_forbidden` and `check_public_interface` BEFORE
//!     writing to disk.
//!   - Runs `agent_check.sh` immediately after each write; if rejected, restores
//!     the pre-turn file buffer and returns the compacted JSON diagnostics to
//!     the grunt without polluting git state (turn-level rollback).
//!   - Task-level rollback is worktree-native: on verifier rejection the
//!     worktree is discarded; the main checkout was never touched (v1.1
//!     eliminated the T0.4 in-memory snapshot transaction).
//!   - Serializes `merge_into` across concurrent waves with an orchestrator
//!     Mutex (`.git/index.lock` race, review finding G3).
//!   - `sweep_orphan_worktrees` self-heals SIGKILL crashes at startup (R3).

use jcode_enforcement::ast_guard::AstGuardrail;
use jcode_enforcement::dedup::{close_tool_result, DedupAction, ToolCall, ToolDedupEngine};
use jcode_enforcement::enforcement::{
    run_turn_with_enforcement, AgentPhase, LlmTransport, ModelTurn, ProviderCaps, ToolInvocation,
};
use crate::subtask::{TaskKind, ValidatedSubtaskSpec};
use crate::verifier::{run_verifier_chain, VerifierOutcome};
use crate::worktree::EphemeralWorktree;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command as AsyncCommand;
use tokio::sync::Mutex;

/// Sweep orphaned `<repo>/target/agent_worktrees/*` dirs and stale
/// `agent/task-*` branches left behind by SIGKILL crashes (Risk R3).
/// Call once at orchestrator startup, BEFORE the main checkout is pinned.
pub async fn sweep_orphan_worktrees(repo_root: &Path) -> Result<()> {
    let wt_root = repo_root.join("target/agent_worktrees");
    if wt_root.exists() {
        let mut rd = tokio::fs::read_dir(&wt_root).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let _ = AsyncCommand::new("git")
                .current_dir(repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .output()
                .await;
            if path.exists() {
                let _ = tokio::fs::remove_dir_all(&path).await;
            }
        }
    }
    let _ = AsyncCommand::new("git")
        .current_dir(repo_root)
        .args(["worktree", "prune"])
        .output()
        .await;

    // Clean up any leftover `agent/task-*` branches
    let branches = AsyncCommand::new("git")
        .current_dir(repo_root)
        .args(["branch", "--list", "agent/task-*"])
        .output()
        .await?;
    for line in String::from_utf8_lossy(&branches.stdout).lines() {
        let branch = line.trim().trim_start_matches("* ");
        if !branch.is_empty() {
            let _ = AsyncCommand::new("git")
                .current_dir(repo_root)
                .args(["branch", "-D", branch])
                .output()
                .await;
        }
    }
    Ok(())
}

/// Read-only / search tools delegated to the host environment (MCP / local FS).
pub trait ReadOnlyToolHost: Send + Sync {
    fn execute_readonly(
        &self,
        worktree_root: &Path,
        call: &ToolInvocation,
    ) -> impl std::future::Future<Output = Result<Value>> + Send;
}

/// Execute a single Grunt subtask inside an isolated worktree up to
/// `spec.max_turns`, followed by the 4-stage Verifier chain and a serialized
/// branch merge. On verifier rejection the worktree is simply discarded — the
/// main checkout is never touched (that IS the v1.1 rollback story).
#[allow(clippy::too_many_arguments)]
pub async fn execute_grunt_subtask<T, H>(
    transport: &T,
    tool_host: &H,
    model: &str,
    caps: ProviderCaps,
    tools_schema: &[Value],
    repo_root: &Path,
    task_id: &str,
    base_ref: &str,
    integration_branch: &str,
    spec: &ValidatedSubtaskSpec,
    affected_packages: &[String],
    merge_lock: &Mutex<()>,
) -> Result<VerifierOutcome>
where
    T: LlmTransport + Sync,
    H: ReadOnlyToolHost,
{
    let worktree = EphemeralWorktree::create(repo_root, task_id, base_ref).await?;
    let worktree_target_file = worktree.path.join(&spec.relative_target_file);
    let baseline_source = tokio::fs::read_to_string(&worktree_target_file).await.ok();

    let ast_guard = AstGuardrail::new().context("initializing AstGuardrail")?;
    let mut dedup = ToolDedupEngine::new(2, 16, 4, 6);

    // Front-load immutable rules in the system prompt: identical prefixes
    // across every call maximize automatic prompt-cache hits (T2.6).
    let mut messages = vec![
        json!({
            "role": "system",
            "content": "You are a deterministic Rust Grunt worker. Every turn in Exploration or \
                        Modification phase MUST invoke a tool. Never use #[allow], #[expect], \
                        panic!, println!, dbg!, or .unwrap(). Only modify the assigned target_file."
        }),
        json!({
            "role": "user",
            "content": serde_json::to_string(spec).unwrap_or_else(|_| spec.task_description.clone())
        }),
    ];

    let mut phase = AgentPhase::Exploration;
    let mut edited_successfully = false;

    for _turn_idx in 0..spec.max_turns {
        let turn = run_turn_with_enforcement(
            transport,
            model,
            &mut messages,
            tools_schema,
            caps,
            phase,
            3,
        )
        .await?;

        let calls = match turn {
            ModelTurn::ToolCalls(c) => c,
            ModelTurn::Text(_) => break, // Reached Summary phase
        };

        for call in calls {
            let dedup_call = ToolCall {
                name: call.name.clone(),
                payload: call.arguments.clone(),
            };

            let tool_output = match dedup.inspect(&dedup_call) {
                DedupAction::RejectLoop { reason } => {
                    close_tool_result(json!({ "error": reason }), false, "Choose a different action or edit.")
                }
                DedupAction::Execute => {
                    if call.name == "write_file" {
                        phase = AgentPhase::Modification;
                        let outcome = handle_write_file(
                            &worktree,
                            &worktree_target_file,
                            baseline_source.as_deref(),
                            spec,
                            &ast_guard,
                            &call.arguments,
                        )
                        .await?;

                        if outcome["status"] == json!("PASSED") {
                            dedup.notify_workspace_mutation();
                            edited_successfully = true;
                            // Single-file task: once the fast gate passes, converge
                            // to Summary and let the verifier chain be the judge.
                            phase = AgentPhase::Summary;
                        }
                        outcome
                    } else {
                        let raw = tool_host.execute_readonly(&worktree.path, &call).await?;
                        close_tool_result(raw, true, "Proceed to next step.")
                    }
                }
            };

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": serde_json::to_string(&tool_output)?
            }));
        }

        if edited_successfully && phase == AgentPhase::Summary {
            break;
        }
    }

    // Run the full 4-stage Verifier chain (uncommitted working-tree state).
    let verification = run_verifier_chain(
        &worktree,
        spec,
        base_ref,
        affected_packages,
        Duration::from_secs(180),
    )
    .await?;

    if matches!(verification, VerifierOutcome::Passed) {
        worktree
            .commit_changes(&format!("feat(swarm): complete subtask {task_id}"))
            .await?;
        // Serialize merges: concurrent `git merge` on repo_root collides on
        // .git/index.lock (review finding G3).
        let _guard = merge_lock.lock().await;
        worktree.merge_into(integration_branch).await?;
    }

    // Rejected or passed: the worktree is disposable. Rejection path = nothing
    // merged, main checkout untouched, branch + worktree removed below.
    worktree.clean_up().await?;
    Ok(verification)
}

/// Validates single-file confinement, runs Tree-sitter AST guardrails, writes
/// the buffer, and invokes `scripts/agent_check.sh`. Automatically rolls back
/// the file buffer if `agent_check.sh` rejects the edit (turn-level rollback).
async fn handle_write_file(
    worktree: &EphemeralWorktree,
    expected_target: &Path,
    baseline_source: Option<&str>,
    spec: &ValidatedSubtaskSpec,
    ast_guard: &AstGuardrail,
    args: &Value,
) -> Result<Value> {
    let req_path = args["path"].as_str().unwrap_or_default();
    let new_content = args["content"].as_str().unwrap_or_default();

    // 1. Confinement check: grunt may ONLY write to `spec.relative_target_file`
    if Path::new(req_path) != spec.relative_target_file {
        return Ok(close_tool_result(
            json!({
                "status": "REJECTED",
                "phase": "confinement",
                "error": format!(
                    "Write denied: `{req_path}` is not your assigned target_file (`{}`).",
                    spec.relative_target_file.display()
                )
            }),
            false,
            "Write only to your assigned target_file.",
        ));
    }

    // 2. AST Guardrail check (only for .rs files)
    if expected_target.extension().and_then(|e| e.to_str()) == Some("rs") {
        if let Err(e) = ast_guard.check_forbidden(new_content) {
            return Ok(close_tool_result(
                json!({ "status": "REJECTED", "phase": "ast_forbidden", "error": e.to_string() }),
                false,
                "Remove banned attributes/macros/unwrap and retry.",
            ));
        }

        let enforce_sig_stability =
            !spec.allow_interface_changes && spec.task_kind != TaskKind::Refactor;
        if enforce_sig_stability {
            if let Some(orig) = baseline_source {
                if let Err(e) = ast_guard.check_public_interface(orig, new_content) {
                    return Ok(close_tool_result(
                        json!({ "status": "REJECTED", "phase": "ast_interface", "error": e.to_string() }),
                        false,
                        "Restore the original public signatures and retry.",
                    ));
                }
            }
        }
    }

    // 3. Capture prior disk state, write new content, and run `agent_check.sh`
    let prior_bytes = tokio::fs::read(expected_target).await.ok();
    if let Some(parent) = expected_target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(expected_target, new_content.as_bytes()).await?;

    let check_script = worktree.path.join("scripts/agent_check.sh");
    let mut cmd = AsyncCommand::new("bash");
    cmd.current_dir(&worktree.path)
        .arg(&check_script)
        .arg(&spec.relative_target_file);
    worktree.apply_env(&mut cmd);

    let check_res = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await;

    match check_res {
        Ok(Ok(out)) if out.status.success() => Ok(close_tool_result(
            json!({ "status": "PASSED" }),
            true,
            "Fast gate passed. Conclude your turn.",
        )),
        Ok(Ok(out)) => {
            // Rollback buffer on fast-gate rejection
            restore_prior_file(expected_target, prior_bytes).await?;
            let stdout_str = String::from_utf8_lossy(&out.stdout);
            let parsed = serde_json::from_str::<Value>(&stdout_str)
                .unwrap_or_else(|_| json!({ "status": "REJECTED", "raw": stdout_str }));
            Ok(close_tool_result(
                parsed,
                false,
                "Fix the reported compiler/clippy diagnostics and re-apply.",
            ))
        }
        _ => {
            restore_prior_file(expected_target, prior_bytes).await?;
            Ok(close_tool_result(
                json!({ "status": "REJECTED", "phase": "timeout", "error": "agent_check.sh timed out" }),
                false,
                "Simplify the change and retry.",
            ))
        }
    }
}

async fn restore_prior_file(target: &Path, prior: Option<Vec<u8>>) -> Result<()> {
    match prior {
        Some(bytes) => tokio::fs::write(target, bytes).await?,
        None => {
            if target.exists() {
                tokio::fs::remove_file(target).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restore_prior_file_recovers_deleted_and_created_state() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("src/lib.rs");
        tokio::fs::create_dir_all(target.parent().unwrap()).await.unwrap();

        // Case 1: file existed before -> content restored.
        tokio::fs::write(&target, b"original").await.unwrap();
        restore_prior_file(&target, Some(b"original".to_vec())).await.unwrap();
        assert_eq!(tokio::fs::read(&target).await.unwrap(), b"original");

        // Case 2: file did not exist before (grunt created it) -> removed.
        restore_prior_file(&target, None).await.unwrap();
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn orphan_sweeper_removes_stale_worktree_dirs() {
        // Needs git on PATH for the (ignored-error) worktree/prune calls.
        if AsyncCommand::new("git").arg("--version").output().await.is_err() {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let stale = repo.path().join("target/agent_worktrees/task-42");
        tokio::fs::create_dir_all(&stale).await.unwrap();
        tokio::fs::write(stale.join("leftover.rs"), b"junk").await.unwrap();

        sweep_orphan_worktrees(repo.path()).await.unwrap();

        assert!(!stale.exists(), "orphan worktree dir must be swept");
    }
}
