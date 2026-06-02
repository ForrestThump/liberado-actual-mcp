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
         LEFT JOIN payees p ON p.id = t.description AND p.tombstone = 0
         LEFT JOIN categories c ON c.id = t.category AND c.tombstone = 0
         WHERE t.tombstone = 0
           AND (t.is_child = 0 OR t.is_child IS NULL)
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
            category: row.get(4)?,
            notes: row.get(5)?,
            cleared: row.get::<_, i64>(6)? != 0,
            reconciled: row.get::<_, i64>(7)? != 0,
        })
    })?;
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

pub fn get_budget_month(path: &std::path::Path, month: &str) -> rusqlite::Result<BudgetMonth> {
    let (start, end) = match month_bounds(month) {
        Some(b) => b,
        None => {
            return Err(rusqlite::Error::InvalidParameterName(
                "Invalid month format; expected YYYY-MM".to_string(),
            ))
        }
    };

    let conn = open(path)?;

    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, COALESCE(cg.name, ''),
                COALESCE(zb.amount, 0) AS budgeted,
                COALESCE(ts.spent, 0) AS spent
         FROM categories c
         LEFT JOIN category_groups cg ON cg.id = c.cat_group AND cg.tombstone = 0
         LEFT JOIN zero_budgets zb
               ON zb.category = c.id
              AND (zb.month = ?1 OR zb.month = CAST(REPLACE(?1, '-', '') AS INTEGER))
         LEFT JOIN (
             SELECT category, SUM(amount) AS spent
             FROM transactions
             WHERE tombstone = 0
               AND (is_child = 0 OR is_child IS NULL)
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

pub fn monthly_summary(
    path: &std::path::Path,
    start_month: &str,
    end_month: &str,
) -> rusqlite::Result<Vec<MonthSummary>> {
    let (start_date, _) = match month_bounds(start_month) {
        Some(b) => b,
        None => {
            return Err(rusqlite::Error::InvalidParameterName(
                "Invalid start_month format; expected YYYY-MM".to_string(),
            ))
        }
    };
    let (_, end_date) = match month_bounds(end_month) {
        Some(b) => b,
        None => {
            return Err(rusqlite::Error::InvalidParameterName(
                "Invalid end_month format; expected YYYY-MM".to_string(),
            ))
        }
    };

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
           AND (t.is_child = 0 OR t.is_child IS NULL)
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
             CREATE TABLE transactions (
                id TEXT, acct TEXT,
                date INTEGER, amount INTEGER,
                description TEXT, notes TEXT, category TEXT,
                cleared INTEGER DEFAULT 0,
                reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0,
                is_child INTEGER DEFAULT 0
             );
             CREATE TABLE zero_budgets (
                id TEXT, month TEXT, category TEXT, amount INTEGER
             );

             -- Accounts
             INSERT INTO accounts VALUES ('acc1','Checking','checking',0,0,0,1);
             INSERT INTO accounts VALUES ('acc2','Savings','savings',0,0,0,2);
             -- Category groups + categories
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             INSERT INTO categories VALUES ('cat2','Utilities','grp1',0,0,0,2);
             -- Payees
             INSERT INTO payees VALUES ('pay1','Grocery Store',NULL,0);
             INSERT INTO payees VALUES ('pay2','Electric Co',NULL,0);
             -- Transactions (amounts in cents: 100 = $1.00)
             INSERT INTO transactions VALUES ('t1','acc1',20240115,-5000,'pay1','weekly shop','cat1',1,0,0,0);
             INSERT INTO transactions VALUES ('t2','acc1',20240120,-2000,'pay2',NULL,'cat2',1,0,0,0);
             INSERT INTO transactions VALUES ('t3','acc1',20240201,100000,NULL,'salary',NULL,1,0,0,0);
             INSERT INTO transactions VALUES ('t4','acc2',20240115,-3000,'pay1',NULL,'cat1',0,0,0,0);
             -- Budget for 2024-01
             INSERT INTO zero_budgets VALUES ('zb1','2024-01','cat1',60000);
             INSERT INTO zero_budgets VALUES ('zb2','2024-01','cat2',10000);",
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
        let txns = get_transactions(db.path(), None, None, None, 500).unwrap();
        // 4 non-child transactions inserted
        assert_eq!(txns.len(), 4);
    }

    #[test]
    fn get_transactions_filter_by_account() {
        let db = test_db();
        let txns = get_transactions(db.path(), Some("acc2"), None, None, 500).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].amount_cents, -3000);
    }

    #[test]
    fn get_transactions_filter_by_date() {
        let db = test_db();
        let txns =
            get_transactions(db.path(), None, Some(20240201), Some(20240228), 500).unwrap();
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
        let bm = get_budget_month(db.path(), "2024-01").unwrap();
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
        let summary = monthly_summary(db.path(), "2024-01", "2024-02").unwrap();
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
                tombstone INTEGER DEFAULT 0, is_child INTEGER DEFAULT 0
             );
             -- month column stores INTEGER values as Actual Budget does in production
             CREATE TABLE zero_budgets (
                id TEXT, month INTEGER, category TEXT, amount INTEGER
             );
             INSERT INTO category_groups VALUES ('grp1','Bills',0,0,0,1);
             INSERT INTO categories VALUES ('cat1','Groceries','grp1',0,0,0,1);
             INSERT INTO categories VALUES ('cat2','Utilities','grp1',0,0,0,2);
             INSERT INTO transactions VALUES
                 ('t1','acc1',20240115,-5000,'pay1','shop','cat1',1,0,0,0);
             INSERT INTO transactions VALUES
                 ('t2','acc1',20240120,-2000,'pay2',NULL,'cat2',1,0,0,0);
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
        let bm = get_budget_month(db.path(), "2024-01").unwrap();
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
        let txns = get_transactions(db.path(), Some(""), None, None, 500).unwrap();
        assert_eq!(txns.len(), 0, "empty account_id should match no accounts");
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
        let bm = get_budget_month(db.path(), "2024-01").unwrap();
        // Verify cents totals are present and consistent with display values
        assert_eq!(bm.total_budgeted_cents, bm.categories.iter().map(|c| c.budgeted_cents).sum::<i64>());
        assert_eq!(bm.total_spent_cents, bm.categories.iter().map(|c| c.spent_cents).sum::<i64>());
        assert_eq!(bm.total_balance_cents, bm.total_budgeted_cents + bm.total_spent_cents);
    }
}
