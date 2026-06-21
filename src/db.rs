use rusqlite::{Connection, OpenFlags, params};

use crate::models::*;

fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

pub fn list_accounts(path: &std::path::Path) -> rusqlite::Result<Vec<Account>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, COALESCE(a.type, 'other'), a.offbudget, a.closed,
                COALESCE(SUM(t.amount), 0) AS balance
         FROM accounts a
         LEFT JOIN transactions t
           ON t.acct = a.id
           AND t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
         WHERE a.tombstone = 0
         GROUP BY a.id, a.name, a.type, a.offbudget, a.closed, a.sort_order
         ORDER BY a.sort_order, a.name",
    )?;
    let rows = stmt.query_map([], |row| {
        let balance: i64 = row.get(5)?;
        Ok(Account {
            id: row.get(0)?,
            name: row.get(1)?,
            account_type: row.get(2)?,
            offbudget: row.get::<_, i64>(3)? != 0,
            closed: row.get::<_, i64>(4)? != 0,
            balance_cents: balance,
            balance_display: format_amount(balance),
        })
    })?;
    rows.collect()
}

pub fn get_transactions(
    path: &std::path::Path,
    account_id: Option<&str>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    limit: i64,
    min_amount: Option<i64>,
    max_amount: Option<i64>,
    category: Option<&str>,
    payee: Option<&str>,
    notes: Option<&str>,
) -> rusqlite::Result<Vec<Transaction>> {
    let conn = open(path)?;

    let mut stmt = conn.prepare(
        "SELECT t.id, t.date, t.amount,
                COALESCE(p.name, ''),
                COALESCE(c.name, ''),
                COALESCE(t.notes, ''),
                COALESCE(t.cleared, 0),
                COALESCE(t.reconciled, 0)
         FROM transactions t
         -- Resolve payees through payee_mapping so merged payees (whose
         -- description points at a tombstoned id) still show the surviving name.
         LEFT JOIN payee_mapping pm ON pm.id = t.description
         LEFT JOIN payees p ON p.id = COALESCE(pm.targetId, t.description) AND p.tombstone = 0
         LEFT JOIN categories c ON c.id = t.category AND c.tombstone = 0
         WHERE t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
           AND (?1 IS NULL OR t.acct = ?1)
           AND (?2 IS NULL OR t.date >= ?2)
           AND (?3 IS NULL OR t.date <= ?3)
           AND (?5 IS NULL OR t.amount >= ?5)
           AND (?6 IS NULL OR t.amount <= ?6)
           AND (?7 IS NULL OR c.id = ?7 OR LOWER(c.name) = LOWER(?7))
           AND (?8 IS NULL OR INSTR(LOWER(COALESCE(p.name, '')), LOWER(?8)) > 0)
           AND (?9 IS NULL OR INSTR(LOWER(COALESCE(t.notes, '')), LOWER(?9)) > 0)
         ORDER BY t.date DESC, t.id
         LIMIT ?4",
    )?;

    let rows = stmt.query_map(
        params![account_id, start_date, end_date, limit, min_amount, max_amount, category, payee, notes],
        |row| {
            let raw_date: i64 = row.get(1)?;
            let amount: i64 = row.get(2)?;
            Ok(Transaction {
                id: row.get(0)?,
                date: date_int_to_str(raw_date),
                amount_cents: amount,
                amount_display: format_amount(amount),
                payee: row.get(3)?,
                category: row.get(4)?,
                notes: row.get(5)?,
                cleared: row.get::<_, i64>(6)? != 0,
                reconciled: row.get::<_, i64>(7)? != 0,
            })
        },
    )?;
    rows.collect()
}

