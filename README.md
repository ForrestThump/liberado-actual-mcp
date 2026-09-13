# liberado-actual-mcp

A Rust MCP server for [Actual Budget](https://actualbudget.org), built with [turbomcp](https://crates.io/crates/turbomcp).

Exposes your budget data as MCP tools so Claude (or any MCP client) can query accounts, transactions, categories, spending patterns, and more.

---

## Tools

### Read tools

| Tool | Description |
|---|---|
| `list_accounts` | All accounts with current balances |
| `get_transactions` | Transaction history with optional filters (see below) |
| `list_categories` | Category groups and categories |
| `list_payees` | All payees |
| `get_budget_month` | Budgeted vs. actual spending by category for a month |
| `monthly_summary` | Income, expenses, net savings per month over a range |
| `spending_by_category` | Expense totals grouped by category for a date range |
| `spending_by_payee` | Expense totals grouped by payee for a date range |
| `uncategorized_transactions` | Transactions with no category or reserved Uncategorized (excludes split parents) |
| `transactions_by_category` | Newest transactions whose category name matches a regex |
| `budget_api_uncategorized` | Same as uncategorized_transactions, live from Liberado Budget REST |
| `budget_api_transactions_by_category` | Category-regex transactions from Liberado Budget REST |
| `balance_history` | Month-by-month running account balance |
| `net_worth` | Total balance across all on-budget accounts |
| `get_rules` | Auto-categorisation rules with parsed conditions and actions |
| `refresh` | Re-download latest budget data from server (server mode only) |

### Write tools

Write tools require `LIBERADO_BUDGET_API_URL` pointing at a running [Liberado Budget](https://github.com/ForrestThump/liberado-budget) server. They modify budget data via REST rather than writing to the SQLite file directly.

| Tool | Description |
|---|---|
| `create_category` | Create an envelope category |
| `update_category` | Rename or hide a category |
| `create_payee` | Create a payee by display name |
| `rename_payee` | Rename a payee |
| `set_transaction_category` | Set one transaction's category |
| `set_transaction_payee` | Set one transaction's payee |
| `categorize_transactions` | Bulk-assign category; optional learn-from-payees |
| `create_payee_rule` | Create a payee-regex auto-categorisation rule |
| `apply_rules` | Re-run rules on uncategorized transactions |
| `set_budget_amount` | Set one category's monthly envelope allocation (`month`, `category` id or name, `amount_cents`) |
| `set_budget_allocations` | Set many allocations for a month (`[{category, amount_cents}, ...]`) |
| `copy_budget` | Copy allocations from `from_month` into `month` |
| `rollover_budget` | Carry leftover envelope balances from `from_month` into `month` |

Rules use `regex` by default (plain text matches as case-insensitive substring). Imports with no matching rule are assigned to the reserved **Uncategorized** category.

Envelope amounts are integer cents. **Income categories store their target as a
negative value on the backend** (so `balance = budgeted + actual` holds with
Actual's sign convention). Pass a *positive* `amount_cents` to these tools; a
later readback via `get_budget_month` shows the negative target. Backend
rejections (e.g. unknown category, invalid month) are returned as MCP
invalid-params errors. Examples:

```json
{ "month": "2026-09", "category": "Groceries", "amount_cents": 50000 }
```

```json
{
  "month": "2026-09",
  "allocations": [
    { "category": "Groceries", "amount_cents": 50000 },
    { "category_id": "c-rent", "amount_cents": 150000 }
  ]
}
```

```json
{ "month": "2026-10", "from_month": "2026-09" }
```

### `get_transactions` parameters

| Parameter | Type | Description |
|---|---|---|
| `account_id` | string? | Filter to one account (omit for all) |
| `start_date` | string? | Earliest date, `YYYY-MM-DD` |
| `end_date` | string? | Latest date, `YYYY-MM-DD` |
| `limit` | integer? | Max rows returned (default 500, max 2000) |
| `min_amount_cents` | integer? | Minimum amount in cents, inclusive (negative = expense; e.g. `-5000` = -$50.00) |
| `max_amount_cents` | integer? | Maximum amount in cents, inclusive |
| `category` | string? | Exact match by category name or id (case-insensitive) |
| `payee` | string? | Partial match on payee name (case-insensitive) |
| `notes` | string? | Partial match on memo/notes text (case-insensitive) |

---

## Access modes

### Local mode (recommended)

Point directly at the `db.sqlite` file that Actual Budget maintains locally.

```bash
ACTUAL_DB_PATH=~/.actual/<budget-sync-id>/db.sqlite
```

The file path depends on how you run Actual Budget:

| Client | Default location |
|---|---|
| Desktop app (Linux) | `~/.config/Actual/<sync-id>/db.sqlite` |
| Desktop app (macOS) | `~/Library/Application Support/Actual/<sync-id>/db.sqlite` |
| `@actual-app/api` | `$ACTUAL_DATA_DIR/<sync-id>/db.sqlite` (default `~/.actual/`) |

Run the Actual Budget desktop app or `@actual-app/api` at least once to sync the budget locally, then point `ACTUAL_DB_PATH` at the resulting file.

### Server download mode

Downloads the budget file directly from your Actual Budget server.

> **Limitation**: only works if your budget uses *simple sync* (unencrypted, legacy storage format). Most modern Actual Budget installations use the CRDT full-sync format — see [ROADMAP.md](ROADMAP.md) for v2/v4 plans.

```bash
ACTUAL_SERVER_URL=http://your-actual-server:5006
ACTUAL_PASSWORD=yourpassword
ACTUAL_BUDGET_ID=your-budget-sync-id   # optional; uses first budget if omitted
```

---

## Running

### From source

```bash
cargo build --release
ACTUAL_DB_PATH=~/.actual/<sync-id>/db.sqlite \
BIND_ADDR=0.0.0.0:8000 \
./target/release/liberado-actual-mcp
```

### Docker

```bash
docker build -t liberado-actual-mcp .
docker run -p 8000:8000 \
  -e ACTUAL_DB_PATH=/data/db.sqlite \
  -v ~/.actual/<sync-id>:/data:ro \
  liberado-actual-mcp
```

---

## Claude Desktop configuration

```json
{
  "mcpServers": {
    "actual-budget": {
      "command": "/path/to/liberado-actual-mcp",
      "env": {
        "ACTUAL_DB_PATH": "/home/you/.actual/<sync-id>/db.sqlite"
      }
    }
  }
}
```

Or for a running HTTP server:

```json
{
  "mcpServers": {
    "actual-budget": {
      "url": "http://localhost:8000/mcp"
    }
  }
}
```

---

## Environment variables

| Variable | Required | Description |
|---|---|---|
| `ACTUAL_DB_PATH` | Local mode | Direct path to the budget SQLite file |
| `ACTUAL_SERVER_URL` | Server mode | URL of your Actual Budget server |
| `ACTUAL_PASSWORD` | Server mode | Server password |
| `ACTUAL_BUDGET_ID` | Server mode | Budget sync ID (uses first if omitted) |
| `LIBERADO_BUDGET_API_URL` | Write tools | Base URL of Liberado Budget REST API (e.g. `http://127.0.0.1:8675`) |
| `BIND_ADDR` | No | Enables HTTP transport on this address; binary defaults to STDIO when unset (Docker image sets `0.0.0.0:8000`) |

---

## License

MIT
