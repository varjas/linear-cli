use anyhow::Result;
use clap::ValueEnum;
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::sync::OnceLock;

use regex::Regex;

use crate::cache::CacheOptions;
use crate::error::CliError;
use crate::json_path::get_path;
use crate::pagination::PaginationOptions;
use crate::OutputFormat;

static QUIET_MODE: OnceLock<bool> = OnceLock::new();

pub fn set_quiet_mode(quiet: bool) {
    let _ = QUIET_MODE.set(quiet);
}

pub fn is_quiet() -> bool {
    QUIET_MODE.get().copied().unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Default, ValueEnum, PartialEq)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct JsonOutputOptions {
    pub compact: bool,
    pub fields: Option<Vec<String>>,
    pub sort: Option<String>,
    pub order: SortOrder,
    pub default_sort: bool,
}

impl JsonOutputOptions {
    pub fn new(
        compact: bool,
        fields: Option<Vec<String>>,
        sort: Option<String>,
        order: SortOrder,
        default_sort: bool,
    ) -> Self {
        Self {
            compact,
            fields,
            sort,
            order,
            default_sort,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputOptions {
    pub format: OutputFormat,
    pub json: JsonOutputOptions,
    pub format_template: Option<String>,
    pub filters: Vec<FilterExpr>,
    pub fail_on_empty: bool,
    pub pagination: PaginationOptions,
    pub cache: CacheOptions,
    pub dry_run: bool,
}

impl OutputOptions {
    pub fn is_json(&self) -> bool {
        matches!(self.format, OutputFormat::Json | OutputFormat::Ndjson)
    }

    pub fn is_ndjson(&self) -> bool {
        self.format == OutputFormat::Ndjson
    }

    pub fn has_template(&self) -> bool {
        self.format_template
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
pub enum FilterOp {
    Eq,
    NotEq,
    Contains,
}

#[derive(Debug, Clone)]
pub struct FilterExpr {
    pub path: Vec<String>,
    pub op: FilterOp,
    pub value: String,
}

pub fn parse_filters(filters: &[String]) -> Result<Vec<FilterExpr>> {
    filters
        .iter()
        .filter(|f| !f.trim().is_empty())
        .map(|f| parse_filter(f))
        .collect()
}

fn parse_filter(input: &str) -> Result<FilterExpr> {
    let trimmed = input.trim();
    let (path, op, value) = if let Some((left, right)) = trimmed.split_once("!=") {
        (left, FilterOp::NotEq, right)
    } else if let Some((left, right)) = trimmed.split_once("~=") {
        (left, FilterOp::Contains, right)
    } else if let Some((left, right)) = trimmed.split_once('=') {
        (left, FilterOp::Eq, right)
    } else {
        anyhow::bail!(
            "Invalid filter '{}'. Use field=value, field!=value, or field~=value",
            input
        );
    };

    let path_parts: Vec<String> = path
        .split('.')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect();

    if path_parts.is_empty() {
        anyhow::bail!("Invalid filter '{}': missing field path", input);
    }

    Ok(FilterExpr {
        path: path_parts,
        op,
        value: value.trim().to_string(),
    })
}

pub fn print_json(value: &Value, output: &OutputOptions) -> Result<()> {
    print_json_owned(value.clone(), output)
}

pub fn print_json_owned(mut out: Value, output: &OutputOptions) -> Result<()> {
    apply_filters(&mut out, &output.filters);
    apply_sort(&mut out, &output.json);
    if let Some(fields) = output.json.fields.as_ref() {
        out = select_fields(&out, fields);
    }

    if output.fail_on_empty {
        if let Value::Array(items) = &out {
            if items.is_empty() {
                return Err(CliError::not_found("No results found").into());
            }
        }
    }

    if let Some(template) = output.format_template.as_deref() {
        return print_template(&out, template);
    }

    if output.is_ndjson() {
        return print_ndjson(&out);
    }

    let text = if output.json.compact {
        serde_json::to_string(&out)?
    } else {
        serde_json::to_string_pretty(&out)?
    };
    println!("{}", text);
    Ok(())
}

fn apply_sort(value: &mut Value, opts: &JsonOutputOptions) {
    let Value::Array(items) = value else { return };

    let sort_key = opts
        .sort
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            if opts.default_sort {
                default_sort_key(items)
            } else {
                None
            }
        });

    let Some(key) = sort_key else { return };

    let mut indexed: Vec<(usize, Value)> = items.drain(..).enumerate().collect();
    indexed.sort_by(|(idx_a, a), (idx_b, b)| {
        let ord = compare_json_field(a, b, &key);
        let ord = match opts.order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        };
        if ord == Ordering::Equal {
            idx_a.cmp(idx_b)
        } else {
            ord
        }
    });
    *items = indexed.into_iter().map(|(_, v)| v).collect();
}

fn apply_filters(value: &mut Value, filters: &[FilterExpr]) {
    if filters.is_empty() {
        return;
    }
    let Value::Array(items) = value else { return };
    let filtered: Vec<Value> = items
        .iter()
        .filter(|item| matches_filters(item, filters))
        .cloned()
        .collect();
    *items = filtered;
}

pub fn filter_values(values: &mut Vec<Value>, filters: &[FilterExpr]) {
    if filters.is_empty() {
        return;
    }
    values.retain(|value| matches_filters(value, filters));
}

fn matches_filters(value: &Value, filters: &[FilterExpr]) -> bool {
    filters.iter().all(|filter| {
        let mut current = value;
        for part in &filter.path {
            match current.get(part.as_str()) {
                Some(next) => current = next,
                None => return matches!(filter.op, FilterOp::NotEq),
            }
        }
        let actual = value_to_string(current).to_lowercase();
        let expected = filter.value.to_lowercase();
        match filter.op {
            FilterOp::Eq => actual == expected,
            FilterOp::NotEq => actual != expected,
            FilterOp::Contains => actual.contains(&expected),
        }
    })
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn template_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\{\{\s*\.?([a-zA-Z0-9_.-]*)\s*\}\}").unwrap())
}

pub fn print_template(value: &Value, template: &str) -> Result<()> {
    match value {
        Value::Array(items) => {
            for item in items {
                println!("{}", render_template(template, item));
            }
        }
        _ => {
            println!("{}", render_template(template, value));
        }
    }
    Ok(())
}

pub fn ensure_non_empty(values: &[Value], output: &OutputOptions) -> Result<()> {
    if output.fail_on_empty && values.is_empty() {
        return Err(CliError::not_found("No results found").into());
    }
    Ok(())
}

fn render_template(template: &str, value: &Value) -> String {
    template_regex()
        .replace_all(template, |caps: &regex::Captures| {
            let path = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if path.is_empty() || path == "." {
                return value_to_string(value);
            }
            let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
            match get_path(value, &parts) {
                Some(found) => value_to_string(found),
                None => String::new(),
            }
        })
        .to_string()
}

fn print_ndjson(value: &Value) -> Result<()> {
    match value {
        Value::Array(items) => {
            for item in items {
                println!("{}", serde_json::to_string(item)?);
            }
        }
        _ => {
            println!("{}", serde_json::to_string(value)?);
        }
    }
    Ok(())
}

fn default_sort_key(items: &[Value]) -> Option<String> {
    if items.iter().any(|v| has_object_key(v, "identifier")) {
        return Some("identifier".to_string());
    }
    if items.iter().any(|v| has_object_key(v, "id")) {
        return Some("id".to_string());
    }
    None
}

fn has_object_key(value: &Value, key: &str) -> bool {
    value.as_object().and_then(|obj| obj.get(key)).is_some()
}

fn compare_json_field(a: &Value, b: &Value, key: &str) -> Ordering {
    let a_key = extract_sort_key(a, key);
    let b_key = extract_sort_key(b, key);
    a_key.partial_cmp(&b_key).unwrap_or(Ordering::Equal)
}

#[derive(Debug, PartialEq)]
enum SortKey {
    Int(i64),
    Float(f64),
    DateTime(i64),
    String(String),
    Null,
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (SortKey::Int(a), SortKey::Int(b)) => a.partial_cmp(b),
            (SortKey::Float(a), SortKey::Float(b)) => a.partial_cmp(b),
            (SortKey::DateTime(a), SortKey::DateTime(b)) => a.partial_cmp(b),
            (SortKey::String(a), SortKey::String(b)) => a.partial_cmp(b),
            (SortKey::Null, SortKey::Null) => Some(Ordering::Equal),
            // Nulls sort last
            (SortKey::Null, _) => Some(Ordering::Greater),
            (_, SortKey::Null) => Some(Ordering::Less),
            // Mixed numeric types: convert to float for comparison
            (SortKey::Int(a), SortKey::Float(b)) => (*a as f64).partial_cmp(b),
            (SortKey::Float(a), SortKey::Int(b)) => a.partial_cmp(&(*b as f64)),
            // Different types: fall back to string comparison
            _ => {
                let a_str = self.to_string_for_cmp();
                let b_str = other.to_string_for_cmp();
                a_str.partial_cmp(&b_str)
            }
        }
    }
}

