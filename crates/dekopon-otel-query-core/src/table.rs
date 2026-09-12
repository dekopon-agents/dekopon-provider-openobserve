//! `--format table`: the same answer, rendered for a model to read instead of to pipe.
//!
//! Two output formats, and the default is the composable one. The shell's `jq` builtin takes the
//! piped command's **value**, not its text
//! (`dekopon-shell/src/builtins/jq.rs`: `run(&self, …, input: Option<Value>)`, handed straight to
//! `evaluate`), so a capability that returns one JSON object is already pipeable with no wrapper:
//!
//! ```text
//! openobserve sql --url … --since 1h 'SELECT …' | jq '.rows[] | .duration_ms'
//! ```
//!
//! That is why the default is one object with a `rows` array rather than JSON Lines. JSON Lines
//! would have to be reassembled by whoever consumed it, and the one consumer that matters here
//! parses nothing — it receives the value.
//!
//! **Key spelling, which is the thing that makes a filter keep working.** Row keys are the store's
//! own folded column names, passed through untouched: `duration_ms`, `capability_id`,
//! `usage_input_tokens`, `operation_name`. Renaming them would break the filter a model wrote
//! against a `SELECT` it typed itself. Envelope and statistic keys are #250's contract —
//! `rows`, `returned`, `total`, `truncated`, `omittedRows`, `turns.durationMs.p95` — and are
//! camelCase because that issue specifies them byte for byte as the shape every backend must
//! produce. The two conventions meet at the envelope boundary and neither moves.
//!
//! The table is a string, and the shell emits a string result verbatim, so `--format table` prints
//! as a table rather than as a quoted JSON scalar.
//!
//! Column order is the order the keys come out of the parsed rows, which `serde_json`'s default map
//! makes alphabetical. Stable across runs is the property that matters: a model reading two answers
//! to the same question should not have to re-find a column.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Widest a single cell renders before it is elided.
const MAX_CELL: usize = 48;

/// Renders one answer as a fixed-width text table.
///
/// Three shapes reach this: an envelope with `rows`, the `broker providers` envelope with
/// `providers`, and the `agent stats` object. The first two become a column table with a footer
/// line carrying the truncation marker; the third becomes a two-column table of flattened paths,
/// with the same marker as its last row.
#[must_use]
pub fn render(value: &Value) -> String {
    for key in ["rows", "providers"] {
        if let Some(Value::Array(rows)) = value.get(key) {
            return rows_table(rows, value);
        }
    }
    object_table(value)
}

