//! Duplicate flags for `GET /api/v1/upcoming`.
//!
//! ForrestThump/liberado-budget#44 sets `stale`, `possible_duplicate_of`, and
//! `duplicate_reason` on each bill. When those fields (or top-level
//! `duplicate_rule`) are present, they are left as the server sent them.
//!
//! An older payload has none of them. This module then applies that PR's rule
//! so the tool shape does not change across the deploy: `stale` is true, and
//! `possible_duplicate_of` names the other schedule, only when that schedule
//! shares the category, the amounts are within 15% of the larger, and its
//! `last_date` is newer by more than this schedule's `interval_days`. A late
//! bill with no such sibling stays `stale: false`. Rows that share a payee id
//! are not duplicates. `schedule_id` (or `id` before `#`) groups expanded
//! occurrences so they are judged once.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

const AMOUNT_TOLERANCE_PERCENT: u64 = 15;

pub fn annotate_upcoming(mut doc: Value, expand_requested: bool) -> Value {
    if !doc.is_object() {
        return doc;
    }
    let mut bills = match doc.get_mut("bills") {
        Some(Value::Array(bills)) => std::mem::take(bills),
        _ => return doc,
    };
    let owned = server_owns_flags(&doc, &bills);
    if !owned {
        flag_duplicates(&mut bills);
    }
    let warnings = if doc
        .get("schedule_warnings")
        .and_then(|v| v.as_array())
        .is_some()
    {
        None
    } else {
        Some(warnings_from_flags(&bills))
    };
    doc["bills"] = Value::Array(bills);
    if doc.get("expanded").and_then(|v| v.as_bool()).is_none() {
        doc["expanded"] = json!(false);
    }
    if expand_requested && doc.get("expanded").and_then(|v| v.as_bool()) != Some(true) {
        doc["expand_note"] = json!(
            "This budget API returned one occurrence per schedule. Count later dates from cadence and interval_days, and say they were inferred."
        );
    }
    if let Some(warnings) = warnings {
        doc["schedule_warnings"] = Value::Array(warnings);
    }
    doc
}

fn server_owns_flags(doc: &Value, bills: &[Value]) -> bool {
    doc.get("duplicate_rule").is_some()
        || bills.iter().any(|bill| {
            bill.get("stale").is_some()
                || bill.get("possible_duplicate_of").is_some()
                || bill.get("duplicate_reason").is_some()
        })
}

fn flag_duplicates(bills: &mut [Value]) {
    let mut order = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, bill) in bills.iter().enumerate() {
        let key = schedule_key(bill);
        if key.is_empty() {
            continue;
        }
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(index);
    }

    let mut targets: HashMap<String, Option<String>> = HashMap::new();
    for key in &order {
        let rep = groups[key][0];
        let mut best: Option<(i64, u64, String)> = None;
        for other in &order {
            if other == key {
                continue;
            }
            let other_rep = groups[other][0];
            let Some(hit) = duplicate_hit(&bills[rep], &bills[other_rep]) else {
                continue;
            };
            let replace = match &best {
                None => true,
                Some((last, gap, id)) => {
                    hit.0 > *last
                        || (hit.0 == *last && (hit.1 < *gap || (hit.1 == *gap && other < id)))
                }
            };
            if replace {
                best = Some((hit.0, hit.1, other.clone()));
            }
        }
        targets.insert(key.clone(), best.map(|(_, _, id)| id));
    }

    for key in &order {
        let target = targets.get(key).and_then(|id| id.clone());
        let (reason, live_key) = if let Some(live_key) = target.clone() {
            let stale_rep = groups[key][0];
            let live_rep = groups[&live_key][0];
            let days =
                last_ord(&bills[live_rep]).unwrap_or(0) - last_ord(&bills[stale_rep]).unwrap_or(0);
            (
                Some(reason_text(
                    &bills[stale_rep],
                    &bills[live_rep],
                    &live_key,
                    days,
                )),
                Some(live_key),
            )
        } else {
            (None, None)
        };
        for index in &groups[key] {
            bills[*index]["stale"] = json!(live_key.is_some());
            bills[*index]["possible_duplicate_of"] = match &live_key {
                Some(id) => json!(id),
                None => Value::Null,
            };
            bills[*index]["duplicate_reason"] = match &reason {
                Some(text) => json!(text),
                None => Value::Null,
            };
        }
    }

    for bill in bills.iter_mut() {
        if bill.get("stale").is_none() {
            bill["stale"] = json!(false);
            bill["possible_duplicate_of"] = Value::Null;
            bill["duplicate_reason"] = Value::Null;
        }
    }
}

