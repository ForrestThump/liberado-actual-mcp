//! Bounded payee listing.
//!
//! `GET /api/v1/payees` accepts `search`, `prefix`, `exclude_empty`, `compact`,
//! `limit`, and `offset` (ForrestThump/liberado-budget#44) and echoes `total`.
//! [`project_payees`] applies the same filters when the payload is the full
//! list, including an older server that ignores the query or returns 400.
//! A response that echoes `total` plus a page no larger than `limit`, whose
//! rows already match, is not sliced again.

use serde_json::{json, Value};

pub const DEFAULT_PAYEE_LIMIT: i64 = 100;
pub const MAX_PAYEE_LIMIT: i64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayeeQuery {
    pub search: Option<String>,
    pub prefix: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub compact: bool,
    /// When true (the default), payees whose name is empty or whitespace are
    /// dropped. Those rows are transfer pseudo-payees. Forwarded as `exclude_empty=1`.
    pub exclude_empty: bool,
}

impl PayeeQuery {
    pub fn from_args(
        search: Option<&str>,
        prefix: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
        compact: Option<bool>,
        exclude_empty: Option<bool>,
    ) -> Result<Self, String> {
        let limit = limit.unwrap_or(DEFAULT_PAYEE_LIMIT);
        if !(1..=MAX_PAYEE_LIMIT).contains(&limit) {
            return Err(format!("limit must be between 1 and {MAX_PAYEE_LIMIT}"));
        }
        let offset = offset.unwrap_or(0);
        if offset < 0 {
            return Err("offset must be >= 0".into());
        }
        Ok(Self {
            search: trimmed(search),
            prefix: trimmed(prefix),
            limit,
            offset,
            compact: compact.unwrap_or(false),
            exclude_empty: exclude_empty.unwrap_or(true),
        })
    }
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Turn a payee payload into a bounded page.
///
/// Accepts a bare array or `{"payees": [...]}`. Extra payee fields are kept
/// unless `compact` is set, which leaves `id` and `name` only.
pub fn project_payees(doc: &Value, query: &PayeeQuery) -> Value {
    let rows = payee_rows(doc);
    let server = server_page(doc, query, &rows);
    let mut omitted_empty = 0i64;
    let mut filtered = Vec::with_capacity(rows.len());
    for row in &rows {
        if query.exclude_empty && empty_name(row) {
            omitted_empty += 1;
            continue;
        }
        if !name_matches(row, query) {
            continue;
        }
        filtered.push(shape_payee(row, query.compact));
    }

    let (page, total, offset, truncated, applied) = if let Some(server) = server {
        if filtered.len() == rows.len() {
            let returned = filtered.len() as i64;
            let truncated = server.truncated || server.offset + returned < server.total;
            (filtered, server.total, server.offset, truncated, "server")
        } else {
            page_client(filtered, query)
        }
    } else {
        page_client(filtered, query)
    };
    let returned = page.len() as i64;

    json!({
        "payees": page,
        "total": total,
        "returned": returned,
        "offset": offset,
        "limit": query.limit,
        "truncated": truncated,
        "omitted_empty": omitted_empty,
        "compact": query.compact,
        "exclude_empty": query.exclude_empty,
        "applied": applied,
    })
}

fn page_client(
    filtered: Vec<Value>,
    query: &PayeeQuery,
) -> (Vec<Value>, i64, i64, bool, &'static str) {
    let total = filtered.len() as i64;
    let start = query.offset.max(0) as usize;
    let end = start.saturating_add(query.limit.max(0) as usize);
    let page: Vec<Value> = filtered.into_iter().skip(start).take(end - start).collect();
    let returned = page.len() as i64;
    let truncated = query.offset + returned < total;
    (page, total, query.offset, truncated, "client")
}

struct ServerPage {
    total: i64,
    offset: i64,
    truncated: bool,
}

/// A budget API that already applied this query echoes `total` and a page
/// no larger than `limit`, and every row still matches. Anything else is the
/// unfiltered list (what `/api/v1/payees` returns today) and must be sliced here.
fn server_page(doc: &Value, query: &PayeeQuery, rows: &[Value]) -> Option<ServerPage> {
    let obj = doc.as_object()?;
    let total = obj.get("total")?.as_i64()?;
    let echoed = obj.get("offset").and_then(|v| v.as_i64()).is_some()
        || obj.get("limit").and_then(|v| v.as_i64()).is_some()
        || obj.get("truncated").and_then(|v| v.as_bool()).is_some();
    if !echoed {
        return None;
    }
    if rows.len() as i64 > query.limit || (rows.len() as i64) > total {
        return None;
    }
    if rows
        .iter()
        .any(|row| (query.exclude_empty && empty_name(row)) || !name_matches(row, query))
    {
        return None;
    }
    let offset = obj
        .get("offset")
        .and_then(|v| v.as_i64())
        .unwrap_or(query.offset);
    let truncated = obj
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some(ServerPage {
        total,
        offset,
        truncated,
    })
}

fn payee_rows(doc: &Value) -> Vec<Value> {
    if let Some(rows) = doc.get("payees").and_then(|v| v.as_array()) {
        return rows.clone();
    }
    doc.as_array().cloned().unwrap_or_default()
}

fn empty_name(row: &Value) -> bool {
    payee_name(row).trim().is_empty()
}

fn payee_name(row: &Value) -> &str {
    row.get("name").and_then(|v| v.as_str()).unwrap_or("")
}

fn name_matches(row: &Value, query: &PayeeQuery) -> bool {
    let name = payee_name(row).to_lowercase();
    if let Some(search) = query.search.as_deref() {
        if !name.contains(&search.to_lowercase()) {
            return false;
        }
    }
    if let Some(prefix) = query.prefix.as_deref() {
        if !name.starts_with(&prefix.to_lowercase()) {
            return false;
        }
    }
    true
}

fn shape_payee(row: &Value, compact: bool) -> Value {
    if !compact {
        return row.clone();
    }
    json!({
        "id": row.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "name": payee_name(row),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "payees": [
                {"id": "p-empty", "name": "", "transfer_account_id": "acct-1"},
                {"id": "p-blank", "name": "   ", "transfer_account_id": "acct-2"},
                {"id": "p-venmo", "name": "Transfer to Venmo", "transfer_account_id": null},
                {"id": "p-ach", "name": "ACH Withdrawal VENMO PAYMENT", "split_amount": 0},
                {"id": "p-anna", "name": "City of Anna"},
                {"id": "p-anna-old", "name": "CITY OF ANNA UT"},
                {"id": "p-rent", "name": "Landlord"}
            ]
        })
    }

