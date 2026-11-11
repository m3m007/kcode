//! T0.3 — Tree-sitter AST guardrails + public-interface tamper detection.
//!
//! Two uses:
//!   * `check_forbidden_attributes` — reject lint escapes (`#[allow]`,
//!     `#[expect]`), binary-level bypasses (`#[no_mangle]`, `#[export_name]`),
//!     banned primitives (`std::thread::spawn`, `panic!`, `dbg!`, `println!`,
//!     `eprintln!`, `.unwrap()`) BEFORE the file hits disk or cargo.
//!   * `check_public_interface_invariants` — diff the normalized signatures of
//!     every `pub` item between the pre-edit and post-edit buffers unless the
//!     subtask explicitly carries `allow_interface_changes`.
//!
//! Runs in <2 ms per file; no cargo invocation involved.

use std::collections::HashMap;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AstSecurityError {
    #[error("tree-sitter failed to parse buffer")]
    ParseFailure,
    #[error("forbidden attributes / constructs injected:\n{0}")]
    Forbidden(String),
    #[error("unauthorized public API signature alteration:\n{0}")]
    InterfaceTampered(String),
}

const FORBIDDEN_QUERY: &str = r#"
;; lint escapes and bypass attributes (attached or standalone)
(attribute
  (identifier) @attr_name (#match? @attr_name "^(allow|expect|no_mangle|export_name)$")
) @forbidden_attr

;; manual OS threads inside an async daemon — full std::thread::spawn path
(call_expression
  function: (scoped_identifier
    path: (scoped_identifier
      path: (identifier) @mod_name (#eq? @mod_name "std")
      name: (identifier) @sub_name (#eq? @sub_name "thread"))
    name: (identifier) @fn_name (#eq? @fn_name "spawn"))
) @prohibited_thread_spawn

;; v1.1: short form `thread::spawn(...)` (e.g. after `use std::thread;`)
(call_expression
  function: (scoped_identifier
    path: (identifier) @thread_mod (#eq? @thread_mod "thread")
    name: (identifier) @spawn_fn (#eq? @spawn_fn "spawn"))
) @prohibited_thread_spawn_short

;; scratchpad debugging macros — bare form
(macro_invocation
  macro: (identifier) @macro_name
  (#match? @macro_name "^(panic|dbg|println|eprintln)$")
) @prohibited_macro

;; v1.1: qualified form `std::panic!(...)` / `core::dbg!(...)` escapes the
;; bare identifier match, so also match any scoped macro path by its final name.
(macro_invocation
  macro: (scoped_identifier name: (identifier) @scoped_macro_name)
  (#match? @scoped_macro_name "^(panic|dbg|println|eprintln)$")
) @prohibited_macro_scoped

;; bare unwrap — force typed error handling (thiserror/anyhow)
(call_expression
  function: (field_expression
    field: (field_identifier) @method (#eq? @method "unwrap"))
) @prohibited_unwrap
"#;

/// Public items whose signatures must remain stable across a subtask.
const PUBLIC_ITEMS_QUERY: &str = r#"
(function_item
  (visibility_modifier) @vis (#eq? @vis "pub")
  name: (identifier) @item_name
) @pub_item

(struct_item
  (visibility_modifier) @vis (#eq? @vis "pub")
  name: (type_identifier) @item_name
) @pub_item

(enum_item
  (visibility_modifier) @vis (#eq? @vis "pub")
  name: (type_identifier) @item_name
) @pub_item

(trait_item
  (visibility_modifier) @vis (#eq? @vis "pub")
  name: (type_identifier) @item_name
) @pub_item

(type_item
  (visibility_modifier) @vis (#eq? @vis "pub")
  name: (type_identifier) @item_name
) @pub_item
"#;

pub struct AstGuardrail {
    language: Language,
    forbidden_query: Query,
    public_items_query: Query,
}

impl AstGuardrail {
    pub fn new() -> Result<Self, AstSecurityError> {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let forbidden_query =
            Query::new(&language, FORBIDDEN_QUERY).map_err(|_| AstSecurityError::ParseFailure)?;
        let public_items_query =
            Query::new(&language, PUBLIC_ITEMS_QUERY).map_err(|_| AstSecurityError::ParseFailure)?;
        Ok(Self { language, forbidden_query, public_items_query })
    }

    fn parse(&self, source: &str) -> Result<tree_sitter::Tree, AstSecurityError> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .map_err(|_| AstSecurityError::ParseFailure)?;
        parser.parse(source, None).ok_or(AstSecurityError::ParseFailure)
    }

    pub fn check_forbidden(&self, source: &str) -> Result<(), AstSecurityError> {
        let tree = self.parse(source)?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&self.forbidden_query, tree.root_node(), source.as_bytes());
        let mut violations: Vec<String> = Vec::new();

        while let Some(m) = matches.next() {
            let rule = self.forbidden_query.capture_names()[m.captures[0].index as usize];
            if let Some(capture) = m.captures.iter().find(|c| {
                self.forbidden_query.capture_names()[c.index as usize].starts_with("prohibited")
                    || self
                        .forbidden_query
                        .capture_names()[c.index as usize]
                        .starts_with("forbidden")
            }) {
                let node = capture.node;
                let pos = node.start_position();
                let snippet = &source[node.start_byte()..node.end_byte()];
                violations.push(format!(
                    "[{rule}] line {}, col {}: `{}` is banned. Use typed errors / tokio primitives / tracing.",
                    pos.row + 1,
                    pos.column + 1,
                    snippet.lines().next().unwrap_or(snippet)
                ));
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(AstSecurityError::Forbidden(violations.join("\n")))
        }
    }

    /// Compares normalized public signatures pre/post edit.
    /// `new` may ADD public items; it may not remove or alter existing ones.
    pub fn check_public_interface(
        &self,
        original: &str,
        new: &str,
    ) -> Result<(), AstSecurityError> {
        let before = self.extract_public_signatures(original)?;
        let after = self.extract_public_signatures(new)?;
        let mut violations = Vec::new();

        for (name, old_sig) in &before {
            match after.get(name) {
                None => violations.push(format!("pub item `{name}` was REMOVED. prior: `{old_sig}`")),
                Some(new_sig) if new_sig != old_sig => violations.push(format!(
                    "pub item `{name}` signature ALTERED.\n  before: `{old_sig}`\n  after:  `{new_sig}`"
                )),
                _ => {}
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(AstSecurityError::InterfaceTampered(violations.join("\n")))
        }
    }

    fn extract_public_signatures(
        &self,
        source: &str,
    ) -> Result<HashMap<String, String>, AstSecurityError> {
        let tree = self.parse(source)?;
        let mut cursor = QueryCursor::new();
        let mut matches =
            cursor.matches(&self.public_items_query, tree.root_node(), source.as_bytes());
        let mut signatures = HashMap::new();

        while let Some(m) = matches.next() {
            let mut item_name: Option<&str> = None;
            let mut item_node: Option<Node> = None;
            for capture in m.captures {
                let name = self.public_items_query.capture_names()[capture.index as usize];
                match name {
                    "item_name" => {
                        item_name = Some(&source[capture.node.start_byte()..capture.node.end_byte()])
                    }
                    n if n.starts_with("pub_") => item_node = Some(capture.node),
                    _ => {}
                }
            }
            if let (Some(name), Some(node)) = (item_name, item_node) {
                // v1.1: qualify with the enclosing impl type so `impl Foo::new`
                // and `impl Bar::new` do not collide in the signature map.
                let qualified_key = qualify_with_enclosing_impl(source, node, name);
                signatures.insert(qualified_key, normalized_item_signature(source, node));
            }
        }
        Ok(signatures)
    }
}

/// Walk up to an enclosing `impl_item` and qualify the key as
/// `impl <Type>::<name>` so same-named methods on different impls never
/// overwrite each other in the signature map (v1.1).
fn qualify_with_enclosing_impl(source: &str, node: Node, item_name: &str) -> String {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if p.kind() == "impl_item" {
            if let Some(ty) = p.child_by_field_name("type") {
                let ty_str = &source[ty.start_byte()..ty.end_byte()];
                return format!("impl {}::{}", ty_str.trim(), item_name);
            }
        }
        cur = p.parent();
    }
    item_name.to_string()
}

/// Skip leading attribute / doc-comment children so doc or derive churn is
/// never reported as interface tampering.
fn signature_body_start(source: &str, node: Node) -> usize {
    let _ = source;
    let mut start = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "attribute_item" | "attribute" | "line_comment" | "block_comment" => {
                start = child.end_byte();
            }
            _ => break,
        }
    }
    start
}

/// Normalized public-item signature, v1.1 semantics:
/// - Functions: strip parameter NAMES (keep types + return type + where
///   clause) and stop before the body `{` — renaming `val` to `v` is NOT
///   tampering (Acceptance Criterion 3 of the v1.0 spec, now actually true).
/// - Structs/enums/traits/type aliases: full definition (minus leading
///   attributes/comments), so deleting a `pub` field or an enum variant IS
///   caught — the v1.0 `signature_header` truncated at `{` and missed it.
fn normalized_item_signature(source: &str, node: Node) -> String {
    if node.kind() == "function_item" {
        let mut parts = Vec::new();
        let body_start = node
            .child_by_field_name("body")
            .map(|b| b.start_byte())
            .unwrap_or_else(|| node.end_byte());

        if let Some(params) = node.child_by_field_name("parameters") {
            let start = signature_body_start(source, node);
            let prefix = &source[start..params.start_byte()];
            parts.push(prefix.split_whitespace().collect::<Vec<_>>().join(" "));

            let mut param_types = Vec::new();
            let mut cursor = params.walk();
            for child in params.children(&mut cursor) {
                match child.kind() {
                    "parameter" => {
                        if let Some(ty) = child.child_by_field_name("type") {
                            let ty_s = &source[ty.start_byte()..ty.end_byte()];
                            param_types.push(ty_s.split_whitespace().collect::<Vec<_>>().join(" "));
                        } else {
                            // grammar variants without a `type` field (e.g. bare self)
                            let whole = &source[child.start_byte()..child.end_byte()];
                            param_types.push(whole.split_whitespace().collect::<Vec<_>>().join(" "));
                        }
                    }
                    "self_parameter" => {
                        let s = &source[child.start_byte()..child.end_byte()];
                        param_types.push(s.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                    _ => {}
                }
            }
            parts.push(format!("({})", param_types.join(", ")));

            let suffix = &source[params.end_byte()..body_start];
            let suffix_clean = suffix.trim().trim_end_matches(';');
            if !suffix_clean.is_empty() {
                parts.push(suffix_clean.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            return parts.join(" ");
        }
    }

    // structs / enums / traits / type aliases: full normalized definition
    let start = signature_body_start(source, node);
    source[start..node.end_byte()]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_allow_no_mangle_panic_unwrap() {
        let guard = AstGuardrail::new().unwrap();
        let bad = r#"
            #[allow(clippy::unwrap_used)]
            pub fn run_task() -> u32 {
                #[no_mangle]
                extern "C" fn foreign() {}
                if true { panic!("crash"); }
                Some(10).unwrap() as u32
            }
        "#;
        let err = guard.check_forbidden(bad).unwrap_err().to_string();
        assert!(err.contains("allow"), "missing allow: {err}");
        assert!(err.contains("no_mangle"), "missing no_mangle: {err}");
        assert!(err.contains("panic"), "missing panic: {err}");
        assert!(err.contains("unwrap"), "missing unwrap: {err}");
    }

    #[test]
    fn allows_clean_internal_changes() {
        let guard = AstGuardrail::new().unwrap();
        let ok = r#"
            pub fn compute(val: u32) -> u32 {
                if val == 0 { return 0; }
                val * 2
            }
            fn helper() -> Option<u32> { Some(1) }
        "#;
        assert!(guard.check_forbidden(ok).is_ok());
    }

    #[test]
    fn detects_signature_tampering() {
        let guard = AstGuardrail::new().unwrap();
        let original = "pub fn handle_connection(stream: u64, timeout: u64) -> u32 { 0 }";
        let tampered = "pub fn handle_connection(stream: u64) -> u32 { 0 }";
        let err = guard.check_public_interface(original, tampered).unwrap_err().to_string();
        assert!(err.contains("ALTERED"), "{err}");
    }

    #[test]
    fn allows_adding_new_public_items() {
        let guard = AstGuardrail::new().unwrap();
        let original = "pub fn compute(val: u32) -> u32 { 0 }";
        let extended = "pub fn compute(val: u32) -> u32 { 0 }\npub struct Extra { pub id: u64 }";
        assert!(guard.check_public_interface(original, extended).is_ok());
    }

    // ----- v1.1 regression tests (external review findings) -----

    #[test]
    fn parameter_rename_is_not_tampering() {
        let guard = AstGuardrail::new().unwrap();
        let original = "pub fn compute(val: u32) -> u32 { val * 2 }";
        let renamed = "pub fn compute(v: u32) -> u32 { v * 2 }";
        assert!(
            guard.check_public_interface(original, renamed).is_ok(),
            "param name change must pass (types unchanged)"
        );
    }

    #[test]
    fn same_named_methods_on_different_impls_do_not_collide() {
        let guard = AstGuardrail::new().unwrap();
        let original = "struct A;\nstruct B;\nimpl A { pub fn new() -> A { A } }\nimpl B { pub fn new() -> B { B } }";
        // Tamper ONLY impl B's signature; impl A must be unaffected by the map key.
        let tampered = "struct A;\nstruct B;\nimpl A { pub fn new() -> A { A } }\nimpl B { pub fn new(x: u8) -> B { B } }";
        let err = guard
            .check_public_interface(original, tampered)
            .unwrap_err()
            .to_string();
        assert!(err.contains("impl B::new"), "key must be impl-qualified: {err}");
        assert!(!err.contains("impl A::new"), "impl A must not be flagged: {err}");
    }

    #[test]
    fn pub_field_tampering_is_caught() {
        let guard = AstGuardrail::new().unwrap();
        let original = "pub struct Session { pub id: u64, pub secret: String }";
        let tampered = "pub struct Session { pub id: u64 }";
        let err = guard.check_public_interface(original, tampered).unwrap_err().to_string();
        assert!(err.contains("ALTERED"), "field deletion must be caught: {err}");
    }

    #[test]
    fn enum_variant_tampering_is_caught() {
        let guard = AstGuardrail::new().unwrap();
        let original = "pub enum State { Idle, Running { since: u64 } }";
        let tampered = "pub enum State { Idle }";
        assert!(guard.check_public_interface(original, tampered).is_err());
    }

    #[test]
    fn doc_and_attribute_churn_is_not_tampering() {
        let guard = AstGuardrail::new().unwrap();
        let original = "/// docs\npub fn compute(val: u32) -> u32 { val }";
        let with_attr = "/// docs\n#[inline]\npub fn compute(val: u32) -> u32 { val }";
        assert!(guard.check_public_interface(original, with_attr).is_ok());
    }

    #[test]
    fn catches_qualified_macros_and_short_thread_spawn() {
        let guard = AstGuardrail::new().unwrap();
        let std_panic = r#"pub fn boom() { std::panic!("silent"); }"#;
        assert!(guard.check_forbidden(std_panic).is_err(), "std::panic! must be caught");

        let short_spawn = r#"
            pub fn leak() {
                let h = thread::spawn(|| 1);
                let _ = h.join();
            }
        "#;
        assert!(guard.check_forbidden(short_spawn).is_err(), "thread::spawn short form must be caught");
    }
}