impl SortKey {
    fn to_string_for_cmp(&self) -> String {
        match self {
            SortKey::Int(n) => n.to_string(),
            SortKey::Float(n) => n.to_string(),
            SortKey::DateTime(ts) => ts.to_string(),
            SortKey::String(s) => s.clone(),
            SortKey::Null => String::new(),
        }
    }
}

fn extract_sort_key(value: &Value, key: &str) -> SortKey {
    // Support nested paths like "state.name"
    let v = if key.contains('.') {
        let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
        get_path(value, &parts).cloned()
    } else {
        value.get(key).cloned()
    };

    let v = match v {
        Some(v) => v,
        None => return SortKey::Null,
    };

    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SortKey::Int(i)
            } else if let Some(f) = n.as_f64() {
                SortKey::Float(f)
            } else {
                SortKey::String(n.to_string())
            }
        }
        Value::String(s) => {
            // Try parsing as RFC3339 date
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&s) {
                SortKey::DateTime(dt.timestamp())
            } else {
                SortKey::String(s.to_lowercase())
            }
        }
        Value::Bool(b) => SortKey::String(b.to_string()),
        Value::Null => SortKey::Null,
        other => SortKey::String(other.to_string()),
    }
}

pub fn sort_values(values: &mut Vec<Value>, key: &str, order: SortOrder) {
    let mut indexed: Vec<(usize, Value)> = values.drain(..).enumerate().collect();
    indexed.sort_by(|(idx_a, a), (idx_b, b)| {
        let ord = compare_json_field(a, b, key);
        let ord = match order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        };
        if ord == Ordering::Equal {
            idx_a.cmp(idx_b)
        } else {
            ord
        }
    });
    *values = indexed.into_iter().map(|(_, v)| v).collect();
}

