//! Liberado Budget REST write client.
//!
//! Reads stay on SQLite. Writes go here so the MCP never opens the shared DB
//! read-write. Base URL comes from `LIBERADO_BUDGET_API_URL`.

use serde_json::{json, Value};
use std::net::TcpListener;
use std::io::{Read, Write};
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

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
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
        let v = self.post("/api/v1/payees", &json!({ "name": name })).await?;
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
            match self
                .set_transaction_category(id, Some(&category_id))
                .await
            {
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
        payee_contains: &str,
        category: &str,
    ) -> Result<Value, String> {
        let needle = payee_contains.trim();
        if needle.is_empty() {
            return Err("payee_contains is required".into());
        }
        let category_id = self.resolve_category(category).await?;
        let rules = self.get("/api/v1/rules").await?;
        if let Some(existing) = find_equivalent_payee_rule(&rules, needle, &category_id) {
            return Ok(json!({
                "created": false,
                "id": existing,
                "reason": "equivalent payee-contains rule already exists",
                "category_id": category_id,
                "payee_contains": needle,
            }));
        }
        let body = payee_rule_body(needle, &category_id);
        let v = self.post("/api/v1/rules", &body).await?;
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        Ok(json!({
            "created": true,
            "id": id,
            "category_id": category_id,
            "payee_contains": needle,
        }))
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

    pub async fn resolve_category(&self, name_or_id: &str) -> Result<String, String> {
        let v = self.get("/api/v1/categories").await?;
        resolve_category_id(&v, name_or_id)
            .ok_or_else(|| format!("unknown category '{name_or_id}'"))
    }

    pub async fn resolve_payee(&self, name_or_id: &str) -> Result<String, String> {
        let v = self.get("/api/v1/payees").await?;
        resolve_payee_id(&v, name_or_id).ok_or_else(|| format!("unknown payee '{name_or_id}'"))
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

pub fn payee_rule_body(payee_contains: &str, category_id: &str) -> Value {
    json!({
        "stage": "pre",
        "conditions_op": "and",
        "conditions": [{
            "field": "payee",
            "op": "contains",
            "value": payee_contains,
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

pub fn find_equivalent_payee_rule(doc: &Value, payee_contains: &str, category_id: &str) -> Option<String> {
    let needle = payee_contains.trim().to_lowercase();
    let rules = doc.get("rules")?.as_array()?;
    for r in rules {
        let conds = r.get("conditions").and_then(|c| c.as_array())?;
        let acts = r.get("actions").and_then(|c| c.as_array())?;
        let payee_ok = conds.iter().any(|c| {
            c.get("field").and_then(|x| x.as_str()).unwrap_or("") == "payee"
                && matches!(
                    c.get("op").and_then(|x| x.as_str()).unwrap_or(""),
                    "contains" | "is" | "equals"
                )
                && c.get("value")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .eq_ignore_ascii_case(&needle)
        });
        let cat_ok = acts.iter().any(|a| {
            a.get("field").and_then(|x| x.as_str()).unwrap_or("") == "category"
                && a.get("value")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    == category_id
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
        assert_eq!(b["conditions"][0]["op"], "contains");
        assert_eq!(b["actions"][0]["field"], "category");
        assert_eq!(b["actions"][0]["value"], "c-groc");
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
}
