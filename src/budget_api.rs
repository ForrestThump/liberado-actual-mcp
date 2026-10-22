//! Liberado Budget REST client.
//!
//! SQLite reads stay on `ACTUAL_DB_PATH`. Writes and live API reads (accounts,
//! transactions, envelope months, loans, coaching summary, uncategorized
//! listings) go here so they hit the running Liberado Budget server rather
//! than a Syncthing mirror. Base URL comes from `LIBERADO_BUDGET_API_URL`.
//! When `LIBERADO_BUDGET_API_KEY` is set, every request sends it as
//! `X-API-Key`. The key is never logged or copied into error text.

use std::collections::{BTreeMap, HashSet};

use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};

/// Tool-facing 401. The response body is dropped so a server that echoes the
/// header cannot leak `LIBERADO_BUDGET_API_KEY` through this error.
const API_KEY_REJECTED: &str =
    "liberado-budget API rejected the key (401); check LIBERADO_BUDGET_API_KEY";

#[cfg(test)]
use std::io::{Read, Write};
#[cfg(test)]
use std::net::TcpListener;
#[cfg(test)]
use std::thread;

#[derive(Clone)]
pub struct BudgetApi {
    base: String,
    client: reqwest::Client,
    /// Present only when `LIBERADO_BUDGET_API_KEY` is non-empty.
    /// `SecretString` redacts `Debug` so the key cannot leak through a log.
    api_key: Option<SecretString>,
}