pub fn list_categories(path: &std::path::Path) -> rusqlite::Result<Vec<CategoryGroup>> {
    let conn = open(path)?;

    let mut grp_stmt = conn.prepare(
        "SELECT id, name, COALESCE(is_income, 0)
         FROM category_groups
         WHERE tombstone = 0
         ORDER BY sort_order, name",
    )?;
    let groups: Vec<(String, String, bool)> = grp_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut cat_stmt = conn.prepare(
        "SELECT id, name, COALESCE(is_income, 0), COALESCE(hidden, 0)
         FROM categories
         WHERE cat_group = ?1 AND tombstone = 0
         ORDER BY sort_order, name",
    )?;

    let mut result = Vec::new();
    for (gid, gname, is_income) in groups {
        let cats: Vec<Category> = cat_stmt
            .query_map([&gid], |row| {
                Ok(Category {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    is_income: row.get::<_, i64>(2)? != 0,
                    hidden: row.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        result.push(CategoryGroup {
            id: gid,
            name: gname,
            is_income,
            categories: cats,
        });
    }
    Ok(result)
}

pub fn list_payees(path: &std::path::Path) -> rusqlite::Result<Vec<Payee>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, transfer_acct
         FROM payees
         WHERE tombstone = 0
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Payee {
            id: row.get(0)?,
            name: row.get(1)?,
            transfer_account_id: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// `month` is "YYYY-MM" (for display and the YYYYMM budget-table lookup);
/// `start`/`end` are the YYYYMMDD bounds [start, end) for the month. The caller
/// is responsible for validating the month format and computing the bounds.
pub fn get_budget_month(
    path: &std::path::Path,
    month: &str,
    start: i64,
    end: i64,
) -> rusqlite::Result<BudgetMonth> {
    let conn = open(path)?;

    // Budget amounts live in zero_budgets (envelope) or reflect_budgets
    // (tracking) depending on the budget type; both tables always exist, and a
    // category appears in only one, so UNION ALL reads whichever is in use.
    // month is stored as an integer YYYYMM (e.g. 202401).
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, COALESCE(cg.name, ''),
                COALESCE(zb.amount, 0) AS budgeted,
                COALESCE(ts.spent, 0) AS spent
         FROM categories c
         LEFT JOIN category_groups cg ON cg.id = c.cat_group AND cg.tombstone = 0
         LEFT JOIN (
             SELECT category, month, amount FROM zero_budgets
             UNION ALL
             SELECT category, month, amount FROM reflect_budgets
         ) zb
               ON zb.category = c.id
              AND zb.month = CAST(REPLACE(?1, '-', '') AS INTEGER)
         LEFT JOIN (
             SELECT category, SUM(amount) AS spent
             FROM transactions
             WHERE tombstone = 0
               -- Split categories live on child rows; the parent holds the
               -- total with a NULL category. Exclude parents, keep children.
               AND (is_parent = 0 OR is_parent IS NULL)
               AND date >= ?2 AND date < ?3
             GROUP BY category
         ) ts ON ts.category = c.id
         WHERE c.tombstone = 0 AND COALESCE(c.hidden, 0) = 0
         ORDER BY cg.sort_order, c.sort_order, c.name",
    )?;

    let cats: Vec<BudgetCategory> = stmt
        .query_map(params![month, start, end], |row| {
            let budgeted: i64 = row.get(3)?;
            let spent: i64 = row.get(4)?;
            let balance = budgeted + spent; // spent is negative for expenses
            Ok(BudgetCategory {
                category_id: row.get(0)?,
                category_name: row.get(1)?,
                group_name: row.get(2)?,
                budgeted_cents: budgeted,
                budgeted_display: format_amount(budgeted),
                spent_cents: spent,
                spent_display: format_amount(spent),
                balance_cents: balance,
                balance_display: format_amount(balance),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total_budgeted: i64 = cats.iter().map(|c| c.budgeted_cents).sum();
    let total_spent: i64 = cats.iter().map(|c| c.spent_cents).sum();
    let total_balance = total_budgeted + total_spent;

    Ok(BudgetMonth {
        month: month.to_string(),
        categories: cats,
        total_budgeted_cents: total_budgeted,
        total_budgeted_display: format_amount(total_budgeted),
        total_spent_cents: total_spent,
        total_spent_display: format_amount(total_spent),
        total_balance_cents: total_balance,
        total_balance_display: format_amount(total_balance),
    })
}

/// `start_date`/`end_date` are YYYYMMDD bounds [start_date, end_date), i.e. the
/// first day of the start month through the first day of the month *after* the
/// end month. The caller validates the month strings and computes these bounds.
pub fn monthly_summary(
    path: &std::path::Path,
    start_date: i64,
    end_date: i64,
) -> rusqlite::Result<Vec<MonthSummary>> {
    let conn = open(path)?;

    // Group by month via integer arithmetic: YYYYMMDD / 100 = YYYYMM
    let mut stmt = conn.prepare(
        "SELECT (t.date / 100) AS ym,
                SUM(CASE WHEN t.amount > 0 THEN t.amount ELSE 0 END) AS income,
                SUM(CASE WHEN t.amount < 0 THEN t.amount ELSE 0 END) AS expenses
         FROM transactions t
         WHERE t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
           AND t.date >= ?1 AND t.date < ?2
         GROUP BY ym
         ORDER BY ym",
    )?;

    let rows = stmt.query_map(params![start_date, end_date], |row| {
        let ym: i64 = row.get(0)?;
        let y = ym / 100;
        let m = ym % 100;
        let month_str = format!("{y:04}-{m:02}");
        let income: i64 = row.get(1)?;
        let expenses: i64 = row.get(2)?;
        let net = income + expenses;
        Ok(MonthSummary {
            month: month_str,
            income_cents: income,
            income_display: format_amount(income),
            expenses_cents: expenses,
            expenses_display: format_amount(expenses),
            net_cents: net,
            net_display: format_amount(net),
        })
    })?;
    rows.collect()
}

pub fn spending_by_category(
    path: &std::path::Path,
    start_date: i64,
    end_date: i64,
) -> rusqlite::Result<Vec<CategorySpending>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(c.name, 'Uncategorized'),
                COALESCE(cg.name, ''),
                SUM(t.amount) AS total,
                COUNT(*) AS cnt
         FROM transactions t
         LEFT JOIN categories c ON c.id = t.category AND c.tombstone = 0
         LEFT JOIN category_groups cg ON cg.id = c.cat_group AND cg.tombstone = 0
         WHERE t.tombstone = 0
           -- Split categories live on child rows (parent holds the total with a
           -- NULL category); exclude parents and keep children for per-category sums.
           AND (t.is_parent = 0 OR t.is_parent IS NULL)
           AND t.amount < 0
           AND t.date >= ?1 AND t.date <= ?2
         GROUP BY c.id
         ORDER BY total ASC",
    )?;

    let rows = stmt.query_map(params![start_date, end_date], |row| {
        let total: i64 = row.get(2)?;
        Ok(CategorySpending {
            category_name: row.get(0)?,
            group_name: row.get(1)?,
            total_cents: total,
            total_display: format_amount(total),
            transaction_count: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// Aggregate spending (negative-amount transactions) by resolved payee name
/// between `start_date` and `end_date` (YYYYMMDD, inclusive).
/// Split transactions are counted via the parent row so each purchase is
/// attributed to its payee exactly once. Results are ordered most-spent first.
pub fn spending_by_payee(
    path: &std::path::Path,
    start_date: i64,
    end_date: i64,
) -> rusqlite::Result<Vec<PayeeSpending>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(p.name, 'No Payee') AS payee_name,
                SUM(t.amount) AS total,
                COUNT(*) AS cnt
         FROM transactions t
         LEFT JOIN payee_mapping pm ON pm.id = t.description
         LEFT JOIN payees p ON p.id = COALESCE(pm.targetId, t.description) AND p.tombstone = 0
         WHERE t.tombstone = 0
           -- Use non-child rows: regular transactions + split parents.
           -- This attributes the full split amount to its payee without
           -- double-counting via child rows.
           AND (t.is_child = 0 OR t.is_child IS NULL)
           AND t.amount < 0
           AND t.date >= ?1 AND t.date <= ?2
         GROUP BY COALESCE(pm.targetId, t.description)
         ORDER BY total ASC",
    )?;
    let rows = stmt.query_map(params![start_date, end_date], |row| {
        let total: i64 = row.get(1)?;
        Ok(PayeeSpending {
            payee_name: row.get(0)?,
            total_cents: total,
            total_display: format_amount(total),
            transaction_count: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// Return transactions that have no category assigned.
/// Split parent rows (is_parent=1) are excluded because their NULL category
/// is intentional — the real categories live on their child rows.
pub fn uncategorized_transactions(
    path: &std::path::Path,
    account_id: Option<&str>,
    start_date: Option<i64>,
    end_date: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<Transaction>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT t.id, t.date, t.amount,
                COALESCE(p.name, ''),
                COALESCE(t.notes, ''),
                COALESCE(t.cleared, 0),
                COALESCE(t.reconciled, 0)
         FROM transactions t
         LEFT JOIN payee_mapping pm ON pm.id = t.description
         LEFT JOIN payees p ON p.id = COALESCE(pm.targetId, t.description) AND p.tombstone = 0
         WHERE t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
           AND (t.is_parent = 0 OR t.is_parent IS NULL)
           AND t.category IS NULL
           AND (?1 IS NULL OR t.acct = ?1)
           AND (?2 IS NULL OR t.date >= ?2)
           AND (?3 IS NULL OR t.date <= ?3)
         ORDER BY t.date DESC, t.id
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![account_id, start_date, end_date, limit], |row| {
        let raw_date: i64 = row.get(1)?;
        let amount: i64 = row.get(2)?;
        Ok(Transaction {
            id: row.get(0)?,
            date: date_int_to_str(raw_date),
            amount_cents: amount,
            amount_display: format_amount(amount),
            payee: row.get(3)?,
            category: String::new(),
            notes: row.get(4)?,
            cleared: row.get::<_, i64>(5)? != 0,
            reconciled: row.get::<_, i64>(6)? != 0,
        })
    })?;
    rows.collect()
}

/// Returns the running end-of-month balance for an account (or all accounts when
/// `account_id` is None) for each month that has transactions in the range
/// [`start_ym`, `end_ym`] where values are YYYYMM integers.
///
/// The `balance_cents` column is the cumulative total from the beginning of the
/// account's history (not just the requested range) so it reflects the true
/// running balance. Months with no activity are omitted.
pub fn balance_history(
    path: &std::path::Path,
    account_id: Option<&str>,
    start_ym: i64,
    end_ym: i64,
) -> rusqlite::Result<Vec<BalanceHistoryEntry>> {
    let conn = open(path)?;
    // Include all history up to end_ym in the inner subquery so the window
    // function accumulates the correct opening balance before start_ym.
    // The window function must be computed over ALL historical months before
    // start_ym so the balance reflects the true opening balance, not just the
    // delta since start_ym. In SQL, WHERE is evaluated before window functions,
    // so we filter in an outer query AFTER the window function runs.
    let mut stmt = conn.prepare(
        "SELECT month, month_net, balance
         FROM (
             SELECT
                 printf('%04d-%02d', ym / 100, ym % 100) AS month,
                 ym,
                 month_net,
                 SUM(month_net) OVER (ORDER BY ym ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
                     AS balance
             FROM (
                 SELECT (t.date / 100) AS ym, SUM(t.amount) AS month_net
                 FROM transactions t
                 WHERE t.tombstone = 0
                   AND (t.is_child = 0 OR t.is_child IS NULL)
                   AND (?1 IS NULL OR t.acct = ?1)
                   AND t.date / 100 <= ?3
                 GROUP BY ym
             )
         )
         WHERE ym >= ?2
         ORDER BY ym",
    )?;
    let rows = stmt.query_map(params![account_id, start_ym, end_ym], |row| {
        let net: i64 = row.get(1)?;
        let balance: i64 = row.get(2)?;
        Ok(BalanceHistoryEntry {
            month: row.get(0)?,
            net_change_cents: net,
            net_change_display: format_amount(net),
            balance_cents: balance,
            balance_display: format_amount(balance),
        })
    })?;
    rows.collect()
}

/// List all transaction auto-categorisation rules stored in the budget.
/// `conditions` and `actions` are parsed from their stored JSON representation.
pub fn get_rules(path: &std::path::Path) -> rusqlite::Result<Vec<Rule>> {
    let conn = open(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, stage, COALESCE(conditions_op, 'and'), conditions, actions
         FROM rules
         WHERE tombstone = 0
         ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let conditions_str: String = row.get(3)?;
        let actions_str: String = row.get(4)?;
        let conditions = serde_json::from_str(&conditions_str).unwrap_or(serde_json::Value::Null);
        let actions = serde_json::from_str(&actions_str).unwrap_or(serde_json::Value::Null);
        Ok(Rule {
            id: row.get(0)?,
            stage: row.get(1)?,
            conditions_op: row.get(2)?,
            conditions,
            actions,
        })
    })?;
    rows.collect()
}

pub fn net_worth(path: &std::path::Path) -> rusqlite::Result<i64> {
    let conn = open(path)?;
    conn.query_row(
        "SELECT COALESCE(SUM(t.amount), 0)
         FROM transactions t
         JOIN accounts a ON a.id = t.acct AND a.tombstone = 0
         WHERE t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
           AND a.closed = 0
           AND a.offbudget = 0",
        [],
        |row| row.get(0),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::NamedTempFile;

    /// Build a minimal Actual Budget–shaped SQLite file for testing.
    fn test_db() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE accounts (
                id TEXT, name TEXT, type TEXT,
                offbudget INTEGER DEFAULT 0,
                closed INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE category_groups (
                id TEXT, name TEXT,
                is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE categories (
                id TEXT, name TEXT, cat_group TEXT,
                is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE payees (
                id TEXT, name TEXT,
                transfer_acct TEXT,
                tombstone INTEGER DEFAULT 0
             );
             CREATE TABLE payee_mapping (
                id TEXT, targetId TEXT
             );
             CREATE TABLE transactions (
                id TEXT, acct TEXT,
                date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0,
                reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0,
                is_child INTEGER DEFAULT 0,
                is_parent INTEGER DEFAULT 0
             );
             -- Both budget tables always exist; month is an INTEGER (YYYYMM).
             CREATE TABLE zero_budgets (
                id TEXT, month INTEGER, category TEXT, amount INTEGER
             );
             CREATE TABLE reflect_budgets (
                id TEXT, month INTEGER, category TEXT, amount INTEGER
             );

             -- Accounts
             INSERT INTO accounts VALUES ('acc1','Checking','checking',0,0,0,1);
             INSERT INTO accounts VALUES ('acc2','Savings','savings',0,0,0,2);
             -- Category groups + categories
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             INSERT INTO categories VALUES ('cat2','Utilities','grp1',0,0,0,2);
             -- Payees + self-mappings
             INSERT INTO payees VALUES ('pay1','Grocery Store',NULL,0);
             INSERT INTO payees VALUES ('pay2','Electric Co',NULL,0);
             INSERT INTO payee_mapping VALUES ('pay1','pay1');
             INSERT INTO payee_mapping VALUES ('pay2','pay2');
             -- Transactions (amounts in cents: 100 = $1.00); trailing cols: tombstone, is_child, is_parent
             INSERT INTO transactions VALUES ('t1','acc1',20240115,-5000,'pay1','weekly shop','cat1',1,0,0,0,0);
             INSERT INTO transactions VALUES ('t2','acc1',20240120,-2000,'pay2',NULL,'cat2',1,0,0,0,0);
             INSERT INTO transactions VALUES ('t3','acc1',20240201,100000,NULL,'salary',NULL,1,0,0,0,0);
             INSERT INTO transactions VALUES ('t4','acc2',20240115,-3000,'pay1',NULL,'cat1',0,0,0,0,0);
             -- Budget for 2024-01 (stored as integer 202401)
             INSERT INTO zero_budgets VALUES ('zb1',202401,'cat1',60000);
             INSERT INTO zero_budgets VALUES ('zb2',202401,'cat2',10000);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn list_accounts_returns_both() {
        let db = test_db();
        let accounts = list_accounts(db.path()).unwrap();
        assert_eq!(accounts.len(), 2);
        let checking = accounts.iter().find(|a| a.id == "acc1").unwrap();
        // balance = -5000 + -2000 + 100000 = 93000
        assert_eq!(checking.balance_cents, 93000);
        assert_eq!(checking.balance_display, "$930.00");
    }

    #[test]
    fn get_transactions_all_accounts() {
        let db = test_db();
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, None, None, None).unwrap();
        // 4 non-child transactions inserted
        assert_eq!(txns.len(), 4);
    }

    #[test]
    fn get_transactions_filter_by_account() {
        let db = test_db();
        let txns = get_transactions(db.path(), Some("acc2"), None, None, 500, None, None, None, None, None).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].amount_cents, -3000);
    }

    #[test]
    fn get_transactions_filter_by_date() {
        let db = test_db();
        let txns =
            get_transactions(db.path(), None, Some(20240201), Some(20240228), 500, None, None, None, None, None).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].date, "2024-02-01");
    }

    #[test]
    fn list_categories_groups_and_children() {
        let db = test_db();
        let groups = list_categories(db.path()).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Bills");
        assert_eq!(groups[0].categories.len(), 2);
    }

    #[test]
    fn list_payees_count() {
        let db = test_db();
        let payees = list_payees(db.path()).unwrap();
        assert_eq!(payees.len(), 2);
    }

    #[test]
    fn get_budget_month_totals() {
        let db = test_db();
        let (start, end) = month_bounds("2024-01").unwrap();
        let bm = get_budget_month(db.path(), "2024-01", start, end).unwrap();
        assert_eq!(bm.month, "2024-01");
        // Both categories have data
        assert_eq!(bm.categories.len(), 2);
        let groceries = bm.categories.iter().find(|c| c.category_name == "Groceries").unwrap();
        assert_eq!(groceries.budgeted_cents, 60000);
        // Spending: t1 (-5000) from acc1 + t4 (-3000) from acc2 = -8000
        assert_eq!(groceries.spent_cents, -8000);
    }

    #[test]
    fn monthly_summary_sums_correctly() {
        let db = test_db();
        let (start, _) = month_bounds("2024-01").unwrap();
        let (_, end) = month_bounds("2024-02").unwrap();
        let summary = monthly_summary(db.path(), start, end).unwrap();
        assert!(!summary.is_empty());
        let jan = summary.iter().find(|m| m.month == "2024-01").unwrap();
        assert_eq!(jan.income_cents, 0);
        // -5000 + -2000 + -3000 = -10000
        assert_eq!(jan.expenses_cents, -10000);
        let feb = summary.iter().find(|m| m.month == "2024-02").unwrap();
        assert_eq!(feb.income_cents, 100000);
    }

    /// Real Actual Budget databases store zero_budgets.month as an INTEGER (e.g. 202401).
    /// This test verifies the dual-format JOIN condition handles that correctly.
    fn test_db_int_months() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE category_groups (
                id TEXT, name TEXT, is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE categories (
                id TEXT, name TEXT, cat_group TEXT,
                is_income INTEGER DEFAULT 0, hidden INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0, sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE transactions (
                id TEXT, acct TEXT, date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0, reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0, is_child INTEGER DEFAULT 0,
                is_parent INTEGER DEFAULT 0
             );
             -- month column stores INTEGER values as Actual Budget does in production
             CREATE TABLE zero_budgets (
                id TEXT, month INTEGER, category TEXT, amount INTEGER
             );
             CREATE TABLE reflect_budgets (
                id TEXT, month INTEGER, category TEXT, amount INTEGER
             );
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             INSERT INTO categories VALUES ('cat2','Utilities','grp1',0,0,0,2);
             INSERT INTO transactions VALUES
                 ('t1','acc1',20240115,-5000,'pay1','shop','cat1',1,0,0,0,0);
             INSERT INTO transactions VALUES
                 ('t2','acc1',20240120,-2000,'pay2',NULL,'cat2',1,0,0,0,0);
             INSERT INTO zero_budgets VALUES ('zb1',202401,'cat1',60000);
             INSERT INTO zero_budgets VALUES ('zb2',202401,'cat2',10000);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn get_budget_month_with_integer_month_storage() {
        let db = test_db_int_months();
        let (start, end) = month_bounds("2024-01").unwrap();
        let bm = get_budget_month(db.path(), "2024-01", start, end).unwrap();
        assert_eq!(bm.month, "2024-01");
        assert_eq!(bm.categories.len(), 2);
        let groceries = bm
            .categories
            .iter()
            .find(|c| c.category_name == "Groceries")
            .unwrap();
        assert_eq!(groceries.budgeted_cents, 60000);
        assert_eq!(groceries.spent_cents, -5000);
    }

    #[test]
    fn spending_by_category_expenses_only() {
        let db = test_db();
        let spending = spending_by_category(db.path(), 20240101, 20240131).unwrap();
        // Only expenses (negative), both in Jan
        assert_eq!(spending.len(), 2);
        for s in &spending {
            assert!(s.total_cents < 0, "expected negative total");
        }
    }

    #[test]
    fn get_transactions_empty_account_id_returns_zero_not_all() {
        // Previously "" was a sentinel that disabled the account filter, returning
        // all transactions. Now it's treated as a literal (non-matching) value.
        let db = test_db();
        let txns = get_transactions(db.path(), Some(""), None, None, 500, None, None, None, None, None).unwrap();
        assert_eq!(txns.len(), 0, "empty account_id should match no accounts");
    }

    #[test]
    fn get_transactions_filter_by_min_amount() {
        let db = test_db();
        // min -2000 includes t2 (-2000) and t3 (100000) but not t1 (-5000) or t4 (-3000)
        let txns = get_transactions(db.path(), None, None, None, 500, Some(-2000), None, None, None, None).unwrap();
        assert_eq!(txns.len(), 2);
        assert!(txns.iter().all(|t| t.amount_cents >= -2000));
    }

    #[test]
    fn get_transactions_filter_by_max_amount() {
        let db = test_db();
        // max -2000 includes only the expenses <= -2000: t1 (-5000), t2 (-2000), t4 (-3000)
        let txns = get_transactions(db.path(), None, None, None, 500, None, Some(-2000), None, None, None).unwrap();
        assert_eq!(txns.len(), 3);
        assert!(txns.iter().all(|t| t.amount_cents <= -2000));
    }

    #[test]
    fn get_transactions_filter_by_category_name() {
        let db = test_db();
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, Some("Groceries"), None, None).unwrap();
        assert_eq!(txns.len(), 2); // t1 and t4
        assert!(txns.iter().all(|t| t.category == "Groceries"));
    }

    #[test]
    fn get_transactions_filter_by_category_id() {
        let db = test_db();
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, Some("cat1"), None, None).unwrap();
        assert_eq!(txns.len(), 2); // t1 and t4
    }

    #[test]
    fn get_transactions_filter_by_payee_partial() {
        let db = test_db();
        // "Grocery" matches "Grocery Store"
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, None, Some("Grocery"), None).unwrap();
        assert_eq!(txns.len(), 2); // t1 and t4

        // "Electric" matches "Electric Co"
        let txns2 = get_transactions(db.path(), None, None, None, 500, None, None, None, Some("Electric"), None).unwrap();
        assert_eq!(txns2.len(), 1); // t2
    }

    #[test]
    fn net_worth_on_budget_accounts_only() {
        // acc1 (on-budget): -5000 + -2000 + 100000 = 93000
        // acc2 (on-budget): -3000
        // net worth = 93000 + (-3000) = 90000
        let db = test_db();
        let nw = net_worth(db.path()).unwrap();
        assert_eq!(nw, 90000);
    }

    #[test]
    fn get_budget_month_includes_cents_totals() {
        let db = test_db();
        let (start, end) = month_bounds("2024-01").unwrap();
        let bm = get_budget_month(db.path(), "2024-01", start, end).unwrap();
        // Verify cents totals are present and consistent with display values
        assert_eq!(bm.total_budgeted_cents, bm.categories.iter().map(|c| c.budgeted_cents).sum::<i64>());
        assert_eq!(bm.total_spent_cents, bm.categories.iter().map(|c| c.spent_cents).sum::<i64>());
        assert_eq!(bm.total_balance_cents, bm.total_budgeted_cents + bm.total_spent_cents);
    }

    /// A budget with one normal transaction plus a split (parent + two children).
    /// Mirrors Actual: the parent (is_parent=1) holds the total with a NULL
    /// category; each child (is_child=1) carries its own category and amount.
    fn test_db_splits() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE accounts (
                id TEXT, name TEXT, type TEXT, offbudget INTEGER DEFAULT 0,
                closed INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE category_groups (
                id TEXT, name TEXT, is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE categories (
                id TEXT, name TEXT, cat_group TEXT, is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE transactions (
                id TEXT, acct TEXT, date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0, reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0, is_child INTEGER DEFAULT 0,
                is_parent INTEGER DEFAULT 0
             );
             CREATE TABLE payees (id TEXT, name TEXT, transfer_acct TEXT, tombstone INTEGER DEFAULT 0);
             CREATE TABLE payee_mapping (id TEXT, targetId TEXT);
             CREATE TABLE zero_budgets (id TEXT, month INTEGER, category TEXT, amount INTEGER);
             CREATE TABLE reflect_budgets (id TEXT, month INTEGER, category TEXT, amount INTEGER);

             INSERT INTO accounts VALUES ('acc1','Checking','checking',0,0,0,1);
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             INSERT INTO categories VALUES ('cat2','Utilities','grp1',0,0,0,2);
             -- trailing cols: tombstone, is_child, is_parent
             -- Normal transaction
             INSERT INTO transactions VALUES ('n1','acc1',20240115,-1000,NULL,NULL,'cat1',1,0,0,0,0);
             -- Split: parent total -5000, NULL category, is_parent=1
             INSERT INTO transactions VALUES ('p1','acc1',20240116,-5000,NULL,NULL,NULL,1,0,0,0,1);
             -- Split children carry the categorized amounts, is_child=1
             INSERT INTO transactions VALUES ('c1','acc1',20240116,-2000,NULL,NULL,'cat1',1,0,0,1,0);
             INSERT INTO transactions VALUES ('c2','acc1',20240116,-3000,NULL,NULL,'cat2',1,0,0,1,0);
             INSERT INTO zero_budgets VALUES ('zb1',202401,'cat1',60000);
             INSERT INTO zero_budgets VALUES ('zb2',202401,'cat2',10000);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn spending_by_category_counts_split_children_not_parent() {
        let db = test_db_splits();
        let spending = spending_by_category(db.path(), 20240101, 20240131).unwrap();
        // Categorized spend lives on children + normal rows; the split parent
        // (NULL category, -5000) must NOT appear as an "Uncategorized" row.
        assert!(
            !spending.iter().any(|s| s.category_name == "Uncategorized"),
            "split parent should be excluded, not surface as Uncategorized"
        );
        let groceries = spending.iter().find(|s| s.category_name == "Groceries").unwrap();
        // n1 (-1000) + c1 (-2000)
        assert_eq!(groceries.total_cents, -3000);
        let utilities = spending.iter().find(|s| s.category_name == "Utilities").unwrap();
        // c2 (-3000)
        assert_eq!(utilities.total_cents, -3000);
    }

    #[test]
    fn get_budget_month_counts_split_children() {
        let db = test_db_splits();
        let (start, end) = month_bounds("2024-01").unwrap();
        let bm = get_budget_month(db.path(), "2024-01", start, end).unwrap();
        let groceries = bm.categories.iter().find(|c| c.category_name == "Groceries").unwrap();
        assert_eq!(groceries.spent_cents, -3000); // n1 + c1
        let utilities = bm.categories.iter().find(|c| c.category_name == "Utilities").unwrap();
        assert_eq!(utilities.spent_cents, -3000); // c2
    }

    #[test]
    fn balances_count_split_parent_not_children() {
        // Account balance / net worth must count the parent total once and skip
        // the children, to avoid double-counting the split.
        let db = test_db_splits();
        let accounts = list_accounts(db.path()).unwrap();
        let acc1 = accounts.iter().find(|a| a.id == "acc1").unwrap();
        // n1 (-1000) + parent p1 (-5000); children excluded.
        assert_eq!(acc1.balance_cents, -6000);
        assert_eq!(net_worth(db.path()).unwrap(), -6000);
    }

    /// Tracking budgets store amounts in reflect_budgets rather than zero_budgets.
    fn test_db_tracking_budget() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE category_groups (
                id TEXT, name TEXT, is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE categories (
                id TEXT, name TEXT, cat_group TEXT, is_income INTEGER DEFAULT 0,
                hidden INTEGER DEFAULT 0, tombstone INTEGER DEFAULT 0,
                sort_order INTEGER DEFAULT 0
             );
             CREATE TABLE transactions (
                id TEXT, acct TEXT, date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0, reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0, is_child INTEGER DEFAULT 0,
                is_parent INTEGER DEFAULT 0
             );
             CREATE TABLE zero_budgets (id TEXT, month INTEGER, category TEXT, amount INTEGER);
             CREATE TABLE reflect_budgets (id TEXT, month INTEGER, category TEXT, amount INTEGER);
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             -- Budget lives only in reflect_budgets (tracking mode).
             INSERT INTO reflect_budgets VALUES ('rb1',202401,'cat1',45000);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn get_budget_month_reads_tracking_budget_table() {
        let db = test_db_tracking_budget();
        let (start, end) = month_bounds("2024-01").unwrap();
        let bm = get_budget_month(db.path(), "2024-01", start, end).unwrap();
        let groceries = bm.categories.iter().find(|c| c.category_name == "Groceries").unwrap();
        assert_eq!(groceries.budgeted_cents, 45000);
    }

    /// A transaction whose description points at a merged-away payee should
    /// resolve to the surviving payee's name via payee_mapping.
    fn test_db_merged_payee() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE payees (id TEXT, name TEXT, transfer_acct TEXT, tombstone INTEGER DEFAULT 0);
             CREATE TABLE payee_mapping (id TEXT, targetId TEXT);
             CREATE TABLE categories (id TEXT, name TEXT, cat_group TEXT, tombstone INTEGER DEFAULT 0);
             CREATE TABLE transactions (
                id TEXT, acct TEXT, date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0, reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0, is_child INTEGER DEFAULT 0,
                is_parent INTEGER DEFAULT 0
             );
             -- 'old' was merged into 'keep' and tombstoned; mapping redirects it.
             INSERT INTO payees VALUES ('keep','Grocery Store',NULL,0);
             INSERT INTO payees VALUES ('old','Old Grocery',NULL,1);
             INSERT INTO payee_mapping VALUES ('keep','keep');
             INSERT INTO payee_mapping VALUES ('old','keep');
             INSERT INTO transactions VALUES ('t1','acc1',20240115,-5000,'old',NULL,NULL,1,0,0,0,0);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn get_transactions_resolves_merged_payee() {
        let db = test_db_merged_payee();
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, None, None, None).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].payee, "Grocery Store");
    }

    #[test]
    fn balance_history_running_balance_correct() {
        // test_db has:
        //   acc1 Jan 2024: -5000 + -2000 = -7000 net; running = -7000
        //   acc1 Feb 2024: +100000 net; running = 93000
        //   acc2 Jan 2024: -3000 net; running = -3000
        // All accounts Jan: -7000 + -3000 = -10000; Feb: running = 90000
        let db = test_db();

        // Single account history
        let history = balance_history(db.path(), Some("acc1"), 202401, 202402).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].month, "2024-01");
        assert_eq!(history[0].net_change_cents, -7000);
        assert_eq!(history[0].balance_cents, -7000);
        assert_eq!(history[1].month, "2024-02");
        assert_eq!(history[1].net_change_cents, 100000);
        assert_eq!(history[1].balance_cents, 93000);

        // All accounts (no filter)
        let all = balance_history(db.path(), None, 202401, 202402).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].balance_cents, -10000); // Jan: -7000 + -3000
        assert_eq!(all[1].balance_cents, 90000);  // Feb: -10000 + 100000
    }

    #[test]
    fn balance_history_start_ym_excludes_earlier_months() {
        // Requesting only Feb should still show the correct cumulative balance
        // (i.e. the Jan transactions are counted in the running total even though
        // Jan is not in the output range).
        let db = test_db();
        let history = balance_history(db.path(), Some("acc1"), 202402, 202402).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].month, "2024-02");
        assert_eq!(history[0].balance_cents, 93000); // includes Jan's -7000
    }

    fn test_db_rules() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("temp file");
        let conn = Connection::open(tmp.path()).expect("open");
        conn.execute_batch(
            "CREATE TABLE rules (
                id TEXT PRIMARY KEY,
                stage TEXT,
                conditions_op TEXT DEFAULT 'and',
                conditions TEXT DEFAULT '[]',
                actions TEXT DEFAULT '[]',
                tombstone INTEGER DEFAULT 0
             );
             INSERT INTO rules VALUES (
                 'rule1', 'pre', 'and',
                 '[{\"field\":\"payee\",\"op\":\"is\",\"value\":\"pay1\"}]',
                 '[{\"op\":\"set\",\"field\":\"category\",\"value\":\"cat1\"}]',
                 0
             );
             INSERT INTO rules VALUES (
                 'rule2', NULL, 'or',
                 '[{\"field\":\"notes\",\"op\":\"contains\",\"value\":\"salary\"}]',
                 '[{\"op\":\"set\",\"field\":\"category\",\"value\":\"income-cat\"}]',
                 0
             );
             -- tombstoned rule should be excluded
             INSERT INTO rules VALUES ('rule3', NULL, 'and', '[]', '[]', 1);",
        )
        .expect("schema");
        drop(conn);
        tmp
    }

    #[test]
    fn get_rules_returns_active_rules_only() {
        let db = test_db_rules();
        let rules = get_rules(db.path()).unwrap();
        assert_eq!(rules.len(), 2, "tombstoned rule must be excluded");
        let pre_rule = rules.iter().find(|r| r.id == "rule1").unwrap();
        assert_eq!(pre_rule.stage, Some("pre".to_string()));
        assert_eq!(pre_rule.conditions_op, "and");
        assert!(pre_rule.conditions.is_array());
        assert!(pre_rule.actions.is_array());

        let null_stage_rule = rules.iter().find(|r| r.id == "rule2").unwrap();
        assert_eq!(null_stage_rule.stage, None);
        assert_eq!(null_stage_rule.conditions_op, "or");
    }

    #[test]
    fn get_transactions_filter_by_notes() {
        let db = test_db();
        // t1 has notes "weekly shop", t3 has notes "salary"; t2 and t4 have no notes
        let txns = get_transactions(db.path(), None, None, None, 500, None, None, None, None, Some("shop")).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].id, "t1");

        let txns2 = get_transactions(db.path(), None, None, None, 500, None, None, None, None, Some("SALARY")).unwrap();
        assert_eq!(txns2.len(), 1);
        assert_eq!(txns2[0].id, "t3");

        // Non-matching notes returns empty
        let txns3 = get_transactions(db.path(), None, None, None, 500, None, None, None, None, Some("zzz")).unwrap();
        assert_eq!(txns3.len(), 0);
    }

    #[test]
    fn spending_by_payee_sums_and_orders() {
        let db = test_db();
        // Jan: t1 pay1 -5000, t2 pay2 -2000, t4 pay1 -3000; t3 is income (+100000), skipped
        let spending = spending_by_payee(db.path(), 20240101, 20240131).unwrap();
        assert_eq!(spending.len(), 2);
        // Ordered most-spent first (most negative total first)
        let grocery = spending.iter().find(|s| s.payee_name == "Grocery Store").unwrap();
        assert_eq!(grocery.total_cents, -8000); // t1 + t4
        assert_eq!(grocery.transaction_count, 2);
        let electric = spending.iter().find(|s| s.payee_name == "Electric Co").unwrap();
        assert_eq!(electric.total_cents, -2000);
    }

    #[test]
    fn uncategorized_transactions_excludes_split_parents() {
        let db = test_db_splits();
        // p1 is a split parent (is_parent=1, category=NULL) — must be excluded
        // n1 has category cat1 — not uncategorized
        // c1 and c2 are children (is_child=1) — excluded
        // No remaining regular uncategorized rows → empty result
        let txns = uncategorized_transactions(db.path(), None, None, None, 500).unwrap();
        assert_eq!(txns.len(), 0, "split parent with NULL category must not appear");
    }

    #[test]
    fn uncategorized_transactions_returns_uncategorized() {
        // t3 (salary) has no category; t1, t2, t4 are categorized
        let db = test_db();
        let txns = uncategorized_transactions(db.path(), None, None, None, 500).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].id, "t3");
        assert_eq!(txns[0].category, "");
    }

    #[test]
    fn uncategorized_transactions_filters_by_account() {
        let db = test_db();
        // acc2 only has t4 which IS categorized; no uncategorized transactions in acc2
        let txns = uncategorized_transactions(db.path(), Some("acc2"), None, None, 500).unwrap();
        assert_eq!(txns.len(), 0);
    }
}