fn rows_table(rows: &[Value], envelope: &Value) -> String {
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        if let Some(object) = row.as_object() {
            for key in object.keys() {
                if !columns.iter().any(|column| column == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    if columns.is_empty() {
        return format!("{}\n", footer(envelope, rows.len()));
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| cell(row.get(column).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            cells
                .iter()
                .map(|row| row[index].chars().count())
                .chain(std::iter::once(column.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    push_row(&mut out, &columns, &widths);
    push_row(
        &mut out,
        &widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>(),
        &widths,
    );
    for row in &cells {
        push_row(&mut out, row, &widths);
    }
    out.push_str(&footer(envelope, rows.len()));
    out.push('\n');
    out
}

fn object_table(value: &Value) -> String {
    let mut flat = BTreeMap::new();
    flatten(String::new(), value, &mut flat);
    let key_width = flat
        .keys()
        .map(|key| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (key, rendered) in &flat {
        out.push_str(&format!("{key:<key_width$}  {rendered}\n"));
    }
    out
}

fn flatten(prefix: String, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(entries) if !entries.is_empty() => {
            for (key, nested) in entries {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(path, nested, out);
            }
        }
        other => {
            out.insert(
                if prefix.is_empty() {
                    "value".to_owned()
                } else {
                    prefix
                },
                cell(other),
            );
        }
    }
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let mut line = String::new();
    for (index, text) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        let width = widths.get(index).copied().unwrap_or(0);
        if index + 1 == cells.len() {
            line.push_str(text);
        } else {
            line.push_str(&format!("{text:<width$}"));
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

/// The truncation marker, which every format carries because dropping rows silently is the one
/// thing this provider must never do.
fn footer(envelope: &Value, shown: usize) -> String {
    let total = envelope.get("total").and_then(Value::as_u64);
    let omitted = envelope
        .get("omittedRows")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let truncated = envelope
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut line = match total {
        Some(total) => format!("-- {shown} of {total} rows"),
        None => format!("-- {shown} rows"),
    };
    if truncated {
        line.push_str(&format!("; truncated, {omitted} omitted"));
    }
    line
}

fn cell(value: &Value) -> String {
    let rendered = match value {
        Value::Null => "-".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let rendered = rendered.replace(['\n', '\t'], " ");
    if rendered.chars().count() > MAX_CELL {
        let mut elided: String = rendered.chars().take(MAX_CELL - 1).collect();
        elided.push('…');
        return elided;
    }
    rendered
}

/// Adds the truncation marker to a rows envelope whose table is empty, so an empty answer still
/// says whether it was empty because nothing matched or because everything was dropped.
#[must_use]
pub fn empty_marker(envelope: &Map<String, Value>) -> String {
    footer(&Value::Object(envelope.clone()), 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render;

    #[test]
    fn a_rows_envelope_becomes_a_fixed_width_table_with_a_footer() {
        let table = render(&json!({
            "rows": [
                {"trace_id": "0af7651916cd43dd8448eb211c80319c", "operation_name": "gateway.session", "duration_ms": 41},
                {"trace_id": "1bf7651916cd43dd8448eb211c80319d", "operation_name": "prompt.model_turn", "duration_ms": 2140}
            ],
            "returned": 2,
            "total": 2,
            "truncated": false,
            "omittedRows": 0
        }));
        let lines: Vec<&str> = table.lines().collect();
        // Alphabetical, which is what the parsed object's key order gives and is stable per run.
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["duration_ms", "operation_name", "trace_id"]
        );
        assert!(lines[1].starts_with("---"), "{table}");
        assert!(lines[2].contains("gateway.session"), "{table}");
        // Columns line up: both rows start their second column at the same offset, and it is the
        // offset the header uses too.
        assert_eq!(
            lines[2].find("gateway.session"),
            lines[3].find("prompt.model_turn")
        );
        assert_eq!(
            lines[0].find("operation_name"),
            lines[2].find("gateway.session")
        );
        assert_eq!(lines.last().copied(), Some("-- 2 of 2 rows"));
    }

    /// The truncation marker is in both formats. In the table it is the footer.
    #[test]
    fn the_table_footer_carries_the_truncation_marker() {
        let table = render(&json!({
            "rows": [{"trace_id": "0af7651916cd43dd8448eb211c80319c"}],
            "returned": 1,
            "total": 1873,
            "truncated": true,
            "omittedRows": 1872
        }));
        assert_eq!(
            table.lines().last(),
            Some("-- 1 of 1873 rows; truncated, 1872 omitted")
        );
    }

    #[test]
    fn an_empty_result_still_says_how_many_the_store_had() {
        let table = render(&json!({
            "rows": [],
            "returned": 0,
            "total": 0,
            "truncated": false,
            "omittedRows": 0
        }));
        assert_eq!(table.trim_end(), "-- 0 of 0 rows");
    }

    #[test]
    fn an_object_shape_becomes_a_two_column_table_of_flattened_paths() {
        let table = render(&json!({
            "agent": "reviewer",
            "turns": {"count": 41, "durationMs": {"p50": 2140, "p95": 9800}},
            "capabilities": {"gh.pull-request.read": 17},
            "truncated": true
        }));
        assert!(table.contains("agent"), "{table}");
        assert!(table.contains("turns.durationMs.p95"), "{table}");
        assert!(
            table.contains("capabilities.gh.pull-request.read"),
            "{table}"
        );
        assert!(table.contains("truncated"), "{table}");
        // Values line up in one column.
        let offsets: Vec<usize> = table
            .lines()
            .map(|line| line.rfind("  ").expect("a gap"))
            .collect();
        assert!(offsets.windows(2).all(|pair| pair[0] == pair[1]), "{table}");
    }

    /// A cell that carries a long string or a newline cannot break the table's shape.
    #[test]
    fn a_wide_or_multiline_cell_is_elided_onto_one_line() {
        let table = render(&json!({
            "rows": [{"note": format!("{}\nsecond line", "x".repeat(200))}],
            "total": 1,
            "truncated": false,
            "omittedRows": 0
        }));
        assert_eq!(table.lines().count(), 4, "{table}");
        assert!(table.contains('…'), "{table}");
    }
}