impl BudgetApi {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("LIBERADO_BUDGET_API_URL").ok()?;
        let base = raw.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return None;
        }
        Some(Self {
            base,
            client: reqwest::Client::new(),
            api_key: api_key_from_env(),
        })
    }

    #[cfg(test)]
    fn new(base: impl Into<String>) -> Self {
        Self::with_key(base, None)
    }

    #[cfg(test)]
    fn with_key(base: impl Into<String>, api_key: Option<&str>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            api_key: api_key
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(|k| SecretString::new(k.to_string())),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut req = self.client.request(method, self.url(path));
        if let Some(key) = &self.api_key {
            let value =
                reqwest::header::HeaderValue::from_str(key.expose_secret()).map_err(|_| {
                    "LIBERADO_BUDGET_API_KEY is not a valid HTTP header value".to_string()
                })?;
            req = req.header("X-API-Key", value);
        }
        Ok(req)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut req = self.request(method, path)?;
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("budget API request failed: {e}"))?;
        Self::json_response(resp).await
    }

    async fn json_response(resp: reqwest::Response) -> Result<Value, String> {
        let status = resp.status();
        if status.as_u16() == 401 {
            // Drain the body so the connection can close, but do not include it.
            let _ = resp.text().await;
            return Err(API_KEY_REJECTED.to_string());
        }
        let text = resp
            .text()
            .await
            .map_err(|e| format!("budget API read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("budget API {status}: {text}"));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| format!("budget API bad JSON: {e}"))
    }

    pub async fn get(&self, path: &str) -> Result<Value, String> {
        self.send(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn post_text(
        &self,
        path: &str,
        content_type: &str,
        body: &str,
    ) -> Result<Value, String> {
        let resp = self
            .request(reqwest::Method::POST, path)?
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("budget API request failed: {e}"))?;
        Self::json_response(resp).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    pub async fn create_category(
        &self,
        name: &str,
        kind: Option<&str>,
        group_name: Option<&str>,
    ) -> Result<String, String> {
        let mut body = json!({ "name": name, "kind": kind.unwrap_or("expense") });
        if let Some(g) = group_name {
            body["group_name"] = json!(g);
        }
        let v = self.post("/api/v1/categories", &body).await?;
        v.get("id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("create_category missing id: {v}"))
    }

    pub async fn update_category(
        &self,
        id_or_name: &str,
        name: Option<&str>,
        hidden: Option<bool>,
    ) -> Result<String, String> {
        let id = self.resolve_category(id_or_name).await?;
        let mut body = json!({});
        if let Some(n) = name {
            body["name"] = json!(n);
        }
        if let Some(h) = hidden {
            body["hidden"] = json!(h);
        }
        self.patch(&format!("/api/v1/categories/{id}"), &body)
            .await?;
        Ok(id)
    }

    pub async fn create_payee(&self, name: &str) -> Result<String, String> {
        let v = self
            .post("/api/v1/payees", &json!({ "name": name }))
            .await?;
        v.get("id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("create_payee missing id: {v}"))
    }

    pub async fn rename_payee(&self, id_or_name: &str, new_name: &str) -> Result<String, String> {
        let id = self.resolve_payee(id_or_name).await?;
        self.patch(
            &format!("/api/v1/payees/{id}"),
            &json!({ "name": new_name }),
        )
        .await?;
        Ok(id)
    }

    pub async fn set_transaction_category(
        &self,
        tx_id: &str,
        category: Option<&str>,
    ) -> Result<(), String> {
        let category_id = match category {
            None | Some("") | Some("null") => None,
            Some(c) => Some(self.resolve_category(c).await?),
        };
        self.patch(
            &format!("/api/v1/transactions/{tx_id}"),
            &json!({ "category_id": category_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn set_transaction_payee(&self, tx_id: &str, payee: &str) -> Result<(), String> {
        self.patch(
            &format!("/api/v1/transactions/{tx_id}"),
            &json!({ "payee": payee }),
        )
        .await?;
        Ok(())
    }

    pub async fn categorize_transactions(
        &self,
        ids: &[String],
        category: &str,
        learn: bool,
    ) -> Result<Value, String> {
        let category_id = self.resolve_category(category).await?;
        let mut updated = Vec::new();
        let mut errors = Vec::new();
        for id in ids {
            match self.set_transaction_category(id, Some(&category_id)).await {
                Ok(()) => updated.push(id.clone()),
                Err(e) => errors.push(json!({ "id": id, "error": e })),
            }
        }
        let mut learned = Vec::new();
        if learn {
            let payees = self.payees_for_ids(ids).await?;
            for p in payees {
                match self.create_payee_rule(&p, &category_id).await {
                    Ok(v) => learned.push(v),
                    Err(e) => errors.push(json!({ "learn_payee": p, "error": e })),
                }
            }
        }
        Ok(json!({
            "category_id": category_id,
            "updated": updated,
            "learned_rules": learned,
            "errors": errors,
        }))
    }

    pub async fn create_payee_rule(
        &self,
        payee_regex: &str,
        category: &str,
    ) -> Result<Value, String> {
        let needle = payee_regex.trim();
        if needle.is_empty() {
            return Err("payee_regex is required".into());
        }
        let category_id = self.resolve_category(category).await?;
        let rules = self.get("/api/v1/rules").await?;
        if let Some(existing) = find_equivalent_payee_rule(&rules, needle, &category_id) {
            return Ok(json!({
                "created": false,
                "id": existing,
                "reason": "equivalent payee regex rule already exists",
                "category_id": category_id,
                "payee_regex": needle,
                "matched": 0,
            }));
        }
        let body = payee_rule_body(needle, &category_id);
        let v = self.post("/api/v1/rules", &body).await?;
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let matched = v.get("matched").and_then(|x| x.as_u64()).unwrap_or(0);
        Ok(json!({
            "created": true,
            "id": id,
            "category_id": category_id,
            "payee_regex": needle,
            "matched": matched,
        }))
    }

    pub async fn get_uncategorized_transactions(
        &self,
        account_id: Option<&str>,
        start_date: Option<&str>,
        end_date: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value, String> {
        let limit = limit.unwrap_or(50).clamp(1, 2000);
        let mut qs = format!("limit={limit}");
        if let Some(a) = account_id {
            qs.push_str(&format!("&account_id={a}"));
        }
        if let Some(s) = start_date {
            qs.push_str(&format!("&start_date={s}"));
        }
        if let Some(e) = end_date {
            qs.push_str(&format!("&end_date={e}"));
        }
        self.get(&format!("/api/v1/transactions/uncategorized?{qs}"))
            .await
    }

    pub async fn get_transactions_by_category_regex(
        &self,
        category_regex: &str,
        account_id: Option<&str>,
        start_date: Option<&str>,
        end_date: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value, String> {
        let limit = limit.unwrap_or(50).clamp(1, 2000);
        let mut qs = format!("category_regex={category_regex}&limit={limit}");
        if let Some(a) = account_id {
            qs.push_str(&format!("&account_id={a}"));
        }
        if let Some(s) = start_date {
            qs.push_str(&format!("&start_date={s}"));
        }
        if let Some(e) = end_date {
            qs.push_str(&format!("&end_date={e}"));
        }
        self.get(&format!("/api/v1/transactions/by-category?{qs}"))
            .await
    }

    pub async fn apply_rules(
        &self,
        account_id: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value, String> {
        let mut body = json!({});
        if let Some(a) = account_id {
            body["account_id"] = json!(a);
        }
        if let Some(l) = limit {
            body["limit"] = json!(l);
        }
        self.post("/api/v1/rules/apply", &body).await
    }

    pub async fn set_budget_amount(
        &self,
        month: &str,
        category: &str,
        amount_cents: i64,
    ) -> Result<Value, String> {
        let id = self.resolve_category(category).await?;
        self.put(
            &format!("/api/v1/budgets/{month}/categories/{id}"),
            &json!({ "amount_cents": amount_cents }),
        )
        .await
    }

    pub async fn set_budget_allocations(
        &self,
        month: &str,
        allocations: &[Value],
    ) -> Result<Value, String> {
        let parsed = parse_allocation_items(allocations)?;
        let cats = self.get("/api/v1/categories").await?;
        let mut resolved = Vec::with_capacity(parsed.len());
        for (category, amount_cents) in parsed {
            let id = resolve_category_id(&cats, &category)
                .ok_or_else(|| format!("unknown category '{category}'"))?;
            resolved.push(json!({
                "category_id": id,
                "amount_cents": amount_cents,
            }));
        }
        self.put(
            &format!("/api/v1/budgets/{month}/allocations"),
            &json!({ "allocations": resolved }),
        )
        .await
    }

    pub async fn copy_budget(&self, month: &str, from_month: &str) -> Result<Value, String> {
        self.post(
            &format!("/api/v1/budgets/{month}/copy"),
            &json!({ "from_month": from_month }),
        )
        .await
    }

    pub async fn rollover_budget(&self, month: &str, from_month: &str) -> Result<Value, String> {
        self.post(
            &format!("/api/v1/budgets/{month}/rollover"),
            &json!({ "from_month": from_month }),
        )
        .await
    }

    /// Live account list (`GET /api/v1/accounts`), including `balance_cents`.
    ///
    /// The running Liberado Budget server, not the `ACTUAL_DB_PATH` mirror.
    pub async fn list_live_accounts(&self) -> Result<Value, String> {
        self.get("/api/v1/accounts").await
    }

    /// Live transactions (`GET /api/v1/transactions`).
    ///
    /// `start_date` and `end_date` are query parameters. Liberado Budget applies
    /// them in SQL (`t.date >= start AND t.date <= end`). This client does not
    /// download an unfiltered page and filter it afterward.
    pub async fn get_live_transactions(
        &self,
        query: &LiveTransactionsQuery<'_>,
    ) -> Result<Value, String> {
        let account_id = match query.account.map(str::trim).filter(|s| !s.is_empty()) {
            Some(account) => Some(self.resolve_account(account).await?),
            None => None,
        };
        let category_id = match query.category.map(str::trim).filter(|s| !s.is_empty()) {
            Some(category) => Some(self.resolve_category(category).await?),
            None => None,
        };
        let path = live_transactions_path(&ResolvedTransactionsQuery {
            account_id: account_id.as_deref(),
            category_id: category_id.as_deref(),
            start_date: query.start_date,
            end_date: query.end_date,
            limit: query.limit.unwrap_or(500),
            min_amount_cents: query.min_amount_cents,
            max_amount_cents: query.max_amount_cents,
            payee: query.payee,
            notes: query.notes,
        })?;
        self.get(&path).await
    }

    /// Live envelope month (`GET /api/v1/budgets/{month}`).
    ///
    /// Returns allocation (zero budget) and carry (reflect / rollover) as
    /// separate amounts. Refuses a payload that only has the combined
    /// `budgeted_cents` sum, so leftover carry is not reported as this month's
    /// allocation.
    pub async fn get_live_budget_month(&self, month: &str) -> Result<Value, String> {
        let doc = self
            .get(&format!("/api/v1/budgets/{}", encode_query(month)))
            .await?;
        project_live_budget_month(&doc)
    }

    pub async fn list_loans(&self) -> Result<Value, String> {
        self.get("/api/v1/loans").await
    }

    pub async fn loan_projection(
        &self,
        strategy: Option<&str>,
        extra_cents: Option<i64>,
    ) -> Result<Value, String> {
        let strategy = parse_loan_strategy(strategy)?;
        let extra = extra_cents.unwrap_or(0).max(0);
        self.get(&format!(
            "/api/v1/loans/projection?strategy={strategy}&extra_cents={extra}"
        ))
        .await
    }

    /// Coaching snapshot: on-budget cash, credit, registered loans, optional
    /// `next_target`, and one-month cashflow.
    ///
    /// Query params match `GET /api/v1/summary`: optional `month` (YYYY-MM;
    /// omitted → server current month), `extra_cents` (default 0), `strategy`
    /// (`avalanche` default, or `snowball`). Live REST, not `ACTUAL_DB_PATH`.
    pub async fn get_summary(
        &self,
        month: Option<&str>,
        extra_cents: Option<i64>,
        strategy: Option<&str>,
    ) -> Result<Value, String> {
        let strategy = parse_loan_strategy(strategy)?;
        let extra = extra_cents.unwrap_or(0);
        if extra < 0 {
            return Err("extra_cents must be >= 0".into());
        }
        let mut qs = Vec::new();
        if let Some(m) = month.map(str::trim).filter(|s| !s.is_empty()) {
            qs.push(format!("month={}", encode_query(m)));
        }
        qs.push(format!("extra_cents={extra}"));
        qs.push(format!("strategy={strategy}"));
        self.get(&format!("/api/v1/summary?{}", qs.join("&"))).await
    }

    /// Import a statement via Liberado Budget REST.
    ///
    /// MCP cannot stream a raw HTTP body as a first-class file, so the cleanest
    /// supported shape is: `csv` = raw CSV text (POSTed as `text/csv`), or
    /// `inbox_file` = a filename already in the server's import inbox
    /// (`POST /api/v1/import/inbox/{file}`). Exactly one of those is required.
    /// `account` is id or name; `format` is auto|discover-card|discover-bank|generic.
    pub async fn import_csv(
        &self,
        account: &str,
        format: Option<&str>,
        csv: Option<&str>,
        inbox_file: Option<&str>,
    ) -> Result<Value, String> {
        let csv = csv.map(str::trim).filter(|s| !s.is_empty());
        let source = match (csv, inbox_file) {
            (Some(csv), None) => ImportSource::Csv(csv),
            (None, Some(file)) => ImportSource::Inbox(parse_inbox_filename(file)?),
            (Some(_), Some(_)) => {
                return Err(
                    "import_csv: pass csv text or inbox_file, not both (csv is the MCP body; inbox_file is a server-side filename)"
                        .into(),
                );
            }
            (None, None) => {
                return Err(
                    "import_csv: csv (raw CSV text) or inbox_file (Liberado Budget import-inbox filename) is required"
                        .into(),
                );
            }
        };
        let format = parse_import_format(format)?;
        let account_id = self.resolve_account(account).await?;
        let qs = format!(
            "account_id={}&format={}",
            encode_query(&account_id),
            encode_query(format)
        );
        match source {
            ImportSource::Csv(csv) => {
                self.post_text(&format!("/api/v1/import/csv?{qs}"), "text/csv", csv)
                    .await
            }
            ImportSource::Inbox(file) => {
                self.post(
                    &format!("/api/v1/import/inbox/{}?{qs}", encode_query(&file)),
                    &json!({}),
                )
                .await
            }
        }
    }

    pub async fn create_transfer(
        &self,
        from_account: &str,
        to_account: &str,
        date: &str,
        amount_cents: i64,
        notes: Option<&str>,
        cleared: Option<bool>,
    ) -> Result<Value, String> {
        let from_account_id = self.resolve_account(from_account).await?;
        let to_account_id = self.resolve_account(to_account).await?;
        let mut body = json!({
            "from_account_id": from_account_id,
            "to_account_id": to_account_id,
            "date": date,
            "amount_cents": amount_cents,
        });
        if let Some(n) = notes {
            body["notes"] = json!(n);
        }
        if let Some(c) = cleared {
            body["cleared"] = json!(c);
        }
        self.post("/api/v1/transfers", &body).await
    }

    pub async fn pin_balance(&self, account: &str, balance_cents: i64) -> Result<Value, String> {
        let id = self.resolve_account(account).await?;
        self.post(
            &format!("/api/v1/accounts/{id}/pin-balance"),
            &json!({ "balance_cents": balance_cents }),
        )
        .await
    }

    /// Spending for a date range from live REST.
    ///
    /// `group_by=category` uses `GET /api/v1/spending/by-category` (the server
    /// already drops transfers and non-negative amounts). `week` and `day` use
    /// `GET /api/v1/transactions` and bucket here. A `category_group` query is
    /// sent when asked; a 400 or an unfiltered body falls back to
    /// `GET /api/v1/categories` and a client-side filter.
    ///
    /// Dates are America/Chicago civil dates as stored. Weeks are Monday-start
    /// ISO weeks of those dates, with no timezone shift.
    pub async fn live_spending(
        &self,
        start_date: &str,
        end_date: &str,
        categories: &[String],
        category_group: Option<&str>,
        group_by: Option<&str>,
    ) -> Result<Value, String> {
        let start = require_iso_date("start_date", Some(start_date))?
            .ok_or_else(|| "start_date is required (YYYY-MM-DD)".to_string())?;
        let end = require_iso_date("end_date", Some(end_date))?
            .ok_or_else(|| "end_date is required (YYYY-MM-DD)".to_string())?;
        if start > end {
            return Err("start_date must be on or before end_date".into());
        }
        let group_by = parse_group_by(group_by)?;
        let catalog = parse_category_catalog(&self.get("/api/v1/categories").await?)?;
        let filter = spending_filter(&catalog, categories, category_group)?;
        let group = category_group.map(str::trim).filter(|s| !s.is_empty());
        let (buckets, category_rows) = if group_by == "category" {
            let rows = self
                .fetch_spending_category_rows(&start, &end, group)
                .await?;
            let categories = project_category_spending(&rows, &catalog, &filter);
            let buckets = categories.iter().map(category_bucket).collect();
            (buckets, categories)
        } else {
            let txns = self
                .fetch_spending_transactions(&start, &end, group, &filter)
                .await?;
            aggregate_transactions(&txns, group_by, &catalog, &filter)?
        };
        let sync = self.simplefin_status().await?;
        Ok(render_spending(
            &start,
            &end,
            group_by,
            buckets,
            category_rows,
            sync,
        ))
    }

    /// `GET /api/v1/simplefin/status` plus `GET /api/v1/simplefin/links`.
    /// Never calls `POST /api/v1/simplefin/sync` or `/claim`.
    pub async fn simplefin_status(&self) -> Result<Value, String> {
        let status = self.get("/api/v1/simplefin/status").await?;
        let links = self.get("/api/v1/simplefin/links").await?;
        Ok(project_simplefin_status(&status, &links))
    }

    async fn fetch_spending_category_rows(
        &self,
        start: &str,
        end: &str,
        category_group: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        let path = spending_by_category_path(start, end, category_group);
        match self.get(&path).await {
            Ok(doc) => Ok(array_field(&doc, "categories")),
            Err(e) if category_group.is_some() && is_budget_status(&e, 400) => {
                let doc = self
                    .get(&spending_by_category_path(start, end, None))
                    .await?;
                Ok(array_field(&doc, "categories"))
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_spending_transactions(
        &self,
        start: &str,
        end: &str,
        category_group: Option<&str>,
        filter: &SpendingFilter,
    ) -> Result<Vec<Value>, String> {
        let ids: &[String] = if filter.active { &filter.ids } else { &[] };
        let mut include_group = category_group.is_some();
        let mut offset: u32 = 0;
        let mut rows = Vec::new();
        loop {
            let group = if include_group { category_group } else { None };
            let path = spending_transactions_path(start, end, ids, group, SPENDING_TX_PAGE, offset);
            let doc = match self.get(&path).await {
                Ok(doc) => doc,
                Err(e) if include_group && is_budget_status(&e, 400) => {
                    include_group = false;
                    let retry =
                        spending_transactions_path(start, end, ids, None, SPENDING_TX_PAGE, offset);
                    self.get(&retry).await?
                }
                Err(e) => return Err(e),
            };
            let page = array_field(&doc, "transactions");
            let n = page.len();
            rows.extend(page);
            if n < SPENDING_TX_PAGE as usize {
                break;
            }
            offset = offset.saturating_add(n as u32);
            if offset > 40_000 {
                return Err(
                    "too many transactions in range; narrow start_date and end_date".into(),
                );
            }
        }
        Ok(rows)
    }

    pub async fn resolve_category(&self, name_or_id: &str) -> Result<String, String> {
        let v = self.get("/api/v1/categories").await?;
        resolve_category_id(&v, name_or_id)
            .ok_or_else(|| format!("unknown category '{name_or_id}'"))
    }

    pub async fn resolve_payee(&self, name_or_id: &str) -> Result<String, String> {
        let v = self.get("/api/v1/payees").await?;
        resolve_payee_id(&v, name_or_id).ok_or_else(|| format!("unknown payee '{name_or_id}'"))
    }

    pub async fn resolve_account(&self, name_or_id: &str) -> Result<String, String> {
        let v = self.get("/api/v1/accounts").await?;
        resolve_account_id(&v, name_or_id).ok_or_else(|| format!("unknown account '{name_or_id}'"))
    }

    async fn payees_for_ids(&self, ids: &[String]) -> Result<Vec<String>, String> {
        let v = self.get("/api/v1/transactions?limit=2000").await?;
        let want: std::collections::HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        let mut payees = Vec::new();
        if let Some(arr) = v.get("transactions").and_then(|x| x.as_array()) {
            for t in arr {
                let id = t.get("id").and_then(|x| x.as_str()).unwrap_or("");
                if want.contains(id) {
                    if let Some(p) = t.get("payee").and_then(|x| x.as_str()) {
                        let p = p.trim();
                        if !p.is_empty() {
                            payees.push(p.to_string());
                        }
                    }
                }
            }
        }
        payees.sort();
        payees.dedup();
        Ok(payees)
    }
}

enum ImportSource<'a> {
    Csv(&'a str),
    Inbox(String),
}

/// Parse MCP/REST allocation objects. Each item needs `amount_cents` and either
/// `category` (id or name) or `category_id`.
pub fn parse_allocation_items(items: &[Value]) -> Result<Vec<(String, i64)>, String> {
    if items.is_empty() {
        return Err("allocations must not be empty".into());
    }
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let category = item
            .get("category")
            .or_else(|| item.get("category_id"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let category =
            category.ok_or_else(|| format!("allocations[{i}] missing category or category_id"))?;
        let amount_cents = item
            .get("amount_cents")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| format!("allocations[{i}] missing amount_cents"))?;
        out.push((category.to_string(), amount_cents));
    }
    Ok(out)
}

pub fn payee_rule_body(payee_regex: &str, category_id: &str) -> Value {
    json!({
        "stage": "pre",
        "conditions_op": "and",
        "conditions": [{
            "field": "payee",
            "op": "regex",
            "value": payee_regex,
        }],
        "actions": [{
            "field": "category",
            "value": category_id,
        }],
    })
}

pub fn resolve_category_id(doc: &Value, name_or_id: &str) -> Option<String> {
    let needle = name_or_id.trim();
    if needle.is_empty() {
        return None;
    }
    let groups = doc.get("category_groups")?.as_array()?;
    let mut by_name: Option<String> = None;
    for g in groups {
        let cats = g.get("categories").and_then(|c| c.as_array())?;
        for c in cats {
            let id = c.get("id").and_then(|x| x.as_str())?;
            let name = c.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if id.eq_ignore_ascii_case(needle) {
                return Some(id.to_string());
            }
            if name.eq_ignore_ascii_case(needle) {
                by_name = Some(id.to_string());
            }
        }
    }
    by_name
}

pub fn resolve_payee_id(doc: &Value, name_or_id: &str) -> Option<String> {
    let needle = name_or_id.trim();
    if needle.is_empty() {
        return None;
    }
    let payees = doc.get("payees")?.as_array()?;
    let mut by_name: Option<String> = None;
    for p in payees {
        let id = p.get("id").and_then(|x| x.as_str())?;
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if id.eq_ignore_ascii_case(needle) {
            return Some(id.to_string());
        }
        if name.eq_ignore_ascii_case(needle) {
            by_name = Some(id.to_string());
        }
    }
    by_name
}

pub fn resolve_account_id(doc: &Value, name_or_id: &str) -> Option<String> {
    let needle = name_or_id.trim();
    if needle.is_empty() {
        return None;
    }
    let accounts = doc.get("accounts")?.as_array()?;
    let mut by_name: Option<String> = None;
    for a in accounts {
        let id = a.get("id").and_then(|x| x.as_str())?;
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if id.eq_ignore_ascii_case(needle) {
            return Some(id.to_string());
        }
        if name.eq_ignore_ascii_case(needle) {
            by_name = Some(id.to_string());
        }
    }
    by_name
}

pub fn parse_loan_strategy(s: Option<&str>) -> Result<&'static str, String> {
    match s
        .unwrap_or("avalanche")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "avalanche" => Ok("avalanche"),
        "snowball" => Ok("snowball"),
        other => Err(format!(
            "strategy must be avalanche|snowball, got '{other}'"
        )),
    }
}

pub fn parse_import_format(s: Option<&str>) -> Result<&'static str, String> {
    match s.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok("auto"),
        "discover-card" => Ok("discover-card"),
        "discover-bank" => Ok("discover-bank"),
        "generic" => Ok("generic"),
        other => Err(format!(
            "format must be auto|discover-card|discover-bank|generic, got '{other}'"
        )),
    }
}

pub fn parse_inbox_filename(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("inbox_file is required".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("inbox_file must be a basename in the Liberado Budget import inbox".into());
    }
    Ok(name.to_string())
}

/// Filters for `GET /api/v1/transactions`. `account` and `category` are id or name.
pub struct LiveTransactionsQuery<'a> {
    pub account: Option<&'a str>,
    pub start_date: Option<&'a str>,
    pub end_date: Option<&'a str>,
    pub limit: Option<i64>,
    pub min_amount_cents: Option<i64>,
    pub max_amount_cents: Option<i64>,
    pub category: Option<&'a str>,
    pub payee: Option<&'a str>,
    pub notes: Option<&'a str>,
}

struct ResolvedTransactionsQuery<'a> {
    account_id: Option<&'a str>,
    category_id: Option<&'a str>,
    start_date: Option<&'a str>,
    end_date: Option<&'a str>,
    limit: i64,
    min_amount_cents: Option<i64>,
    max_amount_cents: Option<i64>,
    payee: Option<&'a str>,
    notes: Option<&'a str>,
}

/// Path for a live transaction query. Dates are included whenever the caller
/// set them; they are not applied by dropping rows from a wider response.
fn live_transactions_path(q: &ResolvedTransactionsQuery<'_>) -> Result<String, String> {
    validate_iso_date("start_date", q.start_date)?;
    validate_iso_date("end_date", q.end_date)?;
    let limit = q.limit.clamp(1, 2000);
    let mut parts = Vec::new();
    push_query(&mut parts, "account_id", q.account_id);
    push_query(&mut parts, "start_date", q.start_date);
    push_query(&mut parts, "end_date", q.end_date);
    push_query(&mut parts, "category_id", q.category_id);
    push_query(&mut parts, "payee", q.payee);
    push_query(&mut parts, "notes", q.notes);
    if let Some(n) = q.min_amount_cents {
        parts.push(format!("min_amount_cents={n}"));
    }
    if let Some(n) = q.max_amount_cents {
        parts.push(format!("max_amount_cents={n}"));
    }
    parts.push(format!("limit={limit}"));
    Ok(format!("/api/v1/transactions?{}", parts.join("&")))
}

fn validate_iso_date(name: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if crate::models::date_str_to_int(value).is_none() {
        return Err(format!("Invalid {name} '{value}'; expected YYYY-MM-DD"));
    }
    Ok(())
}

fn push_query(parts: &mut Vec<String>, key: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    parts.push(format!("{key}={}", encode_query(value)));
}

/// `GET /api/v1/budgets/{month}` currently stores one number per category:
/// `budgeted_cents` (and `envelopes[].allocated_cents`) is
/// `SUM(zero_budgets) + SUM(reflect_budgets)`. October 2026 Tithing was a
/// $570.28 allocation plus $282.64 of September carry, and that endpoint
/// reported $852.92. Writing that sum back through `set_budget_allocations`
/// updates only the zero-budget row and leaves the reflect row, so the carry
/// is double-counted.
const BUDGET_MONTH_SPLIT_GAP: &str = "\
GET /api/v1/budgets/{month} does not split this month's allocation from rollover carry. \
budgeted_cents and envelopes.allocated_cents are the sum of zero_budgets (this month's allocation) \
and reflect_budgets (carry). This gap belongs in ForrestThump/liberado-budget: return \
allocation_cents and carry_cents on each category, or zero_budgets and reflect_budgets arrays. \
Do not pass that sum to set_budget_allocations; the write updates only the zero-budget row and \
leaves the reflect row in place, which double-counts carry.";

struct LiveCategoryParts {
    id: String,
    name: String,
    group: String,
    is_income: bool,
    allocation: i64,
    carry: i64,
    spent: i64,
}

/// Project a budget-month JSON document into allocation, carry, spent, and
/// total budget. `budgeted_cents` is never copied into `allocation_cents`.
pub fn project_live_budget_month(doc: &Value) -> Result<Value, String> {
    let month = doc
        .get("month")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let categories = doc.get("categories").and_then(|c| c.as_array());
    if let Some(cats) = categories {
        let split: Vec<bool> = cats.iter().map(category_has_split_fields).collect();
        if split.iter().any(|present| *present) {
            if split.iter().any(|present| !*present) {
                return Err(
                    "budget month categories are missing allocation_cents or carry_cents; refusing to treat budgeted_cents as the allocation"
                        .into(),
                );
            }
            return Ok(render_live_month(&month, parts_from_fields(cats)?));
        }
    }
    if doc.get("zero_budgets").is_some() || doc.get("reflect_budgets").is_some() {
        let zero = require_budget_array(doc, "zero_budgets")?;
        let reflect = require_budget_array(doc, "reflect_budgets")?;
        return Ok(render_live_month(
            &month,
            parts_from_arrays(categories, zero, reflect)?,
        ));
    }
    Err(BUDGET_MONTH_SPLIT_GAP.into())
}

fn category_has_split_fields(category: &Value) -> bool {
    json_i64(category, "allocation_cents").is_some() && json_i64(category, "carry_cents").is_some()
}

fn parts_from_fields(cats: &[Value]) -> Result<Vec<LiveCategoryParts>, String> {
    let mut out = Vec::with_capacity(cats.len());
    for category in cats {
        let allocation = json_i64(category, "allocation_cents")
            .ok_or_else(|| "category missing allocation_cents".to_string())?;
        let carry = json_i64(category, "carry_cents")
            .ok_or_else(|| "category missing carry_cents".to_string())?;
        out.push(parts_from_category(category, allocation, carry));
    }
    Ok(out)
}

fn parts_from_category(category: &Value, allocation: i64, carry: i64) -> LiveCategoryParts {
    LiveCategoryParts {
        id: json_str(category, "category_id").unwrap_or("").to_string(),
        name: json_str(category, "category_name")
            .unwrap_or("")
            .to_string(),
        group: json_str(category, "group_name").unwrap_or("").to_string(),
        is_income: category
            .get("is_income")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        allocation,
        carry,
        spent: json_i64(category, "spent_cents").unwrap_or(0),
    }
}

fn parts_from_arrays(
    categories: Option<&Vec<Value>>,
    zero: &[Value],
    reflect: &[Value],
) -> Result<Vec<LiveCategoryParts>, String> {
    let mut rows = Vec::new();
    let mut index = std::collections::HashMap::<String, usize>::new();
    if let Some(cats) = categories {
        for category in cats {
            let id = json_str(category, "category_id").unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            index.insert(id, rows.len());
            rows.push(parts_from_category(category, 0, 0));
        }
    }
    apply_budget_rows(&mut rows, &mut index, zero, true)?;
    apply_budget_rows(&mut rows, &mut index, reflect, false)?;
    Ok(rows)
}

fn apply_budget_rows(
    rows: &mut Vec<LiveCategoryParts>,
    index: &mut std::collections::HashMap<String, usize>,
    src: &[Value],
    allocation: bool,
) -> Result<(), String> {
    for (i, row) in src.iter().enumerate() {
        let amount = json_i64(row, "amount_cents")
            .or_else(|| json_i64(row, "amount"))
            .ok_or_else(|| format!("budget row {i} missing amount_cents"))?;
        let key = json_str(row, "category_id")
            .or_else(|| json_str(row, "category"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("budget row {i} missing category_id"))?;
        let idx = find_or_insert_category(rows, index, key);
        if allocation {
            rows[idx].allocation += amount;
        } else {
            rows[idx].carry += amount;
        }
    }
    Ok(())
}

fn find_or_insert_category(
    rows: &mut Vec<LiveCategoryParts>,
    index: &mut std::collections::HashMap<String, usize>,
    key: &str,
) -> usize {
    if let Some(&idx) = index.get(key) {
        return idx;
    }
    if let Some(idx) = rows
        .iter()
        .position(|row| row.name.eq_ignore_ascii_case(key))
    {
        index.insert(key.to_string(), idx);
        return idx;
    }
    index.insert(key.to_string(), rows.len());
    rows.push(LiveCategoryParts {
        id: key.to_string(),
        name: key.to_string(),
        group: String::new(),
        is_income: false,
        allocation: 0,
        carry: 0,
        spent: 0,
    });
    rows.len() - 1
}

fn render_live_month(month: &str, cats: Vec<LiveCategoryParts>) -> Value {
    let total_allocation: i64 = cats.iter().map(|c| c.allocation).sum();
    let total_carry: i64 = cats.iter().map(|c| c.carry).sum();
    let total_spent: i64 = cats.iter().map(|c| c.spent).sum();
    let total_budget = total_allocation + total_carry;
    let categories: Vec<Value> = cats
        .iter()
        .map(|c| {
            let budget = c.allocation + c.carry;
            json!({
                "category_id": c.id,
                "category_name": c.name,
                "group_name": c.group,
                "is_income": c.is_income,
                "allocation_cents": c.allocation,
                "allocation_display": crate::models::format_amount(c.allocation),
                "carry_cents": c.carry,
                "carry_display": crate::models::format_amount(c.carry),
                "spent_cents": c.spent,
                "spent_display": crate::models::format_amount(c.spent),
                "budget_cents": budget,
                "budget_display": crate::models::format_amount(budget),
            })
        })
        .collect();
    json!({
        "month": month,
        "categories": categories,
        "total_allocation_cents": total_allocation,
        "total_allocation_display": crate::models::format_amount(total_allocation),
        "total_carry_cents": total_carry,
        "total_carry_display": crate::models::format_amount(total_carry),
        "total_spent_cents": total_spent,
        "total_spent_display": crate::models::format_amount(total_spent),
        "total_budget_cents": total_budget,
        "total_budget_display": crate::models::format_amount(total_budget),
    })
}

fn require_budget_array<'a>(doc: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    match doc.get(key) {
        Some(Value::Array(rows)) => Ok(rows),
        Some(_) => Err(format!("{key} must be an array")),
        None => Err(format!("{key} is missing; {BUDGET_MONTH_SPLIT_GAP}")),
    }
}

fn json_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

fn json_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn api_key_from_env() -> Option<SecretString> {
    let raw = std::env::var("LIBERADO_BUDGET_API_KEY").ok()?;
    let key = raw.trim();
    if key.is_empty() {
        None
    } else {
        Some(SecretString::new(key.to_string()))
    }
}

const SPENDING_TX_PAGE: i64 = 2000;

struct CatInfo {
    id: String,
    name: String,
    group: String,
    is_income: bool,
}

struct SpendingFilter {
    ids: Vec<String>,
    names_lower: HashSet<String>,
    active: bool,
}

impl SpendingFilter {
    fn allows(&self, name: &str, category_id: Option<&str>) -> bool {
        if !self.active {
            return true;
        }
        if let Some(id) = category_id {
            if self.ids.iter().any(|x| x.eq_ignore_ascii_case(id)) {
                return true;
            }
        }
        self.names_lower.contains(&name.to_lowercase())
    }
}

struct CatTotal {
    id: String,
    name: String,
    group: String,
    total: i64,
    count: i64,
}

fn is_budget_status(err: &str, code: u16) -> bool {
    err.starts_with(&format!("budget API {code}"))
}

fn array_field(doc: &Value, key: &str) -> Vec<Value> {
    doc.get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn require_iso_date(name: &str, value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if crate::models::date_str_to_int(value).is_none() {
        return Err(format!("Invalid {name} '{value}'; expected YYYY-MM-DD"));
    }
    Ok(Some(value.to_string()))
}

pub fn parse_group_by(value: Option<&str>) -> Result<&'static str, String> {
    match value
        .unwrap_or("category")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "category" => Ok("category"),
        "week" => Ok("week"),
        "day" => Ok("day"),
        other => Err(format!("group_by must be category|week|day, got '{other}'")),
    }
}

fn parse_category_catalog(doc: &Value) -> Result<Vec<CatInfo>, String> {
    let groups = doc
        .get("category_groups")
        .and_then(|v| v.as_array())
        .ok_or("categories response missing category_groups")?;
    let mut out = Vec::new();
    for group in groups {
        let group_name = json_str(group, "name").unwrap_or("").to_string();
        let group_income = group
            .get("is_income")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cats = group
            .get("categories")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for cat in cats {
            let is_income = cat
                .get("is_income")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || group_income;
            out.push(CatInfo {
                id: json_str(cat, "id").unwrap_or("").to_string(),
                name: json_str(cat, "name").unwrap_or("").to_string(),
                group: group_name.clone(),
                is_income,
            });
        }
    }
    Ok(out)
}

fn spending_filter(
    catalog: &[CatInfo],
    categories: &[String],
    category_group: Option<&str>,
) -> Result<SpendingFilter, String> {
    let group_name = category_group.map(str::trim).filter(|s| !s.is_empty());
    let requested: Vec<&str> = categories
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if group_name.is_none() && requested.is_empty() {
        return Ok(SpendingFilter {
            ids: Vec::new(),
            names_lower: HashSet::new(),
            active: false,
        });
    }
    let in_group: Vec<&CatInfo> = match group_name {
        Some(group) => {
            let rows: Vec<&CatInfo> = catalog
                .iter()
                .filter(|c| c.group.eq_ignore_ascii_case(group))
                .collect();
            if rows.is_empty() {
                return Err(format!("unknown category group '{group}'"));
            }
            rows
        }
        None => catalog.iter().collect(),
    };
    let selected: Vec<&CatInfo> = if requested.is_empty() {
        in_group
    } else {
        let mut matched = Vec::new();
        for name in requested {
            let found = catalog
                .iter()
                .find(|c| c.id.eq_ignore_ascii_case(name) || c.name.eq_ignore_ascii_case(name));
            let Some(cat) = found else {
                return Err(format!("unknown category '{name}'"));
            };
            if group_name.is_some() && !in_group.iter().any(|c| c.id == cat.id) {
                return Err(format!(
                    "category '{name}' is not in group '{}'",
                    group_name.unwrap_or("")
                ));
            }
            matched.push(cat);
        }
        matched
    };
    Ok(SpendingFilter {
        ids: selected.iter().map(|c| c.id.clone()).collect(),
        names_lower: selected.iter().map(|c| c.name.to_lowercase()).collect(),
        active: true,
    })
}

fn spending_by_category_path(start: &str, end: &str, category_group: Option<&str>) -> String {
    let mut parts = vec![
        format!("start_date={}", encode_query(start)),
        format!("end_date={}", encode_query(end)),
    ];
    if let Some(group) = category_group.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("category_group={}", encode_query(group)));
    }
    format!("/api/v1/spending/by-category?{}", parts.join("&"))
}

fn spending_transactions_path(
    start: &str,
    end: &str,
    category_ids: &[String],
    category_group: Option<&str>,
    limit: i64,
    offset: u32,
) -> String {
    let mut parts = vec![
        format!("start_date={}", encode_query(start)),
        format!("end_date={}", encode_query(end)),
    ];
    for id in category_ids {
        parts.push(format!("category_id={}", encode_query(id)));
    }
    if let Some(group) = category_group.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("category_group={}", encode_query(group)));
    }
    parts.push(format!("limit={limit}"));
    if offset > 0 {
        parts.push(format!("offset={offset}"));
    }
    format!("/api/v1/transactions?{}", parts.join("&"))
}

fn category_is_income(catalog: &[CatInfo], name: &str, category_id: Option<&str>) -> bool {
    catalog.iter().any(|cat| {
        let id_hit = category_id.is_some_and(|id| cat.id.eq_ignore_ascii_case(id));
        let name_hit = !name.is_empty() && cat.name.eq_ignore_ascii_case(name);
        (id_hit || name_hit) && cat.is_income
    })
}

fn lookup_category<'a>(
    catalog: &'a [CatInfo],
    name: &str,
    category_id: Option<&str>,
    group: Option<&str>,
) -> Option<&'a CatInfo> {
    if let Some(id) = category_id {
        if let Some(cat) = catalog.iter().find(|c| c.id.eq_ignore_ascii_case(id)) {
            return Some(cat);
        }
    }
    catalog.iter().find(|cat| {
        cat.name.eq_ignore_ascii_case(name)
            && group.is_none_or(|g| g.is_empty() || cat.group.eq_ignore_ascii_case(g))
    })
}

fn has_transfer_id(tx: &Value) -> bool {
    match tx.get("transfer_id") {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn json_flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn include_spending_txn(tx: &Value, catalog: &[CatInfo], filter: &SpendingFilter) -> bool {
    if has_transfer_id(tx) || json_flag(tx, "is_child") || json_flag(tx, "isChild") {
        return false;
    }
    let amount = tx.get("amount_cents").and_then(|v| v.as_i64()).unwrap_or(0);
    if amount >= 0 {
        return false;
    }
    let name = json_str(tx, "category").unwrap_or("");
    let id = json_str(tx, "category_id");
    if category_is_income(catalog, name, id) {
        return false;
    }
    filter.allows(name, id)
}

fn project_category_spending(
    rows: &[Value],
    catalog: &[CatInfo],
    filter: &SpendingFilter,
) -> Vec<Value> {
    let mut totals = Vec::new();
    for row in rows {
        let name = json_str(row, "category_name").unwrap_or("Uncategorized");
        let group = json_str(row, "group_name").unwrap_or("");
        let info = lookup_category(catalog, name, None, Some(group));
        if info.is_some_and(|c| c.is_income) || category_is_income(catalog, name, None) {
            continue;
        }
        if !filter.allows(name, info.map(|c| c.id.as_str())) {
            continue;
        }
        let total = json_i64(row, "total_cents").unwrap_or(0);
        let count = json_i64(row, "transaction_count").unwrap_or(0);
        if total == 0 && count == 0 {
            continue;
        }
        totals.push(CatTotal {
            id: info.map(|c| c.id.clone()).unwrap_or_default(),
            name: name.to_string(),
            group: info
                .map(|c| c.group.clone())
                .filter(|g| !g.is_empty())
                .unwrap_or_else(|| group.to_string()),
            total,
            count,
        });
    }
    totals.sort_by(|a, b| a.total.cmp(&b.total).then_with(|| a.name.cmp(&b.name)));
    totals.iter().map(category_value).collect()
}

fn category_value(cat: &CatTotal) -> Value {
    json!({
        "category_id": cat.id,
        "category_name": cat.name,
        "group_name": cat.group,
        "total_cents": cat.total,
        "total_display": crate::models::format_amount(cat.total),
        "transaction_count": cat.count,
    })
}

fn category_bucket(cat: &Value) -> Value {
    let mut bucket = cat.clone();
    bucket["key"] = cat.get("category_name").cloned().unwrap_or(json!(""));
    bucket
}

fn aggregate_transactions(
    txns: &[Value],
    group_by: &str,
    catalog: &[CatInfo],
    filter: &SpendingFilter,
) -> Result<(Vec<Value>, Vec<Value>), String> {
    struct Acc {
        total: i64,
        count: i64,
        start: String,
        end: String,
    }
    let mut buckets: BTreeMap<String, Acc> = BTreeMap::new();
    let mut cats: BTreeMap<String, CatTotal> = BTreeMap::new();
    for tx in txns {
        if !include_spending_txn(tx, catalog, filter) {
            continue;
        }
        let amount = json_i64(tx, "amount_cents").unwrap_or(0);
        let date = json_str(tx, "date").unwrap_or("");
        if crate::models::date_str_to_int(date).is_none() {
            return Err(format!("transaction date '{date}' is not YYYY-MM-DD"));
        }
        let name = json_str(tx, "category").unwrap_or("");
        let display_name = if name.is_empty() {
            "Uncategorized"
        } else {
            name
        };
        let id = json_str(tx, "category_id");
        let info = lookup_category(catalog, display_name, id, None);
        let (bucket_key, start, end) = match group_by {
            "week" => {
                let week = iso_week(date)?;
                (week.key, week.start, week.end)
            }
            _ => (date.to_string(), date.to_string(), date.to_string()),
        };
        let bucket = buckets.entry(bucket_key).or_insert_with(|| Acc {
            total: 0,
            count: 0,
            start,
            end,
        });
        bucket.total += amount;
        bucket.count += 1;
        let cat_key = info
            .map(|c| c.id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| display_name.to_lowercase());
        let cat = cats.entry(cat_key).or_insert_with(|| CatTotal {
            id: info.map(|c| c.id.clone()).unwrap_or_default(),
            name: display_name.to_string(),
            group: info.map(|c| c.group.clone()).unwrap_or_default(),
            total: 0,
            count: 0,
        });
        cat.total += amount;
        cat.count += 1;
    }
    let bucket_rows = buckets
        .into_iter()
        .map(|(key, acc)| {
            json!({
                "key": key,
                "start_date": acc.start,
                "end_date": acc.end,
                "total_cents": acc.total,
                "total_display": crate::models::format_amount(acc.total),
                "transaction_count": acc.count,
            })
        })
        .collect();
    let mut category_rows: Vec<CatTotal> = cats.into_values().collect();
    category_rows.sort_by(|a, b| a.total.cmp(&b.total).then_with(|| a.name.cmp(&b.name)));
    Ok((
        bucket_rows,
        category_rows.iter().map(category_value).collect(),
    ))
}

fn render_spending(
    start: &str,
    end: &str,
    group_by: &str,
    buckets: Vec<Value>,
    categories: Vec<Value>,
    sync: Value,
) -> Value {
    let total_cents: i64 = categories
        .iter()
        .filter_map(|c| json_i64(c, "total_cents"))
        .sum();
    let transaction_count: i64 = categories
        .iter()
        .filter_map(|c| json_i64(c, "transaction_count"))
        .sum();
    json!({
        "start_date": start,
        "end_date": end,
        "group_by": group_by,
        "timezone": "America/Chicago",
        "buckets": buckets,
        "categories": categories,
        "total_cents": total_cents,
        "total_display": crate::models::format_amount(total_cents),
        "transaction_count": transaction_count,
        "sync": sync,
    })
}

/// Monday-start ISO week of a stored America/Chicago civil date.
/// The date is already the Chicago calendar day; it is not shifted from UTC.
struct IsoWeek {
    key: String,
    start: String,
    end: String,
}

fn iso_week(date: &str) -> Result<IsoWeek, String> {
    let n = crate::models::date_str_to_int(date)
        .ok_or_else(|| format!("Invalid date '{date}'; expected YYYY-MM-DD"))?;
    let days = days_from_civil(n / 10000, (n / 100) % 100, n % 100);
    let dow = (days + 3).rem_euclid(7) + 1;
    let thursday = days + 4 - dow;
    let iso_year = civil_from_days(thursday) / 10000;
    let jan4 = days_from_civil(iso_year, 1, 4);
    let jan4_dow = (jan4 + 3).rem_euclid(7) + 1;
    let week1_monday = jan4 - (jan4_dow - 1);
    let week = (days - week1_monday) / 7 + 1;
    let monday = days - (dow - 1);
    Ok(IsoWeek {
        key: format!("{iso_year}-W{week:02}"),
        start: crate::models::date_int_to_str(civil_from_days(monday)),
        end: crate::models::date_int_to_str(civil_from_days(monday + 6)),
    })
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> i64 {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    year * 10_000 + m * 100 + (doy - (153 * mp + 2) / 5 + 1)
}

pub fn project_simplefin_status(status: &Value, links_doc: &Value) -> Value {
    let configured = status
        .get("configured")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let accounts = project_simplefin_links(links_doc);
    let cursor = json_i64(status, "cursor_epoch").or_else(|| min_link_cursor(&accounts));
    let mut out = json!({
        "configured": configured,
        "cursor_epoch": cursor,
        "linked_accounts": accounts,
    });
    if let Some(days) = status.get("lookback_days").filter(|v| !v.is_null()) {
        out["lookback_days"] = days.clone();
    }
    match status
        .get("last_successful_sync_at")
        .filter(|v| !v.is_null())
    {
        Some(ts) => {
            out["last_successful_sync_at"] = ts.clone();
        }
        None => {
            out["note"] = json!(sync_freshness_note(cursor));
        }
    }
    out
}

fn sync_freshness_note(cursor: Option<i64>) -> String {
    match cursor {
        Some(epoch) => format!(
            "Exact last-sync time is unavailable (no last_successful_sync_at). \
             cursor_epoch {epoch} is a SimpleFIN fetch cursor, not a last-sync timestamp."
        ),
        None => "Exact last-sync time is unavailable (no last_successful_sync_at).".into(),
    }
}

fn project_simplefin_links(doc: &Value) -> Vec<Value> {
    let rows = doc
        .get("links")
        .and_then(|v| v.as_array())
        .or_else(|| doc.as_array());
    let Some(rows) = rows else {
        return Vec::new();
    };
    rows.iter()
        .map(|link| {
            let mut out = json!({
                "account_name": json_str(link, "account_name")
                    .or_else(|| json_str(link, "name"))
                    .unwrap_or(""),
                "account_id": json_str(link, "account_id").unwrap_or(""),
                "simplefin_account_id": json_str(link, "simplefin_account_id").unwrap_or(""),
            });
            copy_present(&mut out, link, "cursor_epoch");
            copy_present(&mut out, link, "last_sync_at");
            copy_present(&mut out, link, "last_sync_attempt_at");
            copy_present(&mut out, link, "last_sync_error");
            out
        })
        .collect()
}

fn copy_present(out: &mut Value, src: &Value, key: &str) {
    if let Some(value) = src.get(key).filter(|v| !v.is_null()) {
        out[key] = value.clone();
    }
}

fn min_link_cursor(accounts: &[Value]) -> Option<i64> {
    accounts
        .iter()
        .filter_map(|a| json_i64(a, "cursor_epoch"))
        .min()
}

fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn find_equivalent_payee_rule(
    doc: &Value,
    payee_regex: &str,
    category_id: &str,
) -> Option<String> {
    let needle = payee_regex.trim().to_lowercase();
    let rules = doc.get("rules")?.as_array()?;
    for r in rules {
        let conds = r.get("conditions").and_then(|c| c.as_array())?;
        let acts = r.get("actions").and_then(|c| c.as_array())?;
        let payee_ok = conds.iter().any(|c| {
            if c.get("field").and_then(|x| x.as_str()).unwrap_or("") != "payee" {
                return false;
            }
            let value = c.get("value").and_then(|x| x.as_str()).unwrap_or("");
            match c.get("op").and_then(|x| x.as_str()).unwrap_or("regex") {
                "contains" | "is" | "equals" | "regex" | "" => value.eq_ignore_ascii_case(&needle),
                _ => false,
            }
        });
        let cat_ok = acts.iter().any(|a| {
            a.get("field").and_then(|x| x.as_str()).unwrap_or("") == "category"
                && a.get("value").and_then(|x| x.as_str()).unwrap_or("") == category_id
        });
        if payee_ok && cat_ok {
            return r.get("id").and_then(|x| x.as_str()).map(str::to_string);
        }
    }
    None
}

/// Minimal HTTP/1.1 JSON mock for client tests (no extra crates).
#[cfg(test)]
pub fn spawn_json_mock(routes: Vec<(String, u16, String)>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for (expect_path, status, body) in routes {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            let line = req.lines().next().unwrap_or("");
            assert!(
                line.contains(&expect_path),
                "expected path {expect_path} in {line}"
            );
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            drop(stream);
        }
    });
    (format!("http://{addr}"), handle)
}

/// Like [`spawn_json_mock`], but checks `X-API-Key` without printing the value.
/// `expect_key: Some(k)` requires that exact header. `None` requires it absent.
#[cfg(test)]
fn spawn_api_key_mock(
    routes: Vec<(String, u16, String, Option<String>)>,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for (expect_path, status, body, expect_key) in routes {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            let line = req.lines().next().unwrap_or("");
            assert!(
                line.contains(&expect_path),
                "expected path {expect_path} in {line}"
            );
            let got = req.lines().find_map(|header| {
                let (name, value) = header.split_once(':')?;
                if name.eq_ignore_ascii_case("x-api-key") {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            });
            match &expect_key {
                Some(key) => assert!(
                    got.as_deref() == Some(key.as_str()),
                    "X-API-Key missing or mismatched"
                ),
                None => assert!(got.is_none(), "X-API-Key sent when unset"),
            }
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            drop(stream);
        }
    });
    (format!("http://{addr}"), handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_category_prefers_id_then_name() {
        let doc = json!({
            "category_groups": [{
                "id": "g1",
                "name": "Expenses",
                "categories": [
                    {"id": "c-groc", "name": "Groceries"},
                    {"id": "c-gas", "name": "Gas"}
                ]
            }]
        });
        assert_eq!(
            resolve_category_id(&doc, "Groceries").as_deref(),
            Some("c-groc")
        );
        assert_eq!(resolve_category_id(&doc, "c-gas").as_deref(), Some("c-gas"));
        assert!(resolve_category_id(&doc, "NoSuch").is_none());
    }

    #[test]
    fn resolve_payee_by_name_or_id() {
        let doc = json!({
            "payees": [
                {"id": "p1", "name": "FRYS-FOOD-DRG"},
                {"id": "p2", "name": "CANTEEN"}
            ]
        });
        assert_eq!(resolve_payee_id(&doc, "canteen").as_deref(), Some("p2"));
        assert_eq!(resolve_payee_id(&doc, "p1").as_deref(), Some("p1"));
    }

    #[test]
    fn equivalent_rule_matches_contains_and_category() {
        let doc = json!({
            "rules": [{
                "id": "r1",
                "conditions": [{"field": "payee", "op": "contains", "value": "FRYS"}],
                "actions": [{"field": "category", "value": "c-groc"}]
            }]
        });
        assert_eq!(
            find_equivalent_payee_rule(&doc, "frys", "c-groc").as_deref(),
            Some("r1")
        );
        assert!(find_equivalent_payee_rule(&doc, "frys", "other").is_none());
        assert!(find_equivalent_payee_rule(&doc, "amazon", "c-groc").is_none());
    }

    #[test]
    fn payee_rule_body_is_budget_rest_shape() {
        let b = payee_rule_body("FRYS", "c-groc");
        assert_eq!(b["conditions_op"], "and");
        assert_eq!(b["conditions"][0]["op"], "regex");
        assert_eq!(b["actions"][0]["field"], "category");
        assert_eq!(b["actions"][0]["value"], "c-groc");
    }

    #[test]
    fn equivalent_rule_matches_regex_op() {
        let doc = json!({
            "rules": [{
                "id": "r1",
                "conditions": [{"field": "payee", "op": "regex", "value": "FRYS"}],
                "actions": [{"field": "category", "value": "c-groc"}]
            }]
        });
        assert_eq!(
            find_equivalent_payee_rule(&doc, "frys", "c-groc").as_deref(),
            Some("r1")
        );
    }

    #[tokio::test]
    async fn create_category_posts_rest_and_returns_id() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/categories".into(),
            201,
            json!({"id": "new-cat"}).to_string(),
        )]);
        let api = BudgetApi::new(base);
        let id = api
            .create_category("Throwaway", Some("expense"), Some("Test"))
            .await
            .unwrap();
        assert_eq!(id, "new-cat");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn create_payee_rule_returns_matched_count() {
        let cats = json!({
            "category_groups": [{
                "categories": [{"id": "c-groc", "name": "Groceries"}]
            }]
        });
        let (base, h) = spawn_json_mock(vec![
            ("/api/v1/categories".into(), 200, cats.to_string()),
            (
                "/api/v1/rules".into(),
                200,
                json!({"rules": []}).to_string(),
            ),
            (
                "/api/v1/rules".into(),
                201,
                json!({"id": "r-new", "matched": 2}).to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api.create_payee_rule("FRYS", "Groceries").await.unwrap();
        assert_eq!(v["created"], true);
        assert_eq!(v["matched"], 2);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn create_payee_rule_skips_duplicate() {
        let existing = json!({
            "rules": [{
                "id": "r-exist",
                "conditions": [{"field": "payee", "op": "contains", "value": "FRYS"}],
                "actions": [{"field": "category", "value": "c-groc"}]
            }]
        });
        let cats = json!({
            "category_groups": [{
                "categories": [{"id": "c-groc", "name": "Groceries"}]
            }]
        });
        let (base, h) = spawn_json_mock(vec![
            ("/api/v1/categories".into(), 200, cats.to_string()),
            ("/api/v1/rules".into(), 200, existing.to_string()),
        ]);
        let api = BudgetApi::new(base);
        let v = api.create_payee_rule("FRYS", "Groceries").await.unwrap();
        assert_eq!(v["created"], false);
        assert_eq!(v["id"], "r-exist");
        h.join().unwrap();
    }

    #[test]
    fn parse_allocations_accepts_category_or_category_id() {
        let items = vec![
            json!({"category": "Groceries", "amount_cents": 15000}),
            json!({"category_id": "c-rent", "amount_cents": 110000}),
        ];
        let parsed = parse_allocation_items(&items).unwrap();
        assert_eq!(
            parsed,
            vec![("Groceries".into(), 15000), ("c-rent".into(), 110000)]
        );
    }

    #[test]
    fn parse_allocations_rejects_empty_and_missing_fields() {
        assert!(parse_allocation_items(&[]).unwrap_err().contains("empty"));
        assert!(parse_allocation_items(&[json!({"amount_cents": 1})])
            .unwrap_err()
            .contains("category"));
        assert!(parse_allocation_items(&[json!({"category": "Food"})])
            .unwrap_err()
            .contains("amount_cents"));
    }

    #[tokio::test]
    async fn set_budget_amount_puts_resolved_category() {
        let cats = json!({
            "category_groups": [{
                "categories": [{"id": "c-groc", "name": "Groceries"}]
            }]
        });
        let (base, h) = spawn_json_mock(vec![
            ("/api/v1/categories".into(), 200, cats.to_string()),
            (
                "/api/v1/budgets/2026-07/categories/c-groc".into(),
                200,
                json!({
                    "month": "2026-07",
                    "category_id": "c-groc",
                    "amount_cents": 50000
                })
                .to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .set_budget_amount("2026-07", "Groceries", 50000)
            .await
            .unwrap();
        assert_eq!(v["category_id"], "c-groc");
        assert_eq!(v["amount_cents"], 50000);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn set_budget_allocations_resolves_names_then_puts() {
        let cats = json!({
            "category_groups": [{
                "categories": [
                    {"id": "c-groc", "name": "Groceries"},
                    {"id": "c-rent", "name": "Rent"}
                ]
            }]
        });
        let (base, h) = spawn_json_mock(vec![
            ("/api/v1/categories".into(), 200, cats.to_string()),
            (
                "/api/v1/budgets/2026-07/allocations".into(),
                200,
                json!({"updated": 2, "month": "2026-07"}).to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .set_budget_allocations(
                "2026-07",
                &[
                    json!({"category": "Groceries", "amount_cents": 15000}),
                    json!({"category": "Rent", "amount_cents": 110000}),
                ],
            )
            .await
            .unwrap();
        assert_eq!(v["updated"], 2);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn set_budget_allocations_unknown_category() {
        let cats = json!({
            "category_groups": [{
                "categories": [{"id": "c-groc", "name": "Groceries"}]
            }]
        });
        let (base, h) = spawn_json_mock(vec![("/api/v1/categories".into(), 200, cats.to_string())]);
        let api = BudgetApi::new(base);
        let err = api
            .set_budget_allocations(
                "2026-07",
                &[json!({"category": "NoSuch", "amount_cents": 1})],
            )
            .await
            .unwrap_err();
        assert_eq!(err, "unknown category 'NoSuch'");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn copy_and_rollover_post_from_month() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/budgets/2026-08/copy".into(),
                200,
                json!({"copied": 1, "from": "2026-07", "month": "2026-08"}).to_string(),
            ),
            (
                "/api/v1/budgets/2026-08/rollover".into(),
                200,
                json!({"rolled": 1, "from": "2026-07", "month": "2026-08"}).to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let copied = api.copy_budget("2026-08", "2026-07").await.unwrap();
        assert_eq!(copied["copied"], 1);
        let rolled = api.rollover_budget("2026-08", "2026-07").await.unwrap();
        assert_eq!(rolled["rolled"], 1);
        h.join().unwrap();
    }

    fn sample_accounts() -> Value {
        json!({
            "accounts": [
                {"id": "a-chk", "name": "Checking"},
                {"id": "a-sav", "name": "Savings"},
                {"id": "a-card", "name": "Discover Card"}
            ]
        })
    }

    #[test]
    fn resolve_account_by_name_or_id() {
        let doc = sample_accounts();
        assert_eq!(
            resolve_account_id(&doc, "checking").as_deref(),
            Some("a-chk")
        );
        assert_eq!(resolve_account_id(&doc, "a-sav").as_deref(), Some("a-sav"));
        assert!(resolve_account_id(&doc, "NoSuch").is_none());
    }

    #[test]
    fn loan_strategy_and_import_format_parse() {
        assert_eq!(parse_loan_strategy(None).unwrap(), "avalanche");
        assert_eq!(parse_loan_strategy(Some("Snowball")).unwrap(), "snowball");
        assert!(parse_loan_strategy(Some("minimums"))
            .unwrap_err()
            .contains("avalanche|snowball"));
        assert_eq!(parse_import_format(None).unwrap(), "auto");
        assert_eq!(
            parse_import_format(Some("discover-bank")).unwrap(),
            "discover-bank"
        );
        assert!(parse_import_format(Some("ofx"))
            .unwrap_err()
            .contains("discover-card"));
        assert!(parse_inbox_filename("../x.csv").is_err());
        assert_eq!(parse_inbox_filename("stmt.csv").unwrap(), "stmt.csv");
    }

    #[tokio::test]
    async fn list_loans_gets_rest() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/loans".into(),
            200,
            json!({
                "loans": [{
                    "name": "Car",
                    "apr_bps": 699,
                    "min_payment_cents": 35000,
                    "balance_cents": 1250000
                }]
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api.list_loans().await.unwrap();
        assert_eq!(v["loans"][0]["apr_bps"], 699);
        assert_eq!(v["loans"][0]["min_payment_cents"], 35000);
        assert_eq!(v["loans"][0]["balance_cents"], 1250000);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn loan_projection_gets_strategy_and_extra() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/loans/projection?strategy=snowball&extra_cents=20000".into(),
            200,
            json!({
                "strategy": "snowball",
                "extra_payment_cents": 20000,
                "months_total": 18,
                "total_interest_cents": 12345,
                "truncated": false
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api
            .loan_projection(Some("snowball"), Some(20000))
            .await
            .unwrap();
        assert_eq!(v["strategy"], "snowball");
        assert_eq!(v["extra_payment_cents"], 20000);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn loan_projection_rejects_bad_strategy() {
        let api = BudgetApi::new("http://127.0.0.1:1");
        let err = api.loan_projection(Some("foo"), Some(0)).await.unwrap_err();
        assert!(err.contains("avalanche|snowball"));
    }

    #[tokio::test]
    async fn get_summary_gets_month_extra_and_strategy() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/summary?month=2026-07&extra_cents=10000&strategy=avalanche".into(),
            200,
            json!({
                "month": "2026-07",
                "on_budget_cash_cents": 150000,
                "credit_balance_cents": -42000,
                "credit_accounts": [{"id": "a-card", "name": "Card", "balance_cents": -42000}],
                "unregistered_loan_accounts": [],
                "loans": [{
                    "id": "l-car",
                    "account_id": "a-car",
                    "name": "Car",
                    "apr_bps": 699,
                    "min_payment_cents": 35000,
                    "balance_cents": 1250000
                }],
                "next_target": {
                    "name": "Car",
                    "account_id": "a-car",
                    "apr_bps": 699,
                    "extra_cents": 10000,
                    "strategy": "avalanche"
                },
                "cashflow": {
                    "month": "2026-07",
                    "income_cents": 300000,
                    "expenses_cents": -9000,
                    "net_cents": 291000
                }
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api
            .get_summary(Some("2026-07"), Some(10000), Some("avalanche"))
            .await
            .unwrap();
        assert_eq!(v["on_budget_cash_cents"], 150000);
        assert_eq!(v["credit_accounts"][0]["name"], "Card");
        assert_eq!(v["next_target"]["extra_cents"], 10000);
        assert_eq!(v["cashflow"]["net_cents"], 291000);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_summary_omits_month_and_defaults_extra_strategy() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/summary?extra_cents=0&strategy=avalanche".into(),
            200,
            json!({
                "on_budget_cash_cents": 0,
                "next_target": null,
                "loans": []
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api.get_summary(None, None, None).await.unwrap();
        assert_eq!(v["on_budget_cash_cents"], 0);
        assert!(v["next_target"].is_null());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_summary_rejects_bad_strategy_and_negative_extra() {
        let api = BudgetApi::new("http://127.0.0.1:1");
        let err = api
            .get_summary(None, Some(0), Some("minimums"))
            .await
            .unwrap_err();
        assert!(err.contains("avalanche|snowball"));
        let err = api
            .get_summary(Some("2026-07"), Some(-1), None)
            .await
            .unwrap_err();
        assert!(err.contains(">= 0"));
    }

    #[tokio::test]
    async fn import_csv_posts_text_body_after_account_resolve() {
        let csv = "Date,Description,Amount\n2026-09-01,Coffee,-4.50\n";
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/import/csv?account_id=a-chk&format=generic".into(),
                200,
                json!({
                    "report": {
                        "format": "generic",
                        "inserted": 1,
                        "skipped": 0,
                        "errors": []
                    },
                    "rules_matched": 0
                })
                .to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .import_csv("Checking", Some("generic"), Some(csv), None)
            .await
            .unwrap();
        assert_eq!(v["report"]["inserted"], 1);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn import_csv_inbox_file_posts_server_path() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/import/inbox/stmt.csv?account_id=a-chk&format=auto".into(),
                200,
                json!({"file": "stmt.csv", "report": {"inserted": 2}}).to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .import_csv("Checking", None, None, Some("stmt.csv"))
            .await
            .unwrap();
        assert_eq!(v["file"], "stmt.csv");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn import_csv_requires_csv_or_inbox() {
        let api = BudgetApi::new("http://127.0.0.1:1");
        let err = api
            .import_csv("Checking", None, None, None)
            .await
            .unwrap_err();
        assert!(err.contains("csv"));
        let err = api
            .import_csv("Checking", None, Some("a,b"), Some("x.csv"))
            .await
            .unwrap_err();
        assert!(err.contains("not both"));
    }

    #[tokio::test]
    async fn create_transfer_resolves_accounts_then_posts() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/transfers".into(),
                201,
                json!({
                    "from_transaction_id": "t-from",
                    "to_transaction_id": "t-to"
                })
                .to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .create_transfer(
                "Checking",
                "Savings",
                "2026-09-01",
                25000,
                Some("save"),
                Some(true),
            )
            .await
            .unwrap();
        assert_eq!(v["from_transaction_id"], "t-from");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn pin_balance_posts_cents() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/accounts/a-chk/pin-balance".into(),
                200,
                json!({
                    "opening_cents": 100000,
                    "activity_cents": 5000,
                    "balance_cents": 105000
                })
                .to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api.pin_balance("Checking", 105000).await.unwrap();
        assert_eq!(v["balance_cents"], 105000);
        h.join().unwrap();
    }

    /// October 2026 Tithing: $570.28 assigned this month plus $282.64 of
    /// September carry. The combined $852.92 must not come back as the allocation.
    fn tithing_split_category() -> Value {
        json!({
            "category_id": "c-tithe",
            "category_name": "Tithing",
            "group_name": "Giving",
            "is_income": false,
            "budgeted_cents": 85292,
            "budgeted_display": "$852.92",
            "spent_cents": 0,
            "spent_display": "$0.00",
            "balance_cents": 85292,
            "balance_display": "$852.92",
            "allocation_cents": 57028,
            "carry_cents": 28264
        })
    }

    fn assert_tithing_split(cat: &Value) {
        assert_eq!(cat["category_name"], "Tithing");
        assert_eq!(cat["allocation_cents"], 57028);
        assert_eq!(cat["carry_cents"], 28264);
        assert_eq!(cat["spent_cents"], 0);
        assert_eq!(cat["budget_cents"], 85292);
        assert_ne!(cat["allocation_cents"], cat["budget_cents"]);
        assert_ne!(cat["allocation_cents"], cat["carry_cents"]);
    }

    #[test]
    fn live_budget_month_keeps_allocation_and_carry_separate() {
        let doc = json!({
            "month": "2026-10",
            "categories": [tithing_split_category()],
            "envelopes": [{
                "category_id": "c-tithe",
                "name": "Tithing",
                "allocated_cents": 85292
            }],
            "total_budgeted_cents": 85292
        });
        let v = project_live_budget_month(&doc).unwrap();
        assert_eq!(v["month"], "2026-10");
        assert_tithing_split(&v["categories"][0]);
        assert_eq!(v["total_allocation_cents"], 57028);
        assert_eq!(v["total_carry_cents"], 28264);
        assert_eq!(v["total_budget_cents"], 85292);
        assert_ne!(v["total_allocation_cents"], v["total_budget_cents"]);
    }

    #[test]
    fn live_budget_month_reads_zero_and_reflect_arrays() {
        let doc = json!({
            "month": "2026-10",
            "categories": [{
                "category_id": "c-tithe",
                "category_name": "Tithing",
                "group_name": "Giving",
                "is_income": false,
                "budgeted_cents": 85292,
                "spent_cents": 0
            }],
            "zero_budgets": [{"category_id": "c-tithe", "amount_cents": 57028}],
            "reflect_budgets": [{"category_id": "c-tithe", "amount_cents": 28264}]
        });
        let v = project_live_budget_month(&doc).unwrap();
        assert_tithing_split(&v["categories"][0]);
    }

    #[test]
    fn live_budget_month_refuses_combined_budgeted_cents() {
        // Shape actually returned by liberado-budget today: one summed number.
        let doc = json!({
            "month": "2026-10",
            "categories": [{
                "category_id": "c-tithe",
                "category_name": "Tithing",
                "group_name": "Giving",
                "is_income": false,
                "budgeted_cents": 85292,
                "spent_cents": 0,
                "balance_cents": 85292
            }],
            "envelopes": [{
                "category_id": "c-tithe",
                "allocated_cents": 85292,
                "activity_cents": 0,
                "remaining_cents": 85292
            }],
            "total_budgeted_cents": 85292
        });
        let err = project_live_budget_month(&doc).unwrap_err();
        assert!(err.contains("reflect_budgets"), "{err}");
        assert!(err.contains("ForrestThump/liberado-budget"), "{err}");
        assert!(err.contains("set_budget_allocations"), "{err}");
        assert!(!err.contains("\"allocation_cents\": 85292"), "{err}");
    }

    #[test]
    fn live_transactions_path_sends_start_and_end() {
        let path = live_transactions_path(&ResolvedTransactionsQuery {
            account_id: None,
            category_id: None,
            start_date: Some("2026-10-01"),
            end_date: Some("2026-10-31"),
            limit: 500,
            min_amount_cents: None,
            max_amount_cents: None,
            payee: None,
            notes: None,
        })
        .unwrap();
        assert_eq!(
            path,
            "/api/v1/transactions?start_date=2026-10-01&end_date=2026-10-31&limit=500"
        );
        let err = live_transactions_path(&ResolvedTransactionsQuery {
            account_id: None,
            category_id: None,
            start_date: Some("October"),
            end_date: Some("2026-10-31"),
            limit: 500,
            min_amount_cents: None,
            max_amount_cents: None,
            payee: None,
            notes: None,
        })
        .unwrap_err();
        assert!(err.contains("start_date"));
    }

    #[tokio::test]
    async fn list_live_accounts_returns_rest_balances() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/accounts".into(),
            200,
            json!({
                "accounts": [
                    {
                        "id": "a-chk",
                        "name": "Checking",
                        "type": "checking",
                        "offbudget": false,
                        "closed": false,
                        "balance_cents": 125000,
                        "balance_display": "$1250.00"
                    }
                ]
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api.list_live_accounts().await.unwrap();
        assert_eq!(v["accounts"][0]["balance_cents"], 125000);
        assert_eq!(v["accounts"][0]["name"], "Checking");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_live_transactions_sends_start_and_end_dates() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/transactions?start_date=2026-10-01&end_date=2026-10-31&limit=50".into(),
            200,
            json!({
                "transactions": [{
                    "id": "t1",
                    "date": "2026-10-04",
                    "amount_cents": -57028
                }]
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api
            .get_live_transactions(&LiveTransactionsQuery {
                account: None,
                start_date: Some("2026-10-01"),
                end_date: Some("2026-10-31"),
                limit: Some(50),
                min_amount_cents: None,
                max_amount_cents: None,
                category: None,
                payee: None,
                notes: None,
            })
            .await
            .unwrap();
        assert_eq!(v["transactions"][0]["date"], "2026-10-04");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_live_transactions_resolves_account_and_keeps_dates() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
            ),
            (
                "/api/v1/transactions?account_id=a-chk&start_date=2026-10-01&end_date=2026-10-31&limit=500"
                    .into(),
                200,
                json!({"transactions": []}).to_string(),
            ),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .get_live_transactions(&LiveTransactionsQuery {
                account: Some("Checking"),
                start_date: Some("2026-10-01"),
                end_date: Some("2026-10-31"),
                limit: None,
                min_amount_cents: None,
                max_amount_cents: None,
                category: None,
                payee: None,
                notes: None,
            })
            .await
            .unwrap();
        assert!(v["transactions"].as_array().unwrap().is_empty());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_live_budget_month_projects_split_from_rest() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/budgets/2026-10".into(),
            200,
            json!({
                "month": "2026-10",
                "categories": [tithing_split_category()],
                "total_budgeted_cents": 85292
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let v = api.get_live_budget_month("2026-10").await.unwrap();
        assert_tithing_split(&v["categories"][0]);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn get_live_budget_month_errors_when_api_sums_carry_into_budgeted() {
        let (base, h) = spawn_json_mock(vec![(
            "/api/v1/budgets/2026-10".into(),
            200,
            json!({
                "month": "2026-10",
                "categories": [{
                    "category_id": "c-tithe",
                    "category_name": "Tithing",
                    "budgeted_cents": 85292,
                    "spent_cents": 0
                }],
                "envelopes": [{"allocated_cents": 85292}]
            })
            .to_string(),
        )]);
        let api = BudgetApi::new(base);
        let err = api.get_live_budget_month("2026-10").await.unwrap_err();
        assert!(err.contains("ForrestThump/liberado-budget"), "{err}");
        assert!(!err.contains("\"allocation_cents\": 85292"), "{err}");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn requests_send_api_key_header_when_configured() {
        let key = "unit-test-api-key";
        let (base, h) = spawn_api_key_mock(vec![(
            "/api/v1/loans".into(),
            200,
            json!({"loans": []}).to_string(),
            Some(key.into()),
        )]);
        let api = BudgetApi::with_key(base, Some(key));
        let v = api.list_loans().await.unwrap();
        assert!(v["loans"].as_array().unwrap().is_empty());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn requests_omit_api_key_header_when_unset() {
        let (base, h) = spawn_api_key_mock(vec![(
            "/api/v1/loans".into(),
            200,
            json!({"loans": []}).to_string(),
            None,
        )]);
        let api = BudgetApi::with_key(base, Some("   "));
        let v = api.list_loans().await.unwrap();
        assert!(v["loans"].as_array().unwrap().is_empty());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn post_text_sends_api_key_header() {
        let key = "unit-test-api-key";
        let (base, h) = spawn_api_key_mock(vec![
            (
                "/api/v1/accounts".into(),
                200,
                sample_accounts().to_string(),
                Some(key.into()),
            ),
            (
                "/api/v1/import/csv?account_id=a-chk&format=auto".into(),
                200,
                json!({"report": {"inserted": 1}}).to_string(),
                Some(key.into()),
            ),
        ]);
        let api = BudgetApi::with_key(base, Some(key));
        let v = api
            .import_csv("Checking", None, Some("a,b\n"), None)
            .await
            .unwrap();
        assert_eq!(v["report"]["inserted"], 1);
        h.join().unwrap();
    }

    #[tokio::test]
    async fn unauthorized_error_names_the_env_var_and_drops_the_body() {
        let key = "unit-test-api-key";
        let (base, h) = spawn_api_key_mock(vec![(
            "/api/v1/loans".into(),
            401,
            format!(r#"{{"error":"unauthorized","echo":"{key}"}}"#),
            Some(key.into()),
        )]);
        let api = BudgetApi::with_key(base, Some(key));
        let err = api.list_loans().await.unwrap_err();
        assert_eq!(
            err,
            "liberado-budget API rejected the key (401); check LIBERADO_BUDGET_API_KEY"
        );
        assert!(!err.contains(key), "error must not echo the API key");
        h.join().unwrap();
    }

    fn sample_categories() -> Value {
        json!({
            "category_groups": [
                {
                    "id": "g-food",
                    "name": "Food",
                    "is_income": false,
                    "categories": [
                        {"id": "c-groc", "name": "Groceries", "is_income": false},
                        {"id": "c-eat", "name": "Eating Out", "is_income": false}
                    ]
                },
                {
                    "id": "g-home",
                    "name": "Home",
                    "is_income": false,
                    "categories": [
                        {"id": "c-rent", "name": "Rent", "is_income": false}
                    ]
                },
                {
                    "id": "g-in",
                    "name": "Income",
                    "is_income": true,
                    "categories": [
                        {"id": "c-pay", "name": "Paycheck", "is_income": true}
                    ]
                }
            ]
        })
    }

    fn status_without_sync_time() -> String {
        json!({
            "configured": true,
            "lookback_days": 30,
            "linked_accounts": 1
        })
        .to_string()
    }

    fn links_with_cursor() -> String {
        json!({
            "links": [{
                "simplefin_account_id": "ACT-1",
                "account_id": "a-chk",
                "account_name": "Checking",
                "cursor_epoch": 1_700_000_000
            }]
        })
        .to_string()
    }

    #[test]
    fn iso_weeks_are_monday_start_chicago_dates() {
        let first = iso_week("1970-01-01").unwrap();
        assert_eq!(first.key, "1970-W01");
        assert_eq!(first.start, "1969-12-29");
        assert_eq!(first.end, "1970-01-04");
        assert_eq!(iso_week("2021-01-01").unwrap().key, "2020-W53");
        let week = iso_week("2025-12-29").unwrap();
        assert_eq!(week.key, "2026-W01");
        assert_eq!(week.start, "2025-12-29");
        assert_eq!(iso_week("2026-01-01").unwrap().key, "2026-W01");
    }

    #[test]
    fn simplefin_status_note_uses_cursor_when_sync_time_is_absent() {
        let status = json!({"configured": true, "lookback_days": 30});
        let links = json!({
            "links": [
                {"account_name": "Checking", "account_id": "a", "simplefin_account_id": "s", "cursor_epoch": 50, "last_sync_error": "timeout"},
                {"account_name": "Card", "account_id": "b", "simplefin_account_id": "t", "cursor_epoch": 80}
            ]
        });
        let v = project_simplefin_status(&status, &links);
        assert_eq!(v["configured"], true);
        assert_eq!(v["cursor_epoch"], 50);
        assert!(v.get("last_successful_sync_at").is_none());
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains("Exact last-sync time is unavailable"),
            "{note}"
        );
        assert!(note.contains("cursor_epoch 50"), "{note}");
        assert_eq!(v["linked_accounts"][0]["last_sync_error"], "timeout");
        assert!(v["linked_accounts"][0].get("last_sync_at").is_none());
        assert!(v["linked_accounts"][1].get("last_sync_error").is_none());
    }

    #[test]
    fn simplefin_status_keeps_sync_time_and_optional_link_fields() {
        let status = json!({
            "configured": true,
            "last_successful_sync_at": "2026-10-20T15:00:00Z",
            "cursor_epoch": 9
        });
        let links = json!({
            "links": [{
                "account_name": "Checking",
                "account_id": "a-chk",
                "simplefin_account_id": "ACT-1",
                "cursor_epoch": 50,
                "last_sync_at": "2026-10-20T15:00:00Z",
                "last_sync_attempt_at": "2026-10-20T15:00:02Z"
            }]
        });
        let v = project_simplefin_status(&status, &links);
        assert_eq!(v["last_successful_sync_at"], "2026-10-20T15:00:00Z");
        assert_eq!(v["cursor_epoch"], 9);
        assert!(v.get("note").is_none());
        assert_eq!(
            v["linked_accounts"][0]["last_sync_at"],
            "2026-10-20T15:00:00Z"
        );
        assert_eq!(
            v["linked_accounts"][0]["last_sync_attempt_at"],
            "2026-10-20T15:00:02Z"
        );
    }

    #[test]
    fn spending_rows_drop_transfers_income_and_other_groups() {
        let catalog = parse_category_catalog(&sample_categories()).unwrap();
        let filter = spending_filter(&catalog, &[], Some("Food")).unwrap();
        let txns = vec![
            json!({"date": "2026-10-19", "amount_cents": -1000, "category": "Groceries"}),
            json!({"date": "2026-10-21", "amount_cents": -250, "category": "Eating Out"}),
            json!({"date": "2026-10-21", "amount_cents": -9999, "category": "Rent", "transfer_id": "xfer-1"}),
            json!({"date": "2026-10-21", "amount_cents": -4000, "category": "Rent"}),
            json!({"date": "2026-10-21", "amount_cents": 500000, "category": "Paycheck"}),
            json!({"date": "2026-10-21", "amount_cents": -50, "category": "Paycheck"}),
        ];
        let (buckets, cats) = aggregate_transactions(&txns, "week", &catalog, &filter).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0]["total_cents"], -1250);
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0]["category_name"], "Groceries");
        assert_eq!(cats[0]["total_cents"], -1000);
        assert_eq!(cats[1]["category_name"], "Eating Out");
        assert_eq!(cats[1]["transaction_count"], 1);
    }

    #[test]
    fn spending_rows_group_by_day() {
        let catalog = parse_category_catalog(&sample_categories()).unwrap();
        let filter = spending_filter(&catalog, &[], None).unwrap();
        let txns = vec![
            json!({"date": "2026-10-19", "amount_cents": -1000, "category": "Groceries"}),
            json!({"date": "2026-10-19", "amount_cents": -250, "category": "Eating Out"}),
            json!({"date": "2026-10-20", "amount_cents": -400, "category": "Rent"}),
            json!({"date": "2026-10-20", "amount_cents": 100, "category": "Rent"}),
        ];
        let (buckets, cats) = aggregate_transactions(&txns, "day", &catalog, &filter).unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0]["key"], "2026-10-19");
        assert_eq!(buckets[0]["start_date"], "2026-10-19");
        assert_eq!(buckets[0]["end_date"], "2026-10-19");
        assert_eq!(buckets[0]["total_cents"], -1250);
        assert_eq!(buckets[0]["transaction_count"], 2);
        assert_eq!(buckets[1]["key"], "2026-10-20");
        assert_eq!(buckets[1]["total_cents"], -400);
        assert_eq!(cats.len(), 3);
    }

    #[tokio::test]
    async fn live_spending_falls_back_when_category_group_is_rejected() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/categories".into(),
                200,
                sample_categories().to_string(),
            ),
            (
                "/api/v1/spending/by-category?start_date=2026-10-01&end_date=2026-10-31&category_group=Food"
                    .into(),
                400,
                json!({"error": "unknown parameter category_group"}).to_string(),
            ),
            (
                "/api/v1/spending/by-category?start_date=2026-10-01&end_date=2026-10-31".into(),
                200,
                json!({
                    "categories": [
                        {"category_name": "Groceries", "group_name": "Food", "total_cents": -4200, "transaction_count": 3},
                        {"category_name": "Rent", "group_name": "Home", "total_cents": -110000, "transaction_count": 1},
                        {"category_name": "Paycheck", "group_name": "Income", "total_cents": -100, "transaction_count": 1}
                    ]
                })
                .to_string(),
            ),
            (
                "/api/v1/simplefin/status".into(),
                200,
                status_without_sync_time(),
            ),
            ("/api/v1/simplefin/links".into(), 200, links_with_cursor()),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .live_spending("2026-10-01", "2026-10-31", &[], Some("Food"), None)
            .await
            .unwrap();
        assert_eq!(v["group_by"], "category");
        assert_eq!(v["categories"].as_array().unwrap().len(), 1);
        assert_eq!(v["categories"][0]["category_name"], "Groceries");
        assert_eq!(v["categories"][0]["category_id"], "c-groc");
        assert_eq!(v["categories"][0]["total_cents"], -4200);
        assert_eq!(v["total_cents"], -4200);
        assert_eq!(v["transaction_count"], 3);
        assert_eq!(v["buckets"][0]["key"], "Groceries");
        let note = v["sync"]["note"].as_str().unwrap();
        assert!(
            note.contains("Exact last-sync time is unavailable"),
            "{note}"
        );
        assert!(note.contains("1700000000"), "{note}");
        assert!(v["sync"].get("last_successful_sync_at").is_none());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn live_spending_filters_ignored_category_group() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/categories".into(),
                200,
                sample_categories().to_string(),
            ),
            (
                "/api/v1/spending/by-category?start_date=2026-10-01&end_date=2026-10-31&category_group=Food"
                    .into(),
                200,
                json!({
                    "categories": [
                        {"category_name": "Groceries", "group_name": "Food", "total_cents": -4200, "transaction_count": 3},
                        {"category_name": "Rent", "group_name": "Home", "total_cents": -110000, "transaction_count": 1}
                    ]
                })
                .to_string(),
            ),
            (
                "/api/v1/simplefin/status".into(),
                200,
                json!({"configured": true, "last_successful_sync_at": "2026-10-20T15:00:00Z"}).to_string(),
            ),
            ("/api/v1/simplefin/links".into(), 200, json!({"links": []}).to_string()),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .live_spending(
                "2026-10-01",
                "2026-10-31",
                &["c-groc".into()],
                Some("Food"),
                Some("category"),
            )
            .await
            .unwrap();
        assert_eq!(v["categories"].as_array().unwrap().len(), 1);
        assert_eq!(v["categories"][0]["category_name"], "Groceries");
        assert_eq!(v["sync"]["last_successful_sync_at"], "2026-10-20T15:00:00Z");
        assert!(v["sync"].get("note").is_none());
        h.join().unwrap();
    }

    #[tokio::test]
    async fn live_spending_weeks_use_transactions_and_skip_transfers() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/categories".into(),
                200,
                sample_categories().to_string(),
            ),
            (
                "/api/v1/transactions?start_date=2026-10-01&end_date=2026-10-31&limit=2000".into(),
                200,
                json!({
                    "transactions": [
                        {"id": "t1", "date": "2026-10-19", "amount_cents": -1000, "category": "Groceries"},
                        {"id": "t2", "date": "2026-10-21", "amount_cents": -500, "category": "Groceries"},
                        {"id": "t3", "date": "2026-10-26", "amount_cents": -2000, "category": "Rent"},
                        {"id": "t4", "date": "2026-10-21", "amount_cents": -9000, "category": "Groceries", "transfer_id": "pair"},
                        {"id": "t5", "date": "2026-10-15", "amount_cents": 250000, "category": "Paycheck"}
                    ]
                })
                .to_string(),
            ),
            (
                "/api/v1/simplefin/status".into(),
                200,
                status_without_sync_time(),
            ),
            ("/api/v1/simplefin/links".into(), 200, links_with_cursor()),
        ]);
        let api = BudgetApi::new(base);
        let v = api
            .live_spending("2026-10-01", "2026-10-31", &[], None, Some("week"))
            .await
            .unwrap();
        assert_eq!(v["group_by"], "week");
        assert_eq!(v["timezone"], "America/Chicago");
        let buckets = v["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0]["key"], iso_week("2026-10-19").unwrap().key);
        assert_eq!(
            buckets[0]["start_date"],
            iso_week("2026-10-19").unwrap().start
        );
        assert_eq!(buckets[0]["total_cents"], -1500);
        assert_eq!(buckets[1]["total_cents"], -2000);
        assert_eq!(v["total_cents"], -3500);
        assert_eq!(v["transaction_count"], 3);
        assert_eq!(v["categories"][0]["category_name"], "Rent");
        assert_eq!(v["categories"][1]["category_name"], "Groceries");
        h.join().unwrap();
    }

    #[tokio::test]
    async fn simplefin_status_reads_status_and_links_only() {
        let (base, h) = spawn_json_mock(vec![
            (
                "/api/v1/simplefin/status".into(),
                200,
                status_without_sync_time(),
            ),
            ("/api/v1/simplefin/links".into(), 200, links_with_cursor()),
        ]);
        let api = BudgetApi::new(base);
        let v = api.simplefin_status().await.unwrap();
        assert_eq!(v["configured"], true);
        assert_eq!(v["linked_accounts"][0]["account_name"], "Checking");
        assert_eq!(v["cursor_epoch"], 1_700_000_000);
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains("Exact last-sync time is unavailable"),
            "{note}"
        );
        h.join().unwrap();
    }
}
