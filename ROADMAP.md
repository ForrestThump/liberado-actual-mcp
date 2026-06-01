# Roadmap

## v1 — Read-only (current)

- `list_accounts` — all accounts with current balance
- `get_transactions` — transaction history with optional account/date filters
- `list_categories` — category groups and categories
- `list_payees` — all payees
- `get_budget_month` — budgeted vs. actual by category for a month
- `monthly_summary` — income, expenses, net per month over a range
- `spending_by_category` — expense totals by category over a date range
- `refresh` — re-download budget from server
- `net_worth` — total balance across all on-budget accounts

**Access modes:**
- **Local mode**: `ACTUAL_DB_PATH` → read an already-synced `db.sqlite` directly
- **Server download mode**: `ACTUAL_SERVER_URL` + credentials → download the SQLite file from the server (requires unencrypted budget in simple-sync format)

---

## v2 — Write support

Actual Budget uses a **CRDT-based sync protocol** (protobuf messages over `POST /sync`).
To support writes the server must generate and submit valid sync messages.

### Planned tools
- `create_transaction` — add a new transaction; syncs back to server
- `update_transaction` — modify payee, category, amount, notes, cleared status
- `delete_transaction` — mark a transaction as deleted
- `import_transactions` — bulk import with deduplication (mirrors `api.importTransactions`)
- `create_category` / `update_category` / `delete_category`
- `create_payee` / `update_payee` / `delete_payee`
- `create_rule` / `update_rule` / `delete_rule` — transaction auto-categorisation rules
- `run_bank_sync` — trigger GoCardless / SimpleFIN bank sync

### Implementation notes
- Parse the protobuf schema from `@actual-app/api`'s `src/server-listen.ts`
- Implement `SyncRequest` / `SyncResponse` message types with `prost`
- Each write generates one or more `Message { dataset, row, column, value, timestamp }` ops
- Timestamps use Hybrid Logical Clocks (HLC) — see `src/server-listen.ts` in actual-budget/actual
- After applying ops locally, POST them to `{server}/sync`

---

## v3 — Encryption support

Actual Budget supports AES-256-GCM end-to-end encryption for budgets.

### Planned work
- Accept `ACTUAL_ENCRYPTION_PASSWORD` env var
- Derive the budget key from the password using PBKDF2 (same parameters as the JS client)
- Decrypt the downloaded budget file before opening with rusqlite
- Support encrypted writes by encrypting outgoing sync messages

### Implementation notes
- The key derivation and encryption logic lives in `packages/loot-core/src/server/encryption.ts`
- Use `ring` or `aes-gcm` crates for the crypto primitives

---

## v4 — Full CRDT sync (replace simple-sync restriction)

Currently server-download mode only works when Actual Budget uses the legacy "simple sync" storage
(entire SQLite sent as a blob). Most active installations use the full CRDT sync.

### Planned work
- Implement incremental CRDT download: `POST /sync` with `since: 0` to fetch all messages
- Replay messages onto a fresh local SQLite using rusqlite
- Cache the replayed database and apply incremental updates on subsequent syncs
- This removes the `ACTUAL_DB_PATH` workaround for most users