    fn query(
        search: Option<&str>,
        prefix: Option<&str>,
        limit: i64,
        offset: i64,
        compact: bool,
        exclude_empty: bool,
    ) -> PayeeQuery {
        PayeeQuery::from_args(
            search,
            prefix,
            Some(limit),
            Some(offset),
            Some(compact),
            Some(exclude_empty),
        )
        .unwrap()
    }

    #[test]
    fn defaults_bound_the_page_and_drop_empty_names() {
        let q = PayeeQuery::from_args(None, None, None, None, None, None).unwrap();
        assert_eq!(q.limit, DEFAULT_PAYEE_LIMIT);
        assert_eq!(q.offset, 0);
        assert!(!q.compact);
        assert!(q.exclude_empty);
        assert_eq!(project_payees(&sample(), &q)["exclude_empty"], true);
        let page = project_payees(&sample(), &q);
        assert_eq!(page["omitted_empty"], 2);
        assert_eq!(page["total"], 5);
        assert_eq!(page["returned"], 5);
        assert_eq!(page["truncated"], false);
        assert_eq!(page["applied"], "client");
        assert!(page["payees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| !payee_name(p).trim().is_empty()));
    }

    #[test]
    fn search_prefix_compact_and_paging() {
        let page = project_payees(&sample(), &query(Some("venmo"), None, 1, 0, true, true));
        assert_eq!(page["total"], 2);
        assert_eq!(page["returned"], 1);
        assert_eq!(page["truncated"], true);
        assert_eq!(page["payees"][0]["id"], "p-venmo");
        assert_eq!(page["payees"][0]["name"], "Transfer to Venmo");
        assert!(page["payees"][0].get("transfer_account_id").is_none());
        assert_eq!(page["compact"], true);

        let next = project_payees(&sample(), &query(Some("VENMO"), None, 10, 1, false, true));
        assert_eq!(next["returned"], 1);
        assert_eq!(next["truncated"], false);
        assert_eq!(next["payees"][0]["id"], "p-ach");
        assert_eq!(next["payees"][0]["split_amount"], 0);

        let prefixed = project_payees(
            &sample(),
            &query(Some("anna"), Some("city"), 10, 0, true, true),
        );
        let ids: Vec<&str> = prefixed["payees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["p-anna", "p-anna-old"]);
    }

    #[test]
    fn exclude_empty_false_keeps_transfer_pseudopayees() {
        let page = project_payees(&sample(), &query(None, None, 20, 0, false, false));
        assert_eq!(page["omitted_empty"], 0);
        assert_eq!(page["total"], 7);
        assert_eq!(page["payees"][0]["id"], "p-empty");
    }

    #[test]
    fn server_page_is_not_sliced_again() {
        let doc = json!({
            "payees": [{"id": "p-venmo", "name": "Transfer to Venmo"}],
            "total": 2,
            "offset": 1,
            "limit": 1,
            "truncated": false
        });
        let page = project_payees(&doc, &query(Some("venmo"), None, 1, 1, false, true));
        assert_eq!(page["applied"], "server");
        assert_eq!(page["total"], 2);
        assert_eq!(page["offset"], 1);
        assert_eq!(page["returned"], 1);
        assert_eq!(page["truncated"], false);
        assert_eq!(page["payees"][0]["id"], "p-venmo");
    }

    #[test]
    fn echoed_total_with_an_unfiltered_list_is_still_filtered_here() {
        let mut doc = sample();
        doc["total"] = json!(7);
        doc["limit"] = json!(1);
        let page = project_payees(&doc, &query(Some("landlord"), None, 1, 0, true, true));
        assert_eq!(page["applied"], "client");
        assert_eq!(page["total"], 1);
        assert_eq!(page["payees"][0]["name"], "Landlord");
    }

    #[test]
    fn rejects_limit_and_offset_outside_range() {
        let low = PayeeQuery::from_args(None, None, Some(0), None, None, None).unwrap_err();
        assert!(low.contains("limit must be between 1 and 1000"), "{low}");
        let high = PayeeQuery::from_args(None, None, Some(1001), None, None, None).unwrap_err();
        assert!(high.contains("limit must be between 1 and 1000"), "{high}");
        assert!(PayeeQuery::from_args(None, None, Some(1000), None, None, None).is_ok());
        let offset = PayeeQuery::from_args(None, None, None, Some(-1), None, None).unwrap_err();
        assert!(offset.contains("offset must be >= 0"), "{offset}");
    }

    #[test]
    fn bare_array_payload_is_accepted() {
        let doc = json!([
            {"id": "a", "name": "Alpha"},
            {"id": "b", "name": ""}
        ]);
        let page = project_payees(&doc, &query(Some("alp"), None, 10, 0, true, true));
        assert_eq!(page["total"], 1);
        assert_eq!(page["payees"][0]["id"], "a");
        assert_eq!(page["omitted_empty"], 1);
    }
}
