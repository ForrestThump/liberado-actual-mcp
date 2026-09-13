use secrecy::{ExposeSecret, SecretString};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;
use turbomcp::prelude::*;

use crate::{
    actual::{find_budget_file, ActualClient, SQLITE_MAGIC},
    budget_api::BudgetApi,
    db,
    models::{date_str_to_int, format_amount, month_bounds, month_to_ym},
};

fn json_result<T: serde::Serialize>(val: &T) -> McpResult<String> {
    serde_json::to_string_pretty(val).map_err(|e| McpError::internal(e.to_string()))
}

/// Map a Liberado Budget REST error (from `BudgetApi::send`) to the right MCP
/// error kind. `send` formats non-2xx as `budget API {status}: {text}`, so
/// client errors (4xx) surface as invalid-params / permission-denied instead of
/// a generic internal error, which agents cannot act on.
fn budget_api_error(e: String) -> McpError {
    if e.starts_with("budget API 401") || e.starts_with("budget API 403") {
        McpError::permission_denied(e)
    } else if e.starts_with("budget API 4") {
        McpError::invalid_params(e)
    } else {
        McpError::internal(e)
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

struct BudgetCache {
    db_path: PathBuf,
    // Held to prevent deletion of the temp file until all in-flight queries finish.
    _temp_file: Option<tempfile::NamedTempFile>,
}

enum BudgetSource {
    Local(PathBuf),
    Server {
        client: ActualClient,
        password: SecretString,
        budget_id: Option<String>,
    },
}

struct AppState {
    source: BudgetSource,
    // Arc so query closures can hold a reference that keeps _temp_file alive.
    cache: RwLock<Option<Arc<BudgetCache>>>,
    budget_api: Option<BudgetApi>,
}

// ── Server ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ActualServer {
    state: Arc<AppState>,
}

impl ActualServer {
    pub fn new() -> anyhow::Result<Self> {
        let db_path_env = std::env::var("ACTUAL_DB_PATH").ok();
        let server_url = std::env::var("ACTUAL_SERVER_URL").ok();

        let source = if let Some(p) = db_path_env {
            BudgetSource::Local(PathBuf::from(p))
        } else if let Some(url) = server_url {
            let password = std::env::var("ACTUAL_PASSWORD").map_err(|_| {
                anyhow::anyhow!("ACTUAL_PASSWORD must be set when using ACTUAL_SERVER_URL")
            })?;
            BudgetSource::Server {
                client: ActualClient::new(url)?,
                password: SecretString::new(password),
                budget_id: std::env::var("ACTUAL_BUDGET_ID").ok(),
            }
        } else {
            anyhow::bail!(
                "Set either ACTUAL_DB_PATH (local SQLite) or ACTUAL_SERVER_URL + ACTUAL_PASSWORD"
            );
        };

        Ok(Self {
            state: Arc::new(AppState {
                source,
                cache: RwLock::new(None),
                budget_api: BudgetApi::from_env(),
            }),
        })
    }

    /// Returns an Arc to the populated cache, downloading the budget file if needed.
    ///
    /// Callers must hold the returned Arc for the lifetime of any I/O that uses the
    /// cache's db_path. This keeps _temp_file alive and prevents a race where refresh
    /// could delete the temp file while a concurrent query is still opening it.
    async fn db_cache(&self) -> McpResult<Arc<BudgetCache>> {
        // Fast path: cache already populated.
        {
            let r = self.state.cache.read().await;
            if let Some(c) = r.as_ref() {
                return Ok(Arc::clone(c));
            }
        }

        let mut w = self.state.cache.write().await;
        // Re-check after acquiring write lock.
        if let Some(c) = w.as_ref() {
            return Ok(Arc::clone(c));
        }

        let cache = match &self.state.source {
            BudgetSource::Local(path) => {
                if !path.exists() {
                    return Err(McpError::internal(format!(
                        "ACTUAL_DB_PATH not found: {}",
                        path.display()
                    )));
                }
                {
                    use tokio::io::AsyncReadExt;
                    let mut f = tokio::fs::File::open(path).await.map_err(|e| {
                        McpError::internal(format!("Cannot open {}: {e}", path.display()))
                    })?;
                    let mut header = [0u8; 16];
                    f.read_exact(&mut header).await.map_err(|e| {
                        McpError::internal(format!("Cannot read {}: {e}", path.display()))
                    })?;
                    if !header.starts_with(SQLITE_MAGIC) {
                        return Err(McpError::internal(format!(
                            "{} is not a SQLite file. Verify ACTUAL_DB_PATH points to the \
                             db.sqlite inside your Actual Budget data directory.",
                            path.display()
                        )));
                    }
                }
                BudgetCache {
                    db_path: path.clone(),
                    _temp_file: None,
                }
            }
            BudgetSource::Server {
                client,
                password,
                budget_id,
            } => {
                let token = client
                    .login(password.expose_secret())
                    .await
                    .map_err(|e| McpError::internal(format!("Login failed: {e}")))?;

                let files = client
                    .list_files(&token)
                    .await
                    .map_err(|e| McpError::internal(format!("Failed to list files: {e}")))?;

                let file = find_budget_file(&files, budget_id.as_deref()).ok_or_else(|| {
                    McpError::internal(
                        "No budget file found on server. Set ACTUAL_BUDGET_ID or ensure at least one budget exists.".to_string(),
                    )
                })?;

                if file.encrypt_key_id.is_some() {
                    return Err(McpError::internal(
                        "This budget is encrypted. Encryption is not supported in v1 — see ROADMAP.md. \
                         Use ACTUAL_DB_PATH to point directly to a locally-synced SQLite file instead."
                            .to_string(),
                    ));
                }

                let bytes = client
                    .download_file(&token, &file.file_id)
                    .await
                    .map_err(|e| McpError::internal(format!("Download failed: {e}")))?;

                if !bytes.starts_with(SQLITE_MAGIC) {
                    return Err(McpError::internal(
                        "Downloaded budget is not a plain SQLite file. Your Actual Budget server \
                         may be using CRDT sync (full sync mode), which is not yet supported. \
                         Use ACTUAL_DB_PATH to point directly to a locally-synced db.sqlite file. \
                         See ROADMAP.md for v2 plans."
                            .to_string(),
                    ));
                }

                let mut tmp = tempfile::NamedTempFile::new()
                    .map_err(|e| McpError::internal(format!("Temp file error: {e}")))?;
                use std::io::Write;
                tmp.write_all(&bytes)
                    .map_err(|e| McpError::internal(format!("Write error: {e}")))?;
                let db_path = tmp.path().to_path_buf();
                BudgetCache {
                    db_path,
                    _temp_file: Some(tmp),
                }
            }
        };

        let arc = Arc::new(cache);
        *w = Some(Arc::clone(&arc));
        Ok(arc)
    }

    /// Run a SQLite query on a blocking thread.
    ///
    /// The Arc<BudgetCache> is moved into the closure so the temp file (if any)
    /// cannot be dropped until this task completes, preventing a race with refresh.
    async fn query<F, T>(&self, f: F) -> McpResult<T>
    where
        F: FnOnce(&std::path::Path) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let cache = self.db_cache().await?;
        let path = cache.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let _keep = cache;
            f(&path)
        })
        .await
        .map_err(|e| McpError::internal(e.to_string()))?
        .map_err(|e| McpError::internal(format!("Database error: {e}")))
    }

    fn budget_writes(&self) -> McpResult<&BudgetApi> {
        self.state.budget_api.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                "Write tools require LIBERADO_BUDGET_API_URL (Liberado Budget REST, not SQLite)",
            )
        })
    }
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[turbomcp::server(name = "actual-budget-mcp", version = "0.1.0")]
impl ActualServer {
    #[tool("List all accounts with their current balances")]
    async fn list_accounts(&self) -> McpResult<String> {
        json_result(&self.query(db::list_accounts).await?)
    }

