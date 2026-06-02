use serde::{Deserialize, Serialize};

// ── Actual Budget server HTTP response wrappers ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub status: String,
    pub data: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct LoginData {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct UserFile {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub name: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(rename = "encryptKeyId")]
    pub encrypt_key_id: Option<String>,
}

// ── Tool output structs (serialized as JSON to the MCP client) ───────────────

#[derive(Debug, Serialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub account_type: String,
    pub offbudget: bool,
    pub closed: bool,
    pub balance_cents: i64,
    pub balance_display: String,
}

#[derive(Debug, Serialize)]
pub struct Transaction {
    pub id: String,
    pub date: String,        // "YYYY-MM-DD"
    pub amount_cents: i64,
    pub amount_display: String,
    pub payee: String,
    pub category: String,
    pub notes: String,
    pub cleared: bool,
    pub reconciled: bool,
}

#[derive(Debug, Serialize)]
pub struct CategoryGroup {
    pub id: String,
    pub name: String,
    pub is_income: bool,
    pub categories: Vec<Category>,
}

#[derive(Debug, Serialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    pub is_income: bool,
    pub hidden: bool,
}

#[derive(Debug, Serialize)]
pub struct Payee {
    pub id: String,
    pub name: String,
    pub transfer_account_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BudgetCategory {
    pub category_id: String,
    pub category_name: String,
    pub group_name: String,
    pub budgeted_cents: i64,
    pub budgeted_display: String,
    pub spent_cents: i64,
    pub spent_display: String,
    pub balance_cents: i64,
    pub balance_display: String,
}

#[derive(Debug, Serialize)]
pub struct BudgetMonth {
    pub month: String,
    pub categories: Vec<BudgetCategory>,
    pub total_budgeted_display: String,
    pub total_spent_display: String,
    pub total_balance_display: String,
}

#[derive(Debug, Serialize)]
pub struct MonthSummary {
    pub month: String,
    pub income_cents: i64,
    pub income_display: String,
    pub expenses_cents: i64,
    pub expenses_display: String,
    pub net_cents: i64,
    pub net_display: String,
}

#[derive(Debug, Serialize)]
pub struct CategorySpending {
    pub category_name: String,
    pub group_name: String,
    pub total_cents: i64,
    pub total_display: String,
    pub transaction_count: i64,
}

// ── Amount formatting ────────────────────────────────────────────────────────

// Actual Budget stores monetary values as integer cents (100 = $1.00).
pub fn format_amount(cents: i64) -> String {
    let negative = cents < 0;
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let rem = abs % 100;
    if negative {
        format!("-${dollars}.{rem:02}")
    } else {
        format!("${dollars}.{rem:02}")
    }
}

// ── Date helpers ─────────────────────────────────────────────────────────────

// Actual Budget stores transaction dates as YYYYMMDD integers.
pub fn date_int_to_str(d: i64) -> String {
    let year = d / 10000;
    let month = (d % 10000) / 100;
    let day = d % 100;
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn date_str_to_int(s: &str) -> Option<i64> {
    // Accepts "YYYY-MM-DD"
    let parts: Vec<&str> = s.splitn(3, '-').collect();
    if parts.len() != 3 { return None; }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    Some(y * 10000 + m * 100 + d)
}

// First and last day of month as YYYYMMDD integers, given "YYYY-MM".
pub fn month_bounds(month: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = month.splitn(2, '-').collect();
    if parts.len() != 2 { return None; }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let start = y * 10000 + m * 100 + 1;
    let (next_y, next_m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let end = next_y * 10000 + next_m * 100 + 1;
    Some((start, end))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_positive_amount() {
        assert_eq!(format_amount(500), "$5.00");
        assert_eq!(format_amount(100), "$1.00");
        assert_eq!(format_amount(1), "$0.01");
        assert_eq!(format_amount(0), "$0.00");
    }

    #[test]
    fn format_negative_amount() {
        assert_eq!(format_amount(-500), "-$5.00");
        assert_eq!(format_amount(-1234), "-$12.34");
    }

    #[test]
    fn date_int_round_trips() {
        assert_eq!(date_int_to_str(20240115), "2024-01-15");
        assert_eq!(date_int_to_str(20241231), "2024-12-31");
        assert_eq!(date_int_to_str(20010101), "2001-01-01");
    }

    #[test]
    fn date_str_valid() {
        assert_eq!(date_str_to_int("2024-01-15"), Some(20240115));
        assert_eq!(date_str_to_int("2024-12-31"), Some(20241231));
    }

    #[test]
    fn date_str_invalid() {
        assert_eq!(date_str_to_int("not-a-date"), None);
        assert_eq!(date_str_to_int("2024"), None);
        assert_eq!(date_str_to_int(""), None);
    }

    #[test]
    fn month_bounds_normal() {
        assert_eq!(month_bounds("2024-01"), Some((20240101, 20240201)));
        assert_eq!(month_bounds("2024-06"), Some((20240601, 20240701)));
    }

    #[test]
    fn month_bounds_year_rollover() {
        assert_eq!(month_bounds("2024-12"), Some((20241201, 20250101)));
    }

    #[test]
    fn month_bounds_invalid() {
        assert_eq!(month_bounds("2024"), None);
        assert_eq!(month_bounds("not-valid"), None);
    }
}
