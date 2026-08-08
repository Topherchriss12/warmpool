use sqlx::PgPool;
use std::time::Instant;
use testcontainers::clients::Cli;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

use warmpool::TemplatePool;

/// One Postgres container for the entire test binary, started on first use
/// and reused by every test below.
/// spinning up a container per test multiplies Docker startup cost by the number of
/// tests, and it also means every test gets a blank Postgres instance with
/// no template to reuse which defeats the whole point. Container
/// creation is guarded by `OnceCell`, so concurrently running tests all
/// await the same in-flight initialization.
static POSTGRES_URL: OnceCell<String> = OnceCell::const_new();

async fn shared_postgres_url() -> &'static str {
    POSTGRES_URL
        .get_or_init(|| async {
            let container = tokio::task::spawn_blocking(|| {
                // Leaked deliberately for both the Docker CLI handle and the
                // container need to outlive every test in this binary. The
                // actual Docker container is reaped by testcontainers' own
                // ryuk sidecar / on process exit, independent of this leak.
                let docker: &'static Cli = Box::leak(Box::new(Cli::default()));
                docker.run(Postgres::default())
            })
            .await
            .expect("failed to spawn blocking task for testcontainers");

            let host_port = container.get_host_port_ipv4(5432);
            Box::leak(Box::new(container));

            format!("postgres://postgres:postgres@127.0.0.1:{host_port}/postgres")
        })
        .await
}

async fn build_test_db(migrations_path: &str) -> warmpool::TestDatabase {
    build_template(migrations_path)
        .await
        .create_test_database()
        .await
        .expect("failed to create test db")
}

async fn build_template(migrations_path: &str) -> TemplatePool {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    TemplatePool::builder(connect_options)
        .migrations_from(migrations_path)
        .build()
        .await
        .expect("failed to build template pool")
}

#[tokio::test]
#[ignore]
async fn test_creates_database_with_migrations() {
    let db = build_test_db("./tests/fixtures/migrations").await;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("Failed to query table existence");

    assert!(exists, "The 'users' table should exist from the migration");

    db.drop_database().await.expect("Cleanup should succeed");
}

#[tokio::test]
#[ignore]
async fn test_template_cloning_is_fast() {
    let salt = format!("cloning_is_fast_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .build()
        .await
        .expect("failed to build template pool");

    let start_first = Instant::now();
    let db1 = template
        .create_test_database()
        .await
        .expect("failed to create test db");
    let first_duration = start_first.elapsed();
    db1.drop_database().await.ok();

    let start_second = Instant::now();
    let db2 = template
        .create_test_database()
        .await
        .expect("failed to create test db");
    let second_duration = start_second.elapsed();
    db2.drop_database().await.ok();

    assert!(
        second_duration < first_duration,
        "Second run ({second_duration:?}) should be faster than first run ({first_duration:?})"
    );
}

#[tokio::test]
#[ignore]
async fn test_databases_are_isolated() {
    let db1 = build_test_db("./tests/fixtures/migrations").await;
    let db2 = build_test_db("./tests/fixtures/migrations").await;

    let my_uuid = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, 'user1@test.com')")
        .bind(my_uuid)
        .execute(db1.pool())
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db2.pool())
        .await
        .unwrap();

    assert_eq!(count, 0, "db2 should not see data inserted into db1");

    db1.drop_database().await.unwrap();
    db2.drop_database().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_cleanup_drops_database() {
    let url = shared_postgres_url().await;
    let pool = PgPool::connect(url).await.unwrap();

    let db_name = {
        let db = build_test_db("./tests/fixtures/migrations").await;
        let name = db.name().to_string();
        db.drop_database().await.expect("Explicit cleanup failed");
        name
    };

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&db_name)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert!(!exists, "Database should be dropped after cleanup");
}

/// The property the advisory lock exists to guarantee. N callers racing to
/// build the same template at once should all succeed, all get a fully
/// migrated database (not one caught mid-build), and none should error out
/// or deadlock against each other.
#[tokio::test]
#[ignore]
async fn test_concurrent_creation_from_cold_is_safe() {
    // Unique salt so this test always starts from a cold template,
    // regardless of what other tests have already built.
    let salt = format!("concurrent_cold_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .build()
        .await
        .expect("failed to build template pool");

    let concurrency = 5;
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let template = template.clone();
        handles.push(tokio::spawn(async move {
            template.create_test_database().await
        }));
    }

    let mut created = Vec::with_capacity(concurrency);
    for handle in handles {
        let db = handle
            .await
            .expect("task panicked")
            .expect("concurrent create_test_database should not fail");
        created.push(db);
    }

    // Every clone should be a distinct, fully migrated database, not a
    // shared one and not a partially built one from a caller that raced
    // past the advisory lock.
    let mut names = std::collections::HashSet::new();
    for db in &created {
        assert!(names.insert(db.name().to_string()), "database names must be unique");

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
        )
        .fetch_one(db.pool())
        .await
        .expect("failed to query table existence");
        assert!(exists, "every concurrently-created clone must have the migrated schema");
    }

    for db in created {
        db.drop_database().await.ok();
    }
}

/// Changing the migration set must produce a different template rather
/// than silently reusing a stale one, this is the whole safety property
/// the fingerprint as template name design guarantees.
#[tokio::test]
#[ignore]
async fn test_different_migrations_get_different_templates() {
    let db_v1 = build_test_db("./tests/fixtures/migrations").await;
    let db_v2 = build_test_db("./tests/fixtures/migrations_v2").await;

    let has_posts_v1: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'posts')",
    )
    .fetch_one(db_v1.pool())
    .await
    .unwrap();
    let has_posts_v2: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'posts')",
    )
    .fetch_one(db_v2.pool())
    .await
    .unwrap();

    assert!(!has_posts_v1, "the base migration set should not have the posts table");
    assert!(has_posts_v2, "migrations_v2 should have the posts table");

    db_v1.drop_database().await.ok();
    db_v2.drop_database().await.ok();
}

/// Doesn't touch Postgres at all, so it runs under plain `cargo test`
/// without `--ignored` or Docker fast feedback that bad configuration
/// surfaces as a typed, specific error instead of a panic somewhere deep in
/// the pipeline.
#[tokio::test]
async fn test_bad_migrations_path_returns_typed_error() {
    let connect_options: sqlx::postgres::PgConnectOptions =
        "postgres://postgres:postgres@127.0.0.1:1/postgres".parse().unwrap();

    let result = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/does_not_exist")
        .build()
        .await;

    assert!(matches!(result, Err(warmpool::Error::MigratorLoad { .. })));
}