    #[tool(
        "Get transactions for an account. account_id is optional (omit for all accounts). \
            start_date and end_date are optional ISO dates (YYYY-MM-DD). \
            limit caps the number returned (default 500, max 2000). \
            min_amount_cents and max_amount_cents filter by amount in cents \
            (e.g. max_amount_cents=-1 returns only expenses; amounts are negative for debits). \
            category filters by category name or id (exact match, case-insensitive). \
            payee filters by payee name (partial, case-insensitive). \
            notes filters by memo/notes text (partial, case-insensitive)."
    )]
    // Pre-existing: mirrors the 10 filter params of db::get_transactions.
    #[allow(clippy::too_many_arguments)]
    async fn get_transactions(
        &self,
        account_id: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
        limit: Option<i64>,
        min_amount_cents: Option<i64>,
        max_amount_cents: Option<i64>,
        category: Option<String>,
        payee: Option<String>,
        notes: Option<String>,
    ) -> McpResult<String> {
        let start = start_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "Invalid start_date '{d}'; expected YYYY-MM-DD"
                    ))
                })
            })
            .transpose()?;
        let end = end_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!("Invalid end_date '{d}'; expected YYYY-MM-DD"))
                })
            })
            .transpose()?;
        let limit = limit.unwrap_or(500).clamp(1, 2000);
        json_result(
            &self
                .query(move |p| {
                    db::get_transactions(
                        p,
                        account_id.as_deref(),
                        start,
                        end,
                        limit,
                        min_amount_cents,
                        max_amount_cents,
                        category.as_deref(),
                        payee.as_deref(),
                        notes.as_deref(),
                    )
                })
                .await?,
        )
    }

    #[tool("List all category groups and their categories")]
    async fn list_categories(&self) -> McpResult<String> {
        json_result(&self.query(db::list_categories).await?)
    }

    #[tool("List all payees")]
    async fn list_payees(&self) -> McpResult<String> {
        json_result(&self.query(db::list_payees).await?)
    }

    #[tool(
        "Get the budget and actual spending for each category in a month. \
            month format: YYYY-MM (e.g. 2024-03)"
    )]
    async fn get_budget_month(&self, month: String) -> McpResult<String> {
        let (start, end) = month_bounds(&month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid month '{month}'; expected YYYY-MM"))
        })?;
        json_result(
            &self
                .query(move |p| db::get_budget_month(p, &month, start, end))
                .await?,
        )
    }

    #[tool(
        "Summarize income, expenses, and net savings month by month. \
            start_month and end_month use YYYY-MM format (e.g. 2024-01 to 2024-12)."
    )]
    async fn monthly_summary(&self, start_month: String, end_month: String) -> McpResult<String> {
        let (start_date, _) = month_bounds(&start_month).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid start_month '{start_month}'; expected YYYY-MM"
            ))
        })?;
        let (_, end_date) = month_bounds(&end_month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid end_month '{end_month}'; expected YYYY-MM"))
        })?;
        json_result(
            &self
                .query(move |p| db::monthly_summary(p, start_date, end_date))
                .await?,
        )
    }

    #[tool(
        "Aggregate spending by category between two dates (YYYY-MM-DD). \
            Only expense transactions (negative amounts) are included."
    )]
    async fn spending_by_category(
        &self,
        start_date: String,
        end_date: String,
    ) -> McpResult<String> {
        let start = date_str_to_int(&start_date).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid start_date '{start_date}'; expected YYYY-MM-DD"
            ))
        })?;
        let end = date_str_to_int(&end_date).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid end_date '{end_date}'; expected YYYY-MM-DD"
            ))
        })?;
        json_result(
            &self
                .query(move |p| db::spending_by_category(p, start, end))
                .await?,
        )
    }

    #[tool(
        "Re-download the latest budget data from the Actual Budget server (server mode only). \
            In local mode this is a no-op and just confirms the file path."
    )]
    async fn refresh(&self) -> McpResult<String> {
        // Drop the cache entry; in-flight queries hold their own Arc so their
        // temp files stay alive until those tasks complete.
        {
            let mut w = self.state.cache.write().await;
            *w = None;
        }
        let cache = self.db_cache().await?;
        Ok(format!("Budget loaded from: {}", cache.db_path.display()))
    }

    #[tool("Return the net worth across all non-closed, on-budget accounts")]
    async fn net_worth(&self) -> McpResult<String> {
        let total = self.query(db::net_worth).await?;
        Ok(format!(
            "Net worth (on-budget accounts): {}",
            format_amount(total)
        ))
    }

    #[tool(
        "Get the month-by-month running balance for an account. \
            account_id is optional (omit to aggregate across all accounts, including off-budget). \
            start_month and end_month use YYYY-MM format. \
            balance_cents reflects the true cumulative balance from account opening, \
            not just from start_month. Months with no transactions are omitted."
    )]
    async fn balance_history(
        &self,
        account_id: Option<String>,
        start_month: String,
        end_month: String,
    ) -> McpResult<String> {
        let start_ym = month_to_ym(&start_month).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid start_month '{start_month}'; expected YYYY-MM"
            ))
        })?;
        let end_ym = month_to_ym(&end_month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid end_month '{end_month}'; expected YYYY-MM"))
        })?;
        json_result(
            &self
                .query(move |p| db::balance_history(p, account_id.as_deref(), start_ym, end_ym))
                .await?,
        )
    }

    #[tool(
        "List all transaction auto-categorisation rules. \
            Each rule has conditions (criteria to match transactions) and actions \
            (fields to set when matched). conditions and actions are JSON arrays. \
            stage is 'pre', 'post', or null (default/main execution order)."
    )]
    async fn get_rules(&self) -> McpResult<String> {
        json_result(&self.query(db::get_rules).await?)
    }

    #[tool(
        "Aggregate expense spending by payee between two dates (YYYY-MM-DD). \
            Only expense transactions (negative amounts) are included. \
            Split transactions are attributed to the payee on the parent row, \
            so each purchase is counted once. Results are ordered most-spent first."
    )]
    async fn spending_by_payee(&self, start_date: String, end_date: String) -> McpResult<String> {
        let start = date_str_to_int(&start_date).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid start_date '{start_date}'; expected YYYY-MM-DD"
            ))
        })?;
        let end = date_str_to_int(&end_date).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid end_date '{end_date}'; expected YYYY-MM-DD"
            ))
        })?;
        json_result(
            &self
                .query(move |p| db::spending_by_payee(p, start, end))
                .await?,
        )
    }

    #[tool(
        "Return transactions with no category or the reserved Uncategorized category. \
            Split parent rows are excluded because their NULL category is intentional — \
            the real categories live on their child rows. \
            account_id is optional (omit for all accounts). \
            start_date and end_date are optional ISO dates (YYYY-MM-DD). \
            limit caps the number returned (default 200, max 2000). \
            For live data from the Liberado Budget server, use budget_api_uncategorized."
    )]
    async fn uncategorized_transactions(
        &self,
        account_id: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
        limit: Option<i64>,
    ) -> McpResult<String> {
        let start = start_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "Invalid start_date '{d}'; expected YYYY-MM-DD"
                    ))
                })
            })
            .transpose()?;
        let end = end_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!("Invalid end_date '{d}'; expected YYYY-MM-DD"))
                })
            })
            .transpose()?;
        let limit = limit.unwrap_or(200).clamp(1, 2000);
        json_result(
            &self
                .query(move |p| {
                    db::uncategorized_transactions(p, account_id.as_deref(), start, end, limit)
                })
                .await?,
        )
    }

    #[tool(
        "Return recent transactions whose category name matches category_regex. \
            Plain text matches as case-insensitive substring; metacharacters are full regex. \
            account_id is optional (omit for all accounts). \
            start_date and end_date are optional ISO dates (YYYY-MM-DD). \
            limit caps the number returned (default 500, max 2000)."
    )]
    async fn transactions_by_category(
        &self,
        category_regex: String,
        account_id: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
        limit: Option<i64>,
    ) -> McpResult<String> {
        let start = start_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "Invalid start_date '{d}'; expected YYYY-MM-DD"
                    ))
                })
            })
            .transpose()?;
        let end = end_date
            .as_deref()
            .map(|d| {
                date_str_to_int(d).ok_or_else(|| {
                    McpError::invalid_params(format!("Invalid end_date '{d}'; expected YYYY-MM-DD"))
                })
            })
            .transpose()?;
        let limit = limit.unwrap_or(500).clamp(1, 2000);
        let regex = category_regex;
        json_result(
            &self
                .query(move |p| {
                    db::transactions_by_category_regex(
                        p,
                        &regex,
                        account_id.as_deref(),
                        start,
                        end,
                        limit,
                    )
                })
                .await?,
        )
    }

    #[tool(
        "Return uncategorized transactions from the Liberado Budget REST API \
            (requires LIBERADO_BUDGET_API_URL). Same filters as uncategorized_transactions \
            but reads live server data when SQLite may be stale."
    )]
    async fn budget_api_uncategorized(
        &self,
        account_id: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
        limit: Option<i64>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let v = api
            .get_uncategorized_transactions(
                account_id.as_deref(),
                start_date.as_deref(),
                end_date.as_deref(),
                limit,
            )
            .await
            .map_err(McpError::internal)?;
        json_result(&v)
    }

    #[tool(
        "Return transactions whose category name matches category_regex from the \
            Liberado Budget REST API (requires LIBERADO_BUDGET_API_URL). \
            Plain text matches as case-insensitive substring."
    )]
    async fn budget_api_transactions_by_category(
        &self,
        category_regex: String,
        account_id: Option<String>,
        start_date: Option<String>,
        end_date: Option<String>,
        limit: Option<i64>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let v = api
            .get_transactions_by_category_regex(
                &category_regex,
                account_id.as_deref(),
                start_date.as_deref(),
                end_date.as_deref(),
                limit,
            )
            .await
            .map_err(McpError::internal)?;
        json_result(&v)
    }

    #[tool(
        "Create an envelope category. kind is expense (default) or income. \
            group_name is optional (defaults to Expenses/Income)."
    )]
    async fn create_category(
        &self,
        name: String,
        kind: Option<String>,
        group_name: Option<String>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let id = api
            .create_category(&name, kind.as_deref(), group_name.as_deref())
            .await
            .map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "id": id, "name": name }))
    }

    #[tool("Update a category by id or name. Pass hidden=true to hide it.")]
    async fn update_category(
        &self,
        category: String,
        name: Option<String>,
        hidden: Option<bool>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let id = api
            .update_category(&category, name.as_deref(), hidden)
            .await
            .map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "updated": id }))
    }

    #[tool("Create a payee by display name.")]
    async fn create_payee(&self, name: String) -> McpResult<String> {
        let api = self.budget_writes()?;
        let id = api.create_payee(&name).await.map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "id": id, "name": name }))
    }

    #[tool("Rename a payee by id or current name. Transactions keep the same payee id.")]
    async fn rename_payee(&self, payee: String, new_name: String) -> McpResult<String> {
        let api = self.budget_writes()?;
        let id = api
            .rename_payee(&payee, &new_name)
            .await
            .map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "id": id, "name": new_name }))
    }

    #[tool("Set one transaction's category. category is id, name, or empty/null to clear.")]
    async fn set_transaction_category(
        &self,
        transaction_id: String,
        category: Option<String>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let cat = match category.as_deref() {
            None | Some("") | Some("null") => None,
            Some(c) => Some(c),
        };
        api.set_transaction_category(&transaction_id, cat)
            .await
            .map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "updated": transaction_id }))
    }

    #[tool(
        "Assign a category (id or name) to many transactions. \
            learn=true also creates a payee-regex rule from those payees."
    )]
    async fn categorize_transactions(
        &self,
        transaction_ids: Vec<String>,
        category: String,
        learn: Option<bool>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let v = api
            .categorize_transactions(&transaction_ids, &category, learn.unwrap_or(false))
            .await
            .map_err(McpError::internal)?;
        json_result(&v)
    }

    #[tool(
        "Set a transaction's payee by display name (creates or matches, like Actual payee_name)."
    )]
    async fn set_transaction_payee(
        &self,
        transaction_id: String,
        payee: String,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        api.set_transaction_payee(&transaction_id, &payee)
            .await
            .map_err(McpError::internal)?;
        json_result(&serde_json::json!({ "updated": transaction_id, "payee": payee }))
    }

    #[tool(
        "Create a rule: payee matches this regex (plain text = case-insensitive substring) \
            → set category (id or name). Auto-applies to uncategorized transactions; \
            response includes matched count. Idempotent: does not duplicate an equivalent rule."
    )]
    async fn create_payee_rule(&self, payee_regex: String, category: String) -> McpResult<String> {
        let api = self.budget_writes()?;
        let v = api
            .create_payee_rule(&payee_regex, &category)
            .await
            .map_err(McpError::internal)?;
        json_result(&v)
    }

    #[tool(
        "Apply auto-categorisation rules to transactions with no category or the \
            reserved Uncategorized category. Returns matched count."
    )]
    async fn apply_rules(
        &self,
        account_id: Option<String>,
        limit: Option<i64>,
    ) -> McpResult<String> {
        let api = self.budget_writes()?;
        let v = api
            .apply_rules(account_id.as_deref(), limit)
            .await
            .map_err(McpError::internal)?;
        json_result(&v)
    }

    #[tool(
        "Set one category's monthly envelope allocation via Liberado Budget REST. \
            category is id or name. month is YYYY-MM (e.g. 2026-09). \
            amount_cents is integer cents (100 = $1.00). \
            Income categories store their target as a negative value on the backend; \
            pass a positive amount_cents and readback via get_budget_month will be negative."
    )]
    async fn set_budget_amount(
        &self,
        month: String,
        category: String,
        amount_cents: i64,
    ) -> McpResult<String> {
        month_to_ym(&month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid month '{month}'; expected YYYY-MM"))
        })?;
        let api = self.budget_writes()?;
        let v = api
            .set_budget_amount(&month, &category, amount_cents)
            .await
            .map_err(budget_api_error)?;
        json_result(&v)
    }

    #[tool(
        "Set many category allocations for a month in one call via Liberado Budget REST. \
            month is YYYY-MM. allocations is a JSON array of \
            {category (id or name) or category_id, amount_cents}. \
            Income categories store their target as a negative value on the backend; \
            pass positive amount_cents and readback via get_budget_month will be negative."
    )]
    async fn set_budget_allocations(
        &self,
        month: String,
        allocations: Vec<serde_json::Value>,
    ) -> McpResult<String> {
        month_to_ym(&month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid month '{month}'; expected YYYY-MM"))
        })?;
        let api = self.budget_writes()?;
        let v = api
            .set_budget_allocations(&month, &allocations)
            .await
            .map_err(budget_api_error)?;
        json_result(&v)
    }

    #[tool(
        "Copy envelope allocations from from_month into month (YYYY-MM). \
            Copies budgeted amounts only; use rollover_budget to carry leftover balances."
    )]
    async fn copy_budget(&self, month: String, from_month: String) -> McpResult<String> {
        month_to_ym(&month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid month '{month}'; expected YYYY-MM"))
        })?;
        month_to_ym(&from_month).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid from_month '{from_month}'; expected YYYY-MM"
            ))
        })?;
        let api = self.budget_writes()?;
        let v = api
            .copy_budget(&month, &from_month)
            .await
            .map_err(budget_api_error)?;
        json_result(&v)
    }

    #[tool(
        "Rollover leftover envelope balances from from_month into month (YYYY-MM). \
            Remaining = budgeted + spent (spent is negative for expenses)."
    )]
    async fn rollover_budget(&self, month: String, from_month: String) -> McpResult<String> {
        month_to_ym(&month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid month '{month}'; expected YYYY-MM"))
        })?;
        month_to_ym(&from_month).ok_or_else(|| {
            McpError::invalid_params(format!(
                "Invalid from_month '{from_month}'; expected YYYY-MM"
            ))
        })?;
        let api = self.budget_writes()?;
        let v = api
            .rollover_budget(&month, &from_month)
            .await
            .map_err(budget_api_error)?;
        json_result(&v)
    }
}
