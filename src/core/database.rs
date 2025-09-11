use sqlx::{Sqlite, SqlitePool, migrate::MigrateDatabase};

pub struct TxDatabase {
    db: SqlitePool,
}

impl TxDatabase {
    pub async fn new(db_url: &str) -> Self {
        if !Sqlite::database_exists(db_url).await.unwrap_or(false) {
            match Sqlite::create_database(db_url).await {
                Ok(_) => {}
                Err(error) => panic!("error: {}", error),
            }
        }
        TxDatabase {
            db: SqlitePool::connect(db_url).await.unwrap(),
        }
    }

    pub async fn create_tables(&self) {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS transactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                eth_sender TEXT NOT NULL,
                eth_signed_tx TEXT NOT NULL,
                eth_tx_hash TEXT NOT NULL,
                btc_commit_tx TEXT NOT NULL,
                btc_reveal_tx TEXT NOT NULL,
                btc_send_to_op_return_tx TEXT NOT NULL,
                total_fee INTEGER NOT NULL,
                nonce INTEGER NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_eth_sender ON transactions (eth_sender);
            "#,
        )
        .execute(&self.db)
        .await
        .expect("Failed to create transactions table");
    }

    pub async fn clear_tables(&self) {
        sqlx::query(
            r#"
            DROP TABLE IF EXISTS transactions;
            "#,
        )
        .execute(&self.db)
        .await
        .expect("Failed to drop transactions table");
    }

    pub async fn add_tx(
        &self,
        eth_sender: &str,
        eth_signed_tx: &str,
        eth_tx_hash: &str,
        btc_commit_tx: &str,
        btc_reveal_tx: &str,
        btc_send_to_op_return_tx: &str,
        total_fee: i64,
        nonce: i64,
    ) {
        sqlx::query(
            r#"
            INSERT INTO transactions (eth_sender, eth_signed_tx, eth_tx_hash, btc_commit_tx, btc_reveal_tx, btc_send_to_op_return_tx, total_fee, nonce)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?);
            "#,
        )
        .bind(eth_sender.to_ascii_lowercase())
        .bind(eth_signed_tx)
        .bind(eth_tx_hash)
        .bind(btc_commit_tx)
        .bind(btc_reveal_tx)
        .bind(btc_send_to_op_return_tx)
        .bind(total_fee)
        .bind(nonce)
        .execute(&self.db)
        .await
        .expect("Failed to insert transaction");
    }

    pub async fn get_transaction_count(&self, eth_sender: &str) -> i64 {
        let row: (Option<i64>,) = sqlx::query_as(
            r#"
            SELECT MAX(nonce) FROM transactions WHERE eth_sender = ?;
            "#,
        )
        .bind(eth_sender.to_ascii_lowercase())
        .fetch_one(&self.db)
        .await
        .expect("Failed to fetch nonce");

        if let Some(nonce) = row.0 {
            if nonce < 0 {
                // This should never happen, but just in case
                panic!("Nonce in database is negative");
            }
        }
        row.0.map_or(0, |n| n + 1) // Start from nonce 0 if no transactions found
    }
}
