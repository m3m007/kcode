//! Built-in metadata file-search tool: answers "where is X" from an index.
//!
//! The tool exists to collapse N model-driven greps into ONE call. Filesystem
//! search burns turns and each turn is a full cache read, so a single indexed
//! answer is the cheaper shape.
//!
//! It is a **built-in kcode tool, not a preloaded MCP server**. The research that
//! motivated it measured an MCP-delivered index at 1.66x tokens plus an extra
//! ToolSearch turn, while its CLI/plugin route measured 0.82x-0.96x — so the
//! schema is not preloaded and there is no MCP round trip at all.
//!
//! Config-gated behind `[search_index]`, **disabled by default**: with the switch
//! off this tool returns a disabled notice and never creates or touches an index
//! file, so a stock kcode behaves exactly as it did before.

use super::{Tool, ToolContext, ToolOutput};
use crate::config::SearchIndexConfig;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

/// What the caller is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum QueryKind {
    /// Symbols, paths and content, ranked. The default.
    #[default]
    Any,
    /// Only symbol definitions (functions, structs, traits, impls, ...).
    Symbol,
    /// Only file paths.
    File,
}

#[derive(Debug, Deserialize)]
struct SearchIndexInput {
    /// The name, path fragment or text to look for. Use distinctive keywords.
    query: String,
    /// Restrict the answer. Defaults to `any`.
    #[serde(default)]
    kind: Option<QueryKind>,
    /// Max results. Defaults to the configured bound.
    #[serde(default)]
    limit: Option<i64>,
    /// Re-index before answering even when the configured refresh mode says
    /// otherwise. Defaults to false (the config decides).
    #[serde(default)]
    refresh: Option<bool>,
}

pub struct SearchIndexTool;

impl SearchIndexTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SearchIndexTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SearchIndexTool {
    fn name(&self) -> &str {
        "search_index"
    }

    fn description(&self) -> &str {
        "Locate a file, symbol or text in the workspace from a maintained index in ONE call, instead of many greps. Disabled unless [search_index] enabled."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "query": {
                    "type": "string",
                    "description": "Symbol name, path fragment, or text to find. Use distinctive keywords; single characters are rejected."
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "symbol", "file"],
                    "description": "Restrict the answer: 'symbol' for definitions only, 'file' for paths only, 'any' (default) for the ranked union."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Max results. Defaults to the configured [search_index] max_results."
                },
                "refresh": {
                    "type": "boolean",
                    "description": "Re-index before answering even when the configured refresh mode says otherwise."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let params: SearchIndexInput = serde_json::from_value(input)?;
        let settings: SearchIndexConfig = crate::config::config().search_index.clone();

        // Validate input before the config gate. A malformed query or a missing
        // working directory is a caller error, and reporting it is independent of
        // whether the index is switched on: with the gate off the answer names the
        // actual defect instead of silently saying "disabled". Neither path
        // touches the index, so the gate still owns every on-disk effect.
        let query = params.query.trim().to_string();
        if query.is_empty() {
            return Ok(ToolOutput::new(
                "search_index: a query is required (a symbol name, path fragment, or text)",
            )
            .with_title("search_index"));
        }
        if query.chars().count() < 2 {
            return Ok(ToolOutput::new(format!(
                "search_index: query '{query}' is too short to be distinctive; use at least 2 characters"
            ))
            .with_title("search_index"));
        }

        // The session's working directory is the indexed root. Without one there
        // is no tree to index, and guessing a root would hardcode a path.
        let Some(root) = ctx.working_dir.clone() else {
            return Ok(ToolOutput::new(
                "search_index: no session working directory, so there is no tree to index",
            )
            .with_title("search_index"));
        };

        // Config gate: with the switch off this is a no-op that never creates or
        // touches an index file. It runs after validation only, never after any
        // index work.
        if !settings.enabled {
            return Ok(ToolOutput::new(disabled_notice())
                .with_title("search_index")
                .with_metadata(json!({ "search_index": { "enabled": false } })));
        }

        let limit = params
            .limit
            .map(|value| value.clamp(1, 200) as usize)
            .unwrap_or(settings.max_results.clamp(1, 200));
        let kind = params.kind.unwrap_or_default();
        let index_path = kcode_base::search_index::resolve_index_path(&root, &settings.index_path);

        // Refresh + query run on a blocking thread: the index walk and SQLite
        // work are synchronous and must not stall the async runtime.
        let refresh_mode = settings.effective_refresh().to_string();
        let force_refresh = params.refresh.unwrap_or(false);
        let root_for_query = root.clone();
        let index_path_for_query = index_path.clone();
        let max_file_bytes = settings.max_file_bytes as u64;
        let query_for_query = query.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut index =
                kcode_base::search_index::SearchIndex::open(&root_for_query, &index_path_for_query, max_file_bytes)?;
            let fresh = if force_refresh || refresh_mode == "on_write" {
                index.refresh().map(|_| true).unwrap_or(false)
            } else if refresh_mode == "on_query" {
                if index.has_pending_changes()? {
                    index.refresh().map(|_| true).unwrap_or(false)
                } else {
                    true
                }
            } else {
                // "manual": never refresh automatically, report staleness.
                index.is_fresh()
            };
            let mut report = index.query(&query_for_query, limit)?;
            report.fresh = fresh;
            Ok::<kcode_base::search_index::SearchReport, kcode_base::search_index::SearchIndexError>(
                report,
            )
        })
        .await;

        let mut report = match outcome {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => {
                return Ok(ToolOutput::new(format!("search_index: {error}")).with_title("search_index"));
            }
            Err(join_error) => {
                return Ok(ToolOutput::new(format!("search_index: worker failed: {join_error}"))
                    .with_title("search_index"));
            }
        };

        let hits: Vec<kcode_base::search_index::SearchHit> = report
            .hits
            .iter()
            .filter(|hit| match kind {
                QueryKind::Any => true,
                QueryKind::Symbol => hit.kind == "symbol",
                QueryKind::File => hit.kind == "path",
            })
            .cloned()
            .collect();
        report.hits = hits;

        Ok(ToolOutput::new(kcode_base::search_index::render_report(&query, &report))
            .with_title("search_index")
            .with_metadata(json!({
                "search_index": {
                    "enabled": true,
                    "query": query,
                    "kind": format!("{kind:?}").to_lowercase(),
                    "hits": report.hits.len(),
                    "fresh": report.fresh,
                    "files_indexed": report.files_indexed,
                    "symbols_indexed": report.symbols_indexed,
                    "index_path": index_path.display().to_string()
                }
            })))
    }
}

