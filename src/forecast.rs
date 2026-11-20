//! Compact `GET /api/v1/forecast` for one calendar month.
//!
//! The budget API returns integer cents. This module checks the query, then
//! turns that payload into the tool result: dollar strings, nulls omitted, and
//! `schedule_id` left on each row that has one.

use serde_json::{Map, Value};

use crate::models::format_amount;

pub const DEFAULT_VARIABLE_MONTHS: i64 = 2;
pub const MAX_VARIABLE_MONTHS: i64 = 36;

pub const FORECAST_MISSING: &str =
    "The budget API lacks /forecast (GET /api/v1/forecast). Deploy that endpoint before using forecast_month.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastQuery {
    pub month: String,
    pub variable_months: i64,
}

/// `month` defaults to the calendar month after `today` (`YYYY-MM-DD`).
/// `variable_months` defaults to 2 and must be 0..=36.
pub fn resolve_forecast(
    month: Option<&str>,
    variable_months: Option<i64>,
    today: &str,
) -> Result<ForecastQuery, String> {
    let month = match month.map(str::trim).filter(|s| !s.is_empty()) {
        Some(month) => {
            parse_month(month)?;
            month.to_string()
        }
        None => next_month_after(today)?,
    };
    let variable_months = variable_months.unwrap_or(DEFAULT_VARIABLE_MONTHS);
    if !(0..=MAX_VARIABLE_MONTHS).contains(&variable_months) {
        return Err(format!(
            "variable_months must be between 0 and {MAX_VARIABLE_MONTHS}"
        ));
    }
    Ok(ForecastQuery {
        month,
        variable_months,
    })
}

pub fn next_month_after(today: &str) -> Result<String, String> {
    let today = today.trim();
    if today.len() != 10
        || today.as_bytes().get(4) != Some(&b'-')
        || today.as_bytes().get(7) != Some(&b'-')
    {
        return Err(format!("Invalid date '{today}'; expected YYYY-MM-DD"));
    }
    let (year, month) = parse_month(&today[..7])?;
    let day: i64 = today[8..]
        .parse()
        .map_err(|_| format!("Invalid date '{today}'; expected YYYY-MM-DD"))?;
    if !(1..=31).contains(&day) {
        return Err(format!("Invalid date '{today}'; expected YYYY-MM-DD"));
    }
    Ok(if month == 12 {
        format!("{:04}-01", year + 1)
    } else {
        format!("{:04}-{:02}", year, month + 1)
    })
}

/// UTC calendar day as `YYYY-MM-DD`. The default forecast month is the month after this.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_unix_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Dollars for every `*_cents` integer. Nulls are omitted. Other fields, including
/// `schedule_id`, are kept.
pub fn compact_forecast(doc: &Value) -> Value {
    compact_value(doc).unwrap_or(Value::Null)
}

fn compact_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Array(items) => Some(Value::Array(
            items.iter().filter_map(compact_value).collect(),
        )),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if child.is_null() {
                    continue;
                }
                if let Some(cents) = cents_field(key, child) {
                    let name = key.trim_end_matches("_cents");
                    out.insert(name.to_string(), Value::String(format_amount(cents)));
                    continue;
                }
                if let Some(compacted) = compact_value(child) {
                    out.insert(key.clone(), compacted);
                }
            }
            Some(Value::Object(out))
        }
        other => Some(other.clone()),
    }
}

fn cents_field(key: &str, value: &Value) -> Option<i64> {
    if !key.ends_with("_cents") {
        return None;
    }
    value.as_i64()
}

fn parse_month(month: &str) -> Result<(i64, i64), String> {
    let month = month.trim();
    let (year, mon) = month
        .split_once('-')
        .filter(|(year, mon)| {
            year.len() == 4
                && mon.len() == 2
                && year.chars().all(|c| c.is_ascii_digit())
                && mon.chars().all(|c| c.is_ascii_digit())
        })
        .ok_or_else(|| format!("Invalid month '{month}'; expected YYYY-MM"))?;
    let year: i64 = year
        .parse()
        .map_err(|_| format!("Invalid month '{month}'; expected YYYY-MM"))?;
    let mon: i64 = mon
        .parse()
        .map_err(|_| format!("Invalid month '{month}'; expected YYYY-MM"))?;
    if !(1..=12).contains(&mon) {
        return Err(format!("Invalid month '{month}'; expected YYYY-MM"));
    }
    Ok((year, mon))
}

