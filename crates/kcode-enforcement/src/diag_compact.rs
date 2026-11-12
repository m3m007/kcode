//! T0.5 — Diagnostic compactor: cargo JSON diagnostics -> <=4 compact errors.
//!
//! Uses `cargo_metadata::Message::parse_stream` (verified correct entry point
//! for `cargo check --message-format=json`) instead of hand-rolled structs.
//! Output discipline for Flash workers:
//!   * errors only (warnings handled at the clippy gate),
//!   * primary span location + headline + first `help:` suggestion,
//!   * dedup by location, hard cap (default 4) to stop cascade drowning,
//!   * ~35 tokens per error instead of ~450.

use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel};
use cargo_metadata::Message;
use serde::Serialize;
use std::collections::HashSet;
use std::io::BufRead;

#[derive(Debug, Clone, Serialize)]
pub struct CompactDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub location: String,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

pub struct DiagnosticCompactor {
    pub max_errors_to_report: usize,
}

impl Default for DiagnosticCompactor {
    fn default() -> Self {
        Self { max_errors_to_report: 4 }
    }
}

impl DiagnosticCompactor {
    pub fn parse_and_compact<R: BufRead>(&self, stdout: R) -> Vec<CompactDiagnostic> {
        let messages = Message::parse_stream(stdout);
        let mut results: Vec<CompactDiagnostic> = Vec::new();
        let mut seen_locations: HashSet<String> = HashSet::new();

        for msg in messages {
            let Ok(Message::CompilerMessage(cm)) = msg else { continue };
            let diag: Diagnostic = cm.message;
            if diag.level != DiagnosticLevel::Error {
                continue;
            }

            let primary = diag.spans.iter().find(|s| s.is_primary);
            let location = primary
                .map(|s| format!("{}:{}:{}", s.file_name, s.line_start, s.column_start))
                .unwrap_or_else(|| "unknown".into());
            if !seen_locations.insert(location.clone()) {
                continue;
            }

            let error_text = match primary.and_then(|s| s.label.as_deref()) {
                Some(label) if !label.is_empty() => format!("{}: {label}", diag.message),
                _ => diag.message.clone(),
            };

            // rustc models suggestions as `help:` children; the concrete
            // replacement text (if any) hangs off their spans.
            let help = diag.children.iter().find(|c| c.level == DiagnosticLevel::Help).map(|c| {
                let repl = c
                    .spans
                    .iter()
                    .find_map(|s| s.suggested_replacement.clone())
                    .unwrap_or_default();
                if repl.is_empty() {
                    c.message.clone()
                } else {
                    format!("{}: `{repl}`", c.message)
                }
            });

            results.push(CompactDiagnostic {
                code: diag.code.map(|c| c.code),
                location,
                error: sanitize(&error_text),
                help: help.map(|h| sanitize(&h)),
            });

            if results.len() >= self.max_errors_to_report {
                break;
            }
        }
        results
    }
}

fn sanitize(input: &str) -> String {
    input.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"reason":"compiler-message","package_id":"blockcell-daemon 0.1.0","target":{"kind":["bin"],"crate_types":["bin"],"name":"daemon","src_path":"/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"message":{"children":[{"children":[],"code":null,"level":"note","message":"first mutable borrow occurs here","rendered":null,"spans":[]},{"children":[],"code":null,"level":"help","message":"consider using a separate block to end the borrow earlier","rendered":null,"spans":[]}],"code":{"code":"E0499","explanation":null},"level":"error","message":"cannot borrow `tracker` as mutable more than once at a time","rendered":"...", "spans":[{"byte_end":512,"byte_start":505,"column_end":28,"column_start":21,"file_name":"src/session.rs","is_primary":true,"label":"second mutable borrow occurs here","line_end":18,"line_start":18,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}
{"reason":"build-finished","success":false}"#;

    #[test]
    fn compacts_borrow_error_to_single_record() {
        let out = DiagnosticCompactor::default().parse_and_compact(SAMPLE.as_bytes());
        assert_eq!(out.len(), 1);
        let d = &out[0];
        assert_eq!(d.code.as_deref(), Some("E0499"));
        assert_eq!(d.location, "src/session.rs:18:21");
        assert!(d.error.contains("cannot borrow `tracker`"));
        assert!(d.help.as_deref().unwrap().contains("separate block"));
    }

    #[test]
    fn empty_stream_yields_no_errors() {
        let out = DiagnosticCompactor::default().parse_and_compact("".as_bytes());
        assert!(out.is_empty());
    }
}