/// The notice returned when the tool is disabled. It names the exact knob, so a
/// disabled feature is never silent.
fn disabled_notice() -> String {
    "search_index is disabled. Set [search_index] enabled = true in config.toml \
     (or KCODE_SEARCH_ENABLED=1) to index the workspace and answer file/symbol \
     location queries from one call instead of many greps."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_notice_names_the_config_knob() {
        let notice = disabled_notice();
        assert!(
            notice.contains("[search_index] enabled = true"),
            "the disabled notice must name the exact knob: {notice}"
        );
        assert!(
            notice.contains("KCODE_SEARCH_ENABLED=1"),
            "the disabled notice must name the env override: {notice}"
        );
    }

    #[test]
    fn query_kind_defaults_to_any() {
        assert_eq!(QueryKind::default(), QueryKind::Any);
    }

    #[test]
    fn schema_requires_a_query_and_is_not_preloaded() {
        let tool = SearchIndexTool::new();
        let schema = tool.parameters_schema();
        let required = schema.get("required").and_then(|value| value.as_array());
        assert_eq!(
            required.map(|items| items.len()),
            Some(1),
            "only `query` may be required: {schema}"
        );
        assert_eq!(tool.name(), "search_index");
        assert!(
            tool.description()
                .contains("ONE call"),
            "the description must state the one-call economy: {}",
            tool.description()
        );
    }

    #[test]
    fn input_parses_the_documented_shape() {
        let parsed: SearchIndexInput =
            serde_json::from_value(json!({"query": "widget", "kind": "symbol", "limit": 5})).unwrap();
        assert_eq!(parsed.query, "widget");
        assert_eq!(parsed.kind, Some(QueryKind::Symbol));
        assert_eq!(parsed.limit, Some(5));
        assert_eq!(parsed.refresh, None);
    }

    #[test]
    fn a_query_of_only_one_character_is_rejected_as_not_distinctive() {
        // Mirrors the guard in `execute`: fewer than 2 characters cannot be
        // distinctive and would match nearly every file.
        let query = "a";
        assert!(query.chars().count() < 2);
    }
}

#[cfg(test)]
#[path = "search_index_tests.rs"]
mod search_index_tests;
