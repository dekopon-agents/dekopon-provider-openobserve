//! Fitting a result under the grant's output ceiling.
//!
//! `maxOutputBytes` is a refusal in the host, not a truncation, and the provider refusing on its
//! own would be worse: a model that asked for 500 rows and got an error learns nothing about how
//! many there were. So the rows are dropped from the tail until the serialized envelope fits, and
//! the envelope says so — `truncated: true` with `omittedRows: N`. Rows are never dropped silently
//! and the count is never a guess.
//!
//! The host's `OutputTooLarge` stays the backstop. This is a courtesy, not a security bound: a
//! ceiling enforced by the thing being bounded is not a bound.

use serde_json::{Map, Value};

/// The ceiling this provider fits to when the owner's is unknown.
///
/// A component cannot read its own constraint set — `ExecutionConstraints` is owner-side and a
/// query-window or output-size key on it would be tree growth for this provider's sake. So the
/// default matches the narrowest `maxOutputBytes` #250's example constraint sets write (64 KiB for
/// the stats words) and every word takes `--max-output-bytes` to raise it as far as the owner's
/// own ceiling, which the host enforces regardless.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The largest ceiling a caller may name, matching #250's widest example constraint set.
pub const MAX_OUTPUT_BYTES_CEILING: usize = 1024 * 1024;

/// The smallest ceiling worth honouring: below this even an empty envelope will not fit.
pub const MIN_OUTPUT_BYTES: usize = 1024;

/// Builds `{"rows": [...], "returned": n, "total": t, "truncated": bool, "omittedRows": n}` fitted
/// under `max_bytes`, dropping rows from the tail.
///
/// `total` is the store's own count of matching rows, which is why a truncated answer still reports
/// an exact population: `omittedRows` counts what this envelope left out of what the store sent,
/// and `total` says how many the store had.
#[must_use]
pub fn fit_rows(mut rows: Vec<Value>, total: u64, max_bytes: usize) -> Value {
    let fetched = rows.len();
    loop {
        let candidate = envelope(&rows, fetched, total);
        if serialized_len(&candidate) <= max_bytes || rows.is_empty() {
            return candidate;
        }
        rows.pop();
    }
}

/// Fits an object-shaped result by dropping one named list from the tail until it fits.
///
/// The stats shapes are objects, not row lists, and their only unbounded member is a map — the
/// per-capability call counts, the per-reason denial counts. Everything else is a fixed set of
/// scalars, so trimming the map is the whole story.
#[must_use]
pub fn fit_object(mut object: Map<String, Value>, trimmable: &str) -> Value {
    let ceiling = object
        .get("maxOutputBytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
    object.remove("maxOutputBytes");
    let mut omitted = 0_u64;
    loop {
        let mut candidate = object.clone();
        candidate.insert("truncated".to_owned(), Value::Bool(omitted > 0));
        if omitted > 0 {
            candidate.insert("omittedRows".to_owned(), Value::from(omitted));
        }
        let value = Value::Object(candidate);
        if serialized_len(&value) <= ceiling {
            return value;
        }
        let Some(entries) = object.get_mut(trimmable).and_then(Value::as_object_mut) else {
            return value;
        };
        let Some(last) = entries.keys().next_back().cloned() else {
            return value;
        };
        entries.remove(&last);
        omitted += 1;
    }
}

/// Serialized byte length, measured rather than estimated.
#[must_use]
pub fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn envelope(rows: &[Value], fetched: usize, total: u64) -> Value {
    let omitted = fetched.saturating_sub(rows.len());
    serde_json::json!({
        "rows": rows,
        "returned": rows.len(),
        "total": total,
        "truncated": omitted > 0 || u64::try_from(fetched).unwrap_or(u64::MAX) < total,
        "omittedRows": u64::try_from(omitted).unwrap_or(u64::MAX)
            + total.saturating_sub(u64::try_from(fetched).unwrap_or(u64::MAX)),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{fit_object, fit_rows, serialized_len};

    fn rows(count: usize) -> Vec<Value> {
        (0..count)
            .map(|index| json!({"traceId": format!("{index:032x}"), "durationMs": index}))
            .collect()
    }

    /// Nothing is dropped when everything fits, and `total` is the store's count, not the row count.
    #[test]
    fn a_result_that_fits_is_returned_whole() {
        let fitted = fit_rows(rows(3), 3, 64 * 1024);
        assert_eq!(fitted["returned"], 3);
        assert_eq!(fitted["total"], 3);
        assert_eq!(fitted["truncated"], false);
        assert_eq!(fitted["omittedRows"], 0);
    }

    /// The store having more than the provider asked for is itself truncation, and it is reported
    /// even when every fetched row fit.
    #[test]
    fn a_store_with_more_rows_than_the_limit_is_reported_as_truncated() {
        let fitted = fit_rows(rows(5), 1_873, 64 * 1024);
        assert_eq!(fitted["returned"], 5);
        assert_eq!(fitted["total"], 1_873);
        assert_eq!(fitted["truncated"], true);
        assert_eq!(fitted["omittedRows"], 1_868);
    }

    /// Rows come off the tail until the envelope fits, and the count is exact, never an estimate.
    #[test]
    fn rows_are_dropped_from_the_tail_until_the_envelope_fits() {
        let fitted = fit_rows(rows(200), 200, 2_048);
        let returned = fitted["returned"].as_u64().expect("a count");
        assert!(returned > 0 && returned < 200, "{returned}");
        assert_eq!(fitted["truncated"], true);
        assert_eq!(fitted["omittedRows"], 200 - returned);
        assert!(serialized_len(&fitted) <= 2_048);
        assert_eq!(
            fitted["rows"].as_array().expect("rows").len() as u64,
            returned
        );
    }

    /// An object-shaped stat trims its one unbounded map, and says how many entries it dropped.
    #[test]
    fn an_object_shape_trims_its_unbounded_map() {
        let mut capabilities = Map::new();
        for index in 0..200 {
            capabilities.insert(format!("provider.capability-{index:03}"), json!(index));
        }
        let mut object = Map::new();
        object.insert("agent".to_owned(), json!("reviewer"));
        object.insert("capabilities".to_owned(), Value::Object(capabilities));
        object.insert("maxOutputBytes".to_owned(), json!(2_048));

        let fitted = fit_object(object, "capabilities");
        assert_eq!(fitted["truncated"], true);
        assert!(fitted["omittedRows"].as_u64().expect("a count") > 0);
        assert!(serialized_len(&fitted) <= 2_048);
        assert_eq!(fitted["agent"], "reviewer");
        assert!(
            fitted.get("maxOutputBytes").is_none(),
            "the knob is internal"
        );
    }
}
