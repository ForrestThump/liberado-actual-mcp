use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;
use turbomcp::prelude::*;
use secrecy::{ExposeSecret, SecretString};

use crate::{
    actual::{find_budget_file, ActualClient, SQLITE_MAGIC},
    db,
    models::{date_str_to_int, format_amount, month_bounds},
};

fn json_result<T: serde::Serialize>(val: &T) -> McpResult<String> {
    serde_json::to_string_pretty(val).map_err(|e| McpError::internal(e.to_string()))
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
            BudgetSource::Server { client, password, budget_id } => {
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
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[turbomcp::server(name = "actual-budget-mcp", version = "0.1.0")]
impl ActualServer {
    #[tool("List all accounts with their current balances")]
    async fn list_accounts(&self) -> McpResult<String> {
        json_result(&self.query(db::list_accounts).await?)
    }

    #[tool("Get transactions for an account. account_id is optional (omit for all accounts). \
            start_date and end_date are optional ISO dates (YYYY-MM-DD). \
            limit caps the number returned (default 500, max 2000).")]
    async fn get_transactions(
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
                    McpError::invalid_params(format!("Invalid start_date '{d}'; expected YYYY-MM-DD"))
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
                .query(move |p| db::get_transactions(p, account_id.as_deref(), start, end, limit))
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

    #[tool("Get the budget and actual spending for each category in a month. \
            month format: YYYY-MM (e.g. 2024-03)")]
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

    #[tool("Summarize income, expenses, and net savings month by month. \
            start_month and end_month use YYYY-MM format (e.g. 2024-01 to 2024-12).")]
    async fn monthly_summary(
        &self,
        start_month: String,
        end_month: String,
    ) -> McpResult<String> {
        let (start_date, _) = month_bounds(&start_month).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid start_month '{start_month}'; expected YYYY-MM"))
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

    #[tool("Aggregate spending by category between two dates (YYYY-MM-DD). \
            Only expense transactions (negative amounts) are included.")]
    async fn spending_by_category(
        &self,
        start_date: String,
        end_date: String,
    ) -> McpResult<String> {
        let start = date_str_to_int(&start_date).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid start_date '{start_date}'; expected YYYY-MM-DD"))
        })?;
        let end = date_str_to_int(&end_date).ok_or_else(|| {
            McpError::invalid_params(format!("Invalid end_date '{end_date}'; expected YYYY-MM-DD"))
        })?;
        json_result(&self.query(move |p| db::spending_by_category(p, start, end)).await?)
    }

    #[tool("Re-download the latest budget data from the Actual Budget server (server mode only). \
            In local mode this is a no-op and just confirms the file path.")]
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
        Ok(format!("Net worth (on-budget accounts): {}", format_amount(total)))
    }
}