/// `(live last_date ordinal, amount gap)` when `live` makes `stale` a duplicate.
fn duplicate_hit(stale: &Value, live: &Value) -> Option<(i64, u64)> {
    if schedule_key(stale) == schedule_key(live)
        || !different_payee(stale, live)
        || !same_category(stale, live)
    {
        return None;
    }
    let gap = amount_gap(stale, live)?;
    let stale_last = last_ord(stale)?;
    let live_last = last_ord(live)?;
    let newer_by = live_last - stale_last;
    if newer_by <= interval_days(stale) {
        return None;
    }
    Some((live_last, gap))
}

fn reason_text(stale: &Value, live: &Value, live_key: &str, days: i64) -> String {
    let category = text(stale, "category_name").unwrap_or("");
    let live_name = text(live, "payee_name").unwrap_or("");
    let stale_amt = amount_cents(stale).unwrap_or(0);
    let live_amt = amount_cents(live).unwrap_or(0);
    let stale_last = text(stale, "last_date").unwrap_or("");
    let live_last = text(live, "last_date").unwrap_or("");
    format!(
        "same category \"{category}\" with a similar amount (this {stale_amt} cents, {live_name} {live_amt} cents); last_date {stale_last} is {days} days older than {live_last} on schedule {live_key}"
    )
}

fn warnings_from_flags(bills: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut warnings = Vec::new();
    for bill in bills {
        let Some(other) = text(bill, "possible_duplicate_of").filter(|id| !id.is_empty()) else {
            continue;
        };
        let key = schedule_key(bill);
        if seen.insert(key.clone()) {
            let other_name = bills
                .iter()
                .find(|candidate| {
                    schedule_key(candidate) == other || text(candidate, "id") == Some(other)
                })
                .and_then(|candidate| text(candidate, "payee_name"))
                .unwrap_or("");
            warnings.push(json!({
                "schedule_id": key,
                "payee": text(bill, "payee_name").unwrap_or(""),
                "category": text(bill, "category_name").unwrap_or(""),
                "possible_duplicate_of": other,
                "other_payee": other_name,
                "stale": bill.get("stale").and_then(|v| v.as_bool()).unwrap_or(true),
                "duplicate_reason": text(bill, "duplicate_reason").unwrap_or(""),
            }));
        }
    }
    warnings.sort_by(|a, b| {
        text(a, "category")
            .unwrap_or("")
            .cmp(text(b, "category").unwrap_or(""))
            .then(
                text(a, "payee")
                    .unwrap_or("")
                    .cmp(text(b, "payee").unwrap_or("")),
            )
    });
    warnings
}

fn schedule_key(bill: &Value) -> String {
    if let Some(id) = nonempty(bill, "schedule_id") {
        return id.to_string();
    }
    nonempty(bill, "id")
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .to_string()
}

fn different_payee(a: &Value, b: &Value) -> bool {
    match (nonempty(a, "payee_id"), nonempty(b, "payee_id")) {
        (Some(left), Some(right)) => left != right,
        _ => {
            let left = text(a, "payee_name").unwrap_or("").trim();
            let right = text(b, "payee_name").unwrap_or("").trim();
            !left.is_empty() && !left.eq_ignore_ascii_case(right)
        }
    }
}

fn same_category(a: &Value, b: &Value) -> bool {
    match (nonempty(a, "category_id"), nonempty(b, "category_id")) {
        (Some(left), Some(right)) => left == right,
        (None, None) => {
            let left = text(a, "category_name").unwrap_or("").trim();
            let right = text(b, "category_name").unwrap_or("").trim();
            !left.is_empty() && left.eq_ignore_ascii_case(right)
        }
        _ => false,
    }
}

fn amount_gap(a: &Value, b: &Value) -> Option<u64> {
    let left = amount_cents(a)?;
    let right = amount_cents(b)?;
    if left.signum() != right.signum() && left != 0 && right != 0 {
        return None;
    }
    let diff = left.abs_diff(right);
    let max = left.unsigned_abs().max(right.unsigned_abs());
    if max == 0 {
        return Some(0);
    }
    if diff.saturating_mul(100) > max.saturating_mul(AMOUNT_TOLERANCE_PERCENT) {
        return None;
    }
    Some(diff)
}

