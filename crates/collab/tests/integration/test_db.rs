use std::sync::Arc;
use std::time::Duration;

use collab::db::*;
use gpui::BackgroundExecutor;
use parking_lot::Mutex;
use rand::prelude::*;
use sea_orm::ConnectionTrait;
use sqlx::migrate::MigrateDatabase;

#[path = "db_tests/migrations.rs"]
mod migrations;
use migrations::run_database_migrations;

pub struct TestDb {
    pub db: Option<Arc<Database>>,
    pub connection: Option<sqlx::AnyConnection>,
}

impl TestDb {
    pub fn sqlite(executor: BackgroundExecutor) -> Self {
        let url = "sqlite::memory:";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        let mut db = runtime.block_on(async {
            let mut options = ConnectOptions::new(url);
            options.max_connections(5);
            let mut db = Database::new(options).await.unwrap();
            let sql = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/migrations.sqlite/20221109000000_test_schema.sql"
            ));
            db.pool
                .execute(sea_orm::Statement::from_string(
                    db.pool.get_database_backend(),
                    sql,
                ))
                .await
                .unwrap();
            db.initialize_notification_kinds().await.unwrap();
            db
        });

        db.test_options = Some(DatabaseTestOptions {
            executor,
            runtime,
            query_failure_probability: parking_lot::Mutex::new(0.0),
        });

        Self {
            db: Some(Arc::new(db)),
            connection: None,
        }
    }

    pub fn postgres(executor: BackgroundExecutor) -> Self {
        static LOCK: Mutex<()> = Mutex::new(());

        let _guard = LOCK.lock();
        let mut rng = StdRng::from_os_rng();
        let url = format!(
            "postgres://postgres@localhost/zed-test-{}",
            rng.random::<u128>()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        let mut db = runtime.block_on(async {
            sqlx::Postgres::create_database(&url)
                .await
                .expect("failed to create test db");
            let mut options = ConnectOptions::new(url);
            options
                .max_connections(5)
                .idle_timeout(Duration::from_secs(0));
            let mut db = Database::new(options).await.unwrap();
            let migrations_path = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
            run_database_migrations(db.options(), migrations_path)
                .await
                .unwrap();
            db.initialize_notification_kinds().await.unwrap();
            db
        });

        db.test_options = Some(DatabaseTestOptions {
            executor,
            runtime,
            query_failure_probability: parking_lot::Mutex::new(0.0),
        });

        Self {
            db: Some(Arc::new(db)),
            connection: None,
        }
    }

    pub fn db(&self) -> &Arc<Database> {
        self.db.as_ref().unwrap()
    }

    pub fn set_query_failure_probability(&self, probability: f64) {
        let database = self.db.as_ref().unwrap();
        let test_options = database.test_options.as_ref().unwrap();
        *test_options.query_failure_probability.lock() = probability;
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let db = self.db.take().unwrap();
        if let sea_orm::DatabaseBackend::Postgres = db.pool.get_database_backend() {
            db.test_options.as_ref().unwrap().runtime.block_on(async {
                use util::ResultExt;
                let query = "
                        SELECT pg_terminate_backend(pg_stat_activity.pid)
                        FROM pg_stat_activity
                        WHERE
                            pg_stat_activity.datname = current_database() AND
                            pid <> pg_backend_pid();
                    ";
                db.pool
                    .execute(sea_orm::Statement::from_string(
                        db.pool.get_database_backend(),
                        query,
                    ))
                    .await
                    .log_err();
                sqlx::Postgres::drop_database(db.options.get_url())
                    .await
                    .log_err();
            })
        }
    }
}
