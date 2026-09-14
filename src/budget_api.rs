//! Liberado Budget REST client.
//!
//! SQLite reads stay on `ACTUAL_DB_PATH`. Writes and live API reads (loans,
//! coaching summary, uncategorized listings) go here so they hit the running
//! Liberado Budget server rather than a Syncthing mirror. Base URL comes from
//! `LIBERADO_BUDGET_API_URL`.

use serde_json::{json, Value};

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
        })
    }

    #[cfg(test)]
    fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut req = self.client.request(method, self.url(path));
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
            .client
            .request(reqwest::Method::POST, self.url(path))
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
}