fn select_fields(value: &Value, fields: &[String]) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| select_fields(v, fields)).collect())
        }
        Value::Object(_) => {
            let mut out = Map::new();
            for path in fields {
                let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
                if parts.is_empty() {
                    continue;
                }
                if let Some(field_value) = get_path(value, &parts) {
                    set_path(&mut out, &parts, field_value.clone());
                }
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn set_path(out: &mut Map<String, Value>, parts: &[&str], value: Value) {
    if parts.is_empty() {
        return;
    }
    if parts.len() == 1 {
        out.insert(parts[0].to_string(), value);
        return;
    }
    let entry = out
        .entry(parts[0].to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(ref mut map) = entry {
        set_path(map, &parts[1..], value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_filter_eq() {
        let filters = parse_filters(&["status=Done".to_string()]).unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].path, vec!["status"]);
        assert!(matches!(filters[0].op, FilterOp::Eq));
        assert_eq!(filters[0].value, "Done");
    }

    #[test]
    fn test_parse_filter_not_eq() {
        let filters = parse_filters(&["priority!=1".to_string()]).unwrap();
        assert_eq!(filters.len(), 1);
        assert!(matches!(filters[0].op, FilterOp::NotEq));
        assert_eq!(filters[0].value, "1");
    }

    #[test]
    fn test_parse_filter_contains() {
        let filters = parse_filters(&["title~=bug".to_string()]).unwrap();
        assert_eq!(filters.len(), 1);
        assert!(matches!(filters[0].op, FilterOp::Contains));
        assert_eq!(filters[0].value, "bug");
    }

    #[test]
    fn test_parse_filter_nested_path() {
        let filters = parse_filters(&["state.name=In Progress".to_string()]).unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].path, vec!["state", "name"]);
        assert_eq!(filters[0].value, "In Progress");
    }

    #[test]
    fn test_parse_filter_multiple() {
        let filters =
            parse_filters(&["status=Done".to_string(), "priority!=1".to_string()]).unwrap();
        assert_eq!(filters.len(), 2);
    }

    #[test]
    fn test_parse_filter_empty_skipped() {
        let filters = parse_filters(&["".to_string(), "  ".to_string()]).unwrap();
        assert!(filters.is_empty());
    }

    #[test]
    fn test_parse_filter_invalid() {
        let result = parse_filters(&["invalid-filter".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filter_missing_path() {
        let result = parse_filters(&["=value".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_sort_order_default() {
        assert_eq!(SortOrder::default(), SortOrder::Asc);
    }

    #[test]
    fn test_sort_nested_path() {
        let mut values = vec![
            json!({"name": "Charlie", "state": {"name": "Done"}}),
            json!({"name": "Alice", "state": {"name": "Backlog"}}),
            json!({"name": "Bob", "state": {"name": "In Progress"}}),
        ];
        sort_values(&mut values, "state.name", SortOrder::Asc);
        assert_eq!(values[0]["state"]["name"], "Backlog");
        assert_eq!(values[1]["state"]["name"], "Done");
        assert_eq!(values[2]["state"]["name"], "In Progress");
    }

    #[test]
    fn test_sort_top_level_field() {
        let mut values = vec![
            json!({"priority": 3}),
            json!({"priority": 1}),
            json!({"priority": 2}),
        ];
        sort_values(&mut values, "priority", SortOrder::Asc);
        assert_eq!(values[0]["priority"], 1);
        assert_eq!(values[1]["priority"], 2);
        assert_eq!(values[2]["priority"], 3);
    }

    #[test]
    fn test_sort_desc_order() {
        let mut values = vec![
            json!({"priority": 1}),
            json!({"priority": 3}),
            json!({"priority": 2}),
        ];
        sort_values(&mut values, "priority", SortOrder::Desc);
        assert_eq!(values[0]["priority"], 3);
        assert_eq!(values[1]["priority"], 2);
        assert_eq!(values[2]["priority"], 1);
    }

    #[test]
    fn test_sort_string_field() {
        let mut values = vec![
            json!({"name": "Charlie"}),
            json!({"name": "Alice"}),
            json!({"name": "Bob"}),
        ];
        sort_values(&mut values, "name", SortOrder::Asc);
        assert_eq!(values[0]["name"], "Alice");
        assert_eq!(values[1]["name"], "Bob");
        assert_eq!(values[2]["name"], "Charlie");
    }

    #[test]
    fn test_sort_with_nulls_last() {
        let mut values = vec![
            json!({"priority": null}),
            json!({"priority": 1}),
            json!({"priority": 2}),
        ];
        sort_values(&mut values, "priority", SortOrder::Asc);
        assert_eq!(values[0]["priority"], 1);
        assert_eq!(values[1]["priority"], 2);
        assert!(values[2]["priority"].is_null());
    }

    #[test]
    fn test_sort_missing_field() {
        let mut values = vec![
            json!({"name": "Bob"}),
            json!({"name": "Alice", "priority": 1}),
            json!({"name": "Charlie"}),
        ];
        sort_values(&mut values, "priority", SortOrder::Asc);
        // Item with priority should be first, rest are null/missing
        assert_eq!(values[0]["name"], "Alice");
    }

    #[test]
    fn test_sort_preserves_order_for_equal_values() {
        let mut values = vec![
            json!({"name": "First", "priority": 1}),
            json!({"name": "Second", "priority": 1}),
            json!({"name": "Third", "priority": 1}),
        ];
        sort_values(&mut values, "priority", SortOrder::Asc);
        // Stable sort: original order preserved for equal values
        assert_eq!(values[0]["name"], "First");
        assert_eq!(values[1]["name"], "Second");
        assert_eq!(values[2]["name"], "Third");
    }

    #[test]
    fn test_filter_values_eq() {
        let mut values = vec![
            json!({"status": "Done", "name": "A"}),
            json!({"status": "In Progress", "name": "B"}),
            json!({"status": "Done", "name": "C"}),
        ];
        let filters = parse_filters(&["status=Done".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["name"], "A");
        assert_eq!(values[1]["name"], "C");
    }

    #[test]
    fn test_filter_values_not_eq() {
        let mut values = vec![
            json!({"priority": 1, "name": "A"}),
            json!({"priority": 2, "name": "B"}),
            json!({"priority": 1, "name": "C"}),
        ];
        let filters = parse_filters(&["priority!=1".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["name"], "B");
    }

    #[test]
    fn test_filter_values_contains() {
        let mut values = vec![
            json!({"title": "Fix login bug", "id": 1}),
            json!({"title": "Add feature", "id": 2}),
            json!({"title": "Bug in signup", "id": 3}),
        ];
        let filters = parse_filters(&["title~=bug".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["id"], 1);
        assert_eq!(values[1]["id"], 3);
    }

    #[test]
    fn test_filter_values_nested_path() {
        let mut values = vec![
            json!({"name": "A", "state": {"name": "Done"}}),
            json!({"name": "B", "state": {"name": "Backlog"}}),
            json!({"name": "C", "state": {"name": "Done"}}),
        ];
        let filters = parse_filters(&["state.name=Done".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["name"], "A");
        assert_eq!(values[1]["name"], "C");
    }

    #[test]
    fn test_filter_case_insensitive() {
        let mut values = vec![
            json!({"status": "DONE"}),
            json!({"status": "done"}),
            json!({"status": "Done"}),
        ];
        let filters = parse_filters(&["status=done".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn test_filter_multiple_conditions() {
        let mut values = vec![
            json!({"status": "Done", "priority": 1}),
            json!({"status": "Done", "priority": 2}),
            json!({"status": "Backlog", "priority": 1}),
        ];
        let filters =
            parse_filters(&["status=Done".to_string(), "priority=1".to_string()]).unwrap();
        filter_values(&mut values, &filters);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["priority"], 1);
        assert_eq!(values[0]["status"], "Done");
    }

    #[test]
    fn test_filter_no_filters_is_noop() {
        let mut values = vec![json!({"a": 1}), json!({"a": 2})];
        filter_values(&mut values, &[]);
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn test_select_fields_single() {
        let value = json!({"id": "abc", "name": "Test", "description": "Long desc"});
        let fields = vec!["name".to_string()];
        let result = select_fields(&value, &fields);
        assert_eq!(result, json!({"name": "Test"}));
    }

    #[test]
    fn test_select_fields_nested() {
        let value = json!({"id": "abc", "state": {"name": "Done", "id": "s1"}});
        let fields = vec!["id".to_string(), "state.name".to_string()];
        let result = select_fields(&value, &fields);
        assert_eq!(result, json!({"id": "abc", "state": {"name": "Done"}}));
    }

    #[test]
    fn test_select_fields_array() {
        let value = json!([
            {"id": "1", "name": "A", "extra": true},
            {"id": "2", "name": "B", "extra": false},
        ]);
        let fields = vec!["id".to_string(), "name".to_string()];
        let result = select_fields(&value, &fields);
        assert_eq!(
            result,
            json!([
                {"id": "1", "name": "A"},
                {"id": "2", "name": "B"},
            ])
        );
    }

    #[test]
    fn test_select_fields_missing_field() {
        let value = json!({"id": "abc", "name": "Test"});
        let fields = vec!["id".to_string(), "nonexistent".to_string()];
        let result = select_fields(&value, &fields);
        assert_eq!(result, json!({"id": "abc"}));
    }

    #[test]
    fn test_render_template_simple() {
        let value = json!({"name": "Test", "id": "abc"});
        let result = render_template("{{name}} ({{id}})", &value);
        assert_eq!(result, "Test (abc)");
    }

    #[test]
    fn test_render_template_nested() {
        let value = json!({"name": "Test", "state": {"name": "Done"}});
        let result = render_template("{{name}}: {{state.name}}", &value);
        assert_eq!(result, "Test: Done");
    }

    #[test]
    fn test_render_template_missing_field() {
        let value = json!({"name": "Test"});
        let result = render_template("{{name}} {{missing}}", &value);
        assert_eq!(result, "Test ");
    }

    #[test]
    fn test_render_template_with_spaces() {
        let value = json!({"name": "Test"});
        let result = render_template("{{ name }}", &value);
        assert_eq!(result, "Test");
    }
}