fn interval_days(bill: &Value) -> i64 {
    if let Some(days) = json_i64(bill, "interval_days") {
        if days > 0 {
            return days;
        }
    }
    match text(bill, "cadence").unwrap_or("") {
        "weekly" => 7,
        "biweekly" => 14,
        "quarterly" => 91,
        "yearly" => 365,
        _ => 30,
    }
}

fn last_ord(bill: &Value) -> Option<i64> {
    parse_ymd(text(bill, "last_date")?)
}

fn amount_cents(bill: &Value) -> Option<i64> {
    let value = bill.get("mean_amount_cents")?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|n| n.round() as i64))
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    text(value, key).filter(|s| !s.is_empty())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn json_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| v.as_i64())
}

/// Days since the civil epoch (Howard Hinnant). `1970-01-01` is 0.
fn parse_ymd(value: &str) -> Option<i64> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::too_many_arguments)]
    fn bill(
        name: &str,
        category: &str,
        next_date: &str,
        last_date: &str,
        cents: i64,
        cadence: &str,
        interval: i64,
        source: &str,
        confidence: f64,
    ) -> Value {
        let payee_id: String = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(|ch| ch.to_lowercase())
            .collect();
        let category_id: String = category
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(|ch| ch.to_lowercase())
            .collect();
        json!({
            "id": format!("{payee_id}:{category_id}:{next_date}"),
            "payee_id": payee_id,
            "payee_name": name,
            "category_id": category_id,
            "category_name": category,
            "next_date": next_date,
            "last_date": last_date,
            "mean_amount_cents": cents,
            "cadence": cadence,
            "interval_days": interval,
            "source": source,
            "confidence": confidence,
            "kind": if cents > 0 { "income" } else { "expense" },
        })
    }

    /// Appendix D (today 2026-11-20). The fallback uses the budget API's rule,
    /// so two unrelated inference bills of about $21 are duplicates when one
    /// `last_date` is newer by more than a month. A merely late bill is not.
    fn appendix_d() -> Value {
        json!({
            "today": "2026-11-20",
            "horizon_days": 365,
            "bills": [
                bill("CURSOR AI POWE", "Inference", "2026-11-20", "2026-10-20", -2132, "monthly", 30, "detected", 0.98),
                bill("Optimum", "Subscriptions", "2026-11-21", "2026-10-19", -4532, "monthly", 30, "detected", 1.0),
                bill("VISIBLE", "Utilities", "2026-11-23", "2026-10-23", -2500, "monthly", 30, "detected", 0.99),
                bill("ACH Withdrawal TOYOTA ACH RTL WEB", "Loans", "2026-11-24", "2026-08-19", -56849, "monthly", 30, "detected", 1.0),
                bill("Early Pay PAYROLL ACH from RAYTHEON COMPANY", "Payroll", "2026-11-24", "2026-09-01", 285140, "biweekly", 14, "detected", 1.0),
                bill("Payment", "Loans", "2026-11-24", "2026-08-24", -9645, "monthly", 30, "detected", 0.98),
                bill("Raytheon", "Payroll", "2026-11-24", "2026-11-10", 281954, "biweekly", 14, "detected", 0.95),
                bill("ACH Withdrawal Ch JesusChrist DONATION", "Tithing", "2026-11-25", "2026-09-03", -28514, "biweekly", 14, "override", 1.0),
                bill("Spotify", "Subscriptions", "2026-11-30", "2026-10-30", -1190, "monthly", 30, "detected", 1.0),
                bill("Donation", "Tithing", "2026-12-01", "2026-11-13", -29265, "biweekly", 14, "detected", 0.94),
                bill("ACH Withdrawal VENMO PAYMENT", "Rent", "2026-12-01", "2026-09-02", -185000, "monthly", 30, "override", 1.0),
                bill("Transfer to Venmo", "Rent", "2026-12-02", "2026-11-02", -185000, "monthly", 30, "detected", 1.0),
                bill("MS. AIMEE LITTLES 4805257541 AZ", "Education", "2026-12-04", "2026-08-04", -100, "monthly", 30, "detected", 0.98),
                bill("GROK XAI 8002698161 CA", "Inference", "2026-12-09", "2026-08-09", -3000, "monthly", 30, "detected", 1.0),
                bill("Grayson Collin", "Utilities", "2026-12-10", "2026-11-10", -27517, "monthly", 30, "detected", 0.87),
                bill("OPENAI *CHATGPT", "Inference", "2026-12-10", "2026-08-10", -2128, "monthly", 30, "detected", 1.0),
                bill("ANTHROPIC* CLAUDE SUB 4152360599 CA", "Inference", "2026-12-11", "2026-07-11", -2149, "monthly", 30, "detected", 0.99),
                bill("INTEREST CHARGE ON PURCHASES", "Loans", "2026-12-12", "2026-08-12", -8825, "monthly", 30, "detected", 0.75),
                bill("CITY OF ANNA UT", "Utilities", "2026-12-13", "2026-09-03", -23452, "monthly", 30, "detected", 0.44),
                bill("City of Anna", "Utilities", "2026-12-14", "2026-11-10", -21475, "monthly", 30, "detected", 0.99),
                bill("Discover Credit Card", "Credit Card", "2026-12-14", "2026-11-12", -35150, "monthly", 30, "detected", 0.62),
                bill("VISIBLE", "Utilities", "2026-12-14", "2026-10-14", -2500, "monthly", 30, "detected", 0.99),
                bill("Toyota", "Loans", "2026-12-15", "2026-11-13", -56849, "monthly", 30, "detected", 1.0),
                bill("State Farm", "Insurance", "2026-12-16", "2026-11-17", -20166, "monthly", 30, "detected", 0.92),
            ]
        })
    }

    fn by_name<'a>(doc: &'a Value, name: &str) -> &'a Value {
        doc["bills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["payee_name"] == name)
            .unwrap()
    }

    fn warning_pairs(doc: &Value) -> Vec<(String, String)> {
        doc["schedule_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["payee"].as_str().unwrap().to_string(),
                    w["other_payee"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn appendix_d_uses_the_budget_api_duplicate_rule() {
        let doc = annotate_upcoming(appendix_d(), false);
        assert_eq!(doc["expanded"], false);
        assert!(doc.get("expand_note").is_none());
        assert_eq!(doc["horizon_days"], 365);
        assert_eq!(
            warning_pairs(&doc),
            vec![
                (
                    "ANTHROPIC* CLAUDE SUB 4152360599 CA".into(),
                    "CURSOR AI POWE".into()
                ),
                ("OPENAI *CHATGPT".into(), "CURSOR AI POWE".into()),
                ("ACH Withdrawal TOYOTA ACH RTL WEB".into(), "Toyota".into()),
                (
                    "Early Pay PAYROLL ACH from RAYTHEON COMPANY".into(),
                    "Raytheon".into()
                ),
                (
                    "ACH Withdrawal VENMO PAYMENT".into(),
                    "Transfer to Venmo".into()
                ),
                (
                    "ACH Withdrawal Ch JesusChrist DONATION".into(),
                    "Donation".into()
                ),
                ("CITY OF ANNA UT".into(), "City of Anna".into()),
            ]
        );

        let rent = by_name(&doc, "ACH Withdrawal VENMO PAYMENT");
        assert_eq!(rent["stale"], true);
        assert_eq!(
            rent["possible_duplicate_of"],
            by_name(&doc, "Transfer to Venmo")["id"]
        );
        let reason = rent["duplicate_reason"].as_str().unwrap();
        assert!(reason.contains("61 days older"), "{reason}");
        assert!(reason.contains("Transfer to Venmo"), "{reason}");
        assert_eq!(by_name(&doc, "Transfer to Venmo")["stale"], false);
        assert!(by_name(&doc, "Transfer to Venmo")["possible_duplicate_of"].is_null());

        for name in [
            "CURSOR AI POWE",
            "GROK XAI 8002698161 CA",
            "Payment",
            "INTEREST CHARGE ON PURCHASES",
            "MS. AIMEE LITTLES 4805257541 AZ",
            "Optimum",
            "Spotify",
            "VISIBLE",
            "Grayson Collin",
            "Discover Credit Card",
            "State Farm",
            "Toyota",
            "Raytheon",
            "Donation",
            "City of Anna",
        ] {
            let row = by_name(&doc, name);
            assert_eq!(row["stale"], false, "{name}");
            assert!(row["possible_duplicate_of"].is_null(), "{name}");
        }
    }

    #[test]
    fn server_duplicate_fields_are_not_recomputed() {
        let doc = annotate_upcoming(
            json!({
                "today": "2026-11-20",
                "expanded": true,
                "duplicate_rule": "server",
                "bills": [
                    {
                        "id": "old:rent",
                        "schedule_id": "old:rent",
                        "payee_id": "p-old",
                        "payee_name": "ACH Withdrawal VENMO PAYMENT",
                        "category_id": "rent",
                        "category_name": "Rent",
                        "mean_amount_cents": -185000,
                        "last_date": "2026-09-02",
                        "next_date": "2026-12-01",
                        "interval_days": 30,
                        "stale": false,
                        "possible_duplicate_of": null,
                        "duplicate_reason": null
                    },
                    {
                        "id": "new:rent#2026-12-02",
                        "schedule_id": "new:rent",
                        "payee_id": "p-new",
                        "payee_name": "Transfer to Venmo",
                        "category_id": "rent",
                        "category_name": "Rent",
                        "mean_amount_cents": -185000,
                        "last_date": "2026-11-02",
                        "next_date": "2026-12-02",
                        "interval_days": 30,
                        "stale": false,
                        "possible_duplicate_of": null,
                        "duplicate_reason": null
                    }
                ]
            }),
            true,
        );
        assert_eq!(doc["bills"][0]["stale"], false);
        assert!(doc["bills"][0]["possible_duplicate_of"].is_null());
        assert_eq!(doc["expanded"], true);
        assert_eq!(doc["duplicate_rule"], "server");
        assert!(doc.get("expand_note").is_none());
        assert_eq!(doc["schedule_warnings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn same_payee_and_merely_late_are_not_stale() {
        let visible = annotate_upcoming(
            json!({
                "today": "2026-11-20",
                "bills": [
                    bill("VISIBLE", "Utilities", "2026-11-23", "2026-10-23", -2500, "monthly", 30, "detected", 0.99),
                    bill("VISIBLE", "Utilities", "2026-12-14", "2026-10-14", -2500, "monthly", 30, "detected", 0.99)
                ]
            }),
            false,
        );
        assert_eq!(visible["bills"][0]["stale"], false);
        assert_eq!(visible["bills"][1]["stale"], false);

        let late = annotate_upcoming(
            json!({
                "today": "2026-11-20",
                "bills": [
                    bill("Payment", "Loans", "2026-11-24", "2026-08-24", -9645, "monthly", 30, "detected", 0.98)
                ]
            }),
            true,
        );
        assert_eq!(late["bills"][0]["stale"], false);
        assert_eq!(late["expanded"], false);
        assert!(late["expand_note"]
            .as_str()
            .unwrap()
            .contains("one occurrence"));
    }

    #[test]
    fn expanded_occurrences_share_one_warning() {
        let doc = annotate_upcoming(
            json!({
                "today": "2026-11-20",
                "expanded": true,
                "bills": [
                    {
                        "id": "old:rent#2026-12-01",
                        "schedule_id": "old:rent",
                        "occurrence": 1,
                        "payee_id": "p-old",
                        "payee_name": "ACH Withdrawal VENMO PAYMENT",
                        "category_id": "rent",
                        "category_name": "Rent",
                        "mean_amount_cents": -185000,
                        "last_date": "2026-09-02",
                        "next_date": "2026-12-01",
                        "interval_days": 30
                    },
                    {
                        "id": "old:rent#2027-01-01",
                        "schedule_id": "old:rent",
                        "occurrence": 2,
                        "payee_id": "p-old",
                        "payee_name": "ACH Withdrawal VENMO PAYMENT",
                        "category_id": "rent",
                        "category_name": "Rent",
                        "mean_amount_cents": -185000,
                        "last_date": "2026-09-02",
                        "next_date": "2027-01-01",
                        "interval_days": 30
                    },
                    {
                        "id": "new:rent#2026-12-02",
                        "schedule_id": "new:rent",
                        "occurrence": 1,
                        "payee_id": "p-new",
                        "payee_name": "Transfer to Venmo",
                        "category_id": "rent",
                        "category_name": "Rent",
                        "mean_amount_cents": -185000,
                        "last_date": "2026-11-02",
                        "next_date": "2026-12-02",
                        "interval_days": 30
                    }
                ]
            }),
            true,
        );
        assert_eq!(doc["bills"][0]["possible_duplicate_of"], "new:rent");
        assert_eq!(doc["bills"][1]["possible_duplicate_of"], "new:rent");
        assert_eq!(doc["bills"][0]["stale"], true);
        assert_eq!(doc["bills"][2]["stale"], false);
        assert_eq!(doc["schedule_warnings"].as_array().unwrap().len(), 1);
        assert_eq!(doc["schedule_warnings"][0]["schedule_id"], "old:rent");
        assert!(doc.get("expand_note").is_none());
    }

    #[test]
    fn non_object_payload_is_unchanged() {
        let doc = json!([1, 2]);
        assert_eq!(annotate_upcoming(doc.clone(), false), doc);
    }
}
