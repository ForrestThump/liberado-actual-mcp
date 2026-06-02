# liberado-actual-mcp

A Rust MCP server for [Actual Budget](https://actualbudget.org), built with [turbomcp](https://crates.io/crates/turbomcp).

Exposes your budget data as MCP tools so Claude (or any MCP client) can query accounts, transactions, categories, spending patterns, and more.

---

## Tools

| Tool | Description |
|---|---|
| `list_accounts` | All accounts with current balances |
| `get_transactions` | Transaction history; filter by account and/or date range |
| `list_categories` | Category groups and categories |
| `list_payees` | All payees |
| `get_budget_month` | Budgeted vs. actual spending by category for a month |
| `monthly_summary` | Income, expenses, net savings per month |
| `spending_by_category` | Expense totals by category for a date range |
| `net_worth` | Total balance across all on-budget accounts |
| `refresh` | Re-download latest budget data from server |

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
| `BIND_ADDR` | No | Enables HTTP transport on this address; binary defaults to STDIO when unset (Docker image sets `0.0.0.0:8000`) |
| `ACTUAL_CURRENCY_SYMBOL` | No | Currency symbol prefix used in display amounts (default: `$`) |

---

## License

MIT