/// Howard Hinnant. Unix day 0 is 1970-01-01.
fn civil_from_unix_days(unix_days: i64) -> (i64, i64, i64) {
    let days = unix_days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    (year, month as i64, day as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_next_month_and_two_variable_months() {
        let q = resolve_forecast(None, None, "2026-11-20").unwrap();
        assert_eq!(q.month, "2026-12");
        assert_eq!(q.variable_months, 2);
        assert_eq!(next_month_after("2026-12-31").unwrap(), "2027-01");
        assert_eq!(
            resolve_forecast(Some(" 2026-02 "), Some(0), "2026-11-20")
                .unwrap()
                .month,
            "2026-02"
        );
    }

    #[test]
    fn rejects_a_bad_month_or_variable_months() {
        let month = resolve_forecast(Some("2026-13"), None, "2026-11-20").unwrap_err();
        assert!(month.contains("YYYY-MM"), "{month}");
        let months = resolve_forecast(Some("2026-12"), Some(-1), "2026-11-20").unwrap_err();
        assert!(months.contains("variable_months"), "{months}");
        let months = resolve_forecast(Some("2026-12"), Some(37), "2026-11-20").unwrap_err();
        assert!(months.contains("variable_months"), "{months}");
    }

    #[test]
    fn civil_day_zero_is_the_unix_epoch() {
        assert_eq!(civil_from_unix_days(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_days(10957), (2000, 1, 1));
        let today = today_utc();
        assert_eq!(today.len(), 10);
        assert!(next_month_after(&today).is_ok(), "{today}");
    }

    #[test]
    fn compact_renders_dollars_keeps_schedule_ids_and_drops_nulls() {
        let doc = json!({
            "month": "2026-12",
            "as_of": "2026-11-20",
            "income": {
                "total_cents": 563908,
                "items": [
                    {
                        "date": "2026-12-08",
                        "payee": "Raytheon",
                        "amount_cents": 281954,
                        "schedule_id": "ray:pay",
                        "note": null
                    }
                ]
            },
            "bills": {
                "total_cents": -185100,
                "items": [
                    {
                        "date": "2026-12-02",
                        "payee": "Transfer to Venmo",
                        "category": "Rent",
                        "amount_cents": -185000,
                        "last_actual_cents": -185000,
                        "last_date": "2026-11-02",
                        "schedule_id": "p-new:rent",
                        "flags": ["possible_duplicate"]
                    },
                    {
                        "date": "2026-12-04",
                        "payee": "Ms Aimee",
                        "category": "Education",
                        "amount_cents": -100,
                        "last_actual_cents": null,
                        "last_date": null,
                        "schedule_id": "aimee:edu",
                        "flags": null
                    }
                ]
            },
            "leftover_after_bills_cents": 378808,
            "variable_spending": {
                "basis_months": ["2026-10", "2026-11"],
                "total_cents": -42000,
                "by_category": [
                    {"category": "Groceries", "group": "Food", "avg_cents": -21000}
                ]
            },
            "projected_net_cents": 336808,
            "budget": {"allocated_cents": 200000, "carry_cents": 0, "is_empty": false},
            "on_budget_cash_cents": 1500000,
            "warnings": ["Rent may duplicate an older schedule"],
            "unused": null
        });
        let v = compact_forecast(&doc);
        assert_eq!(v["month"], "2026-12");
        assert_eq!(v["income"]["total"], "$5639.08");
        assert_eq!(v["income"]["items"][0]["amount"], "$2819.54");
        assert_eq!(v["income"]["items"][0]["schedule_id"], "ray:pay");
        assert!(v["income"]["items"][0].get("note").is_none());
        assert!(v["income"]["items"][0].get("amount_cents").is_none());

        let rent = &v["bills"]["items"][0];
        assert_eq!(rent["amount"], "-$1850.00");
        assert_eq!(rent["last_actual"], "-$1850.00");
        assert_eq!(rent["schedule_id"], "p-new:rent");
        assert_eq!(rent["flags"][0], "possible_duplicate");
        assert_eq!(rent["category"], "Rent");

        let aimee = &v["bills"]["items"][1];
        assert_eq!(aimee["amount"], "-$1.00");
        assert_eq!(aimee["schedule_id"], "aimee:edu");
        assert!(aimee.get("last_actual").is_none());
        assert!(aimee.get("last_date").is_none());
        assert!(aimee.get("flags").is_none());

        assert_eq!(v["leftover_after_bills"], "$3788.08");
        assert_eq!(v["variable_spending"]["basis_months"][0], "2026-10");
        assert_eq!(v["variable_spending"]["total"], "-$420.00");
        assert_eq!(v["variable_spending"]["by_category"][0]["avg"], "-$210.00");
        assert_eq!(v["projected_net"], "$3368.08");
        assert_eq!(v["budget"]["allocated"], "$2000.00");
        assert_eq!(v["budget"]["carry"], "$0.00");
        assert_eq!(v["budget"]["is_empty"], false);
        assert_eq!(v["on_budget_cash"], "$15000.00");
        assert_eq!(v["warnings"][0], "Rent may duplicate an older schedule");
        assert!(v.get("unused").is_none());
        assert!(v.get("leftover_after_bills_cents").is_none());
    }
}
