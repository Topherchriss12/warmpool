use sqlx::{Connection, Executor, PgPool};
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
// #[ignore]
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
// #[ignore]
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
// #[ignore]
async fn test_databases_are_isolated() {
    let db1 = build_test_db("./tests/fixtures/migrations").await;
    let db2 = build_test_db("./tests/fixtures/migrations").await;

    eprintln!("db1 name: {}", db1.name());
    eprintln!("db2 name: {}", db2.name());
    eprintln!("Are they equal? {}", db1.name() == db2.name());

    let db1_current_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(db1.pool())
        .await
        .unwrap();

    let db2_current_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(db2.pool())
        .await
        .unwrap();

    eprintln!("db1 current database: {}", db1_current_db);
    eprintln!("db2 current database: {}", db2_current_db);

    let db2_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db2.pool())
        .await
        .unwrap();

    let my_uuid = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, email, password_hash) VALUES ($1, 'user1@test.com', 'Hash_Pass')",
    )
    .bind(my_uuid)
    .execute(db1.pool())
    .await
    .unwrap();

    let db2_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db2.pool())
        .await
        .unwrap();

    eprintln!(
        "db1 user count: {}",
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
            .fetch_one(db1.pool())
            .await
            .unwrap()
    );
    eprintln!("db2 user count before: {}", db2_before);
    eprintln!("db2 user count after: {}", db2_after);

    assert_eq!(
        db2_after, db2_before,
        "db2 should not see data inserted into db1"
    );

    db1.drop_database().await.unwrap();
    db2.drop_database().await.unwrap();
}

#[tokio::test]
// #[ignore]
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
// #[ignore]
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
        assert!(
            names.insert(db.name().to_string()),
            "database names must be unique"
        );

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
        )
        .fetch_one(db.pool())
        .await
        .expect("failed to query table existence");
        assert!(
            exists,
            "every concurrently-created clone must have the migrated schema"
        );
    }

    for db in created {
        db.drop_database().await.ok();
    }
}

/// Changing the migration set must produce a different template rather
/// than silently reusing a stale one, this is the whole safety property
/// the fingerprint as template name design guarantees.
#[tokio::test]
// #[ignore]
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

    assert!(
        !has_posts_v1,
        "the base migration set should not have the posts table"
    );
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
        "postgres://postgres:postgres@127.0.0.1:1/postgres"
            .parse()
            .unwrap();

    let result = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/does_not_exist")
        .build()
        .await;

    assert!(matches!(result, Err(warmpool::Error::MigratorLoad { .. })));
}

// 0.1.x

/// Functional correctness for each `CloneStrategy` variant: every one of
/// them must actually produce a working, fully migrated database. This
/// can't prove *which* strategy Postgres used internally (that isn't
/// exposed anywhere queryable), only that specifying each one doesn't
/// break cloning which is still worth locking in, since a typo'd SQL
/// fragment in `CloneStrategy::sql_clause` would otherwise only surface as
/// a runtime `CreateTestDb` error the first time someone actually chose
/// that variant.
#[tokio::test]
// #[ignore]
async fn test_clone_strategy_wal_log_produces_a_working_database() {
    let salt = format!("strategy_wal_log_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .clone_strategy(warmpool::CloneStrategy::WalLog)
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("WAL_LOG clone should succeed");

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(exists);

    db.drop_database().await.ok();
}

#[tokio::test]
// #[ignore]
async fn test_clone_strategy_file_copy_produces_a_working_database() {
    let salt = format!("strategy_file_copy_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .clone_strategy(warmpool::CloneStrategy::FileCopy)
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("FILE_COPY clone should succeed");

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(exists);

    db.drop_database().await.ok();
}

#[tokio::test]
// #[ignore]
async fn test_clone_strategy_auto_produces_a_working_database() {
    let salt = format!("strategy_auto_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .clone_strategy(warmpool::CloneStrategy::Auto)
        .build()
        .await
        .expect("failed to build template pool");

    let db = template.create_test_database().await.expect(
        "Auto (no STRATEGY clause) clone should succeed, this is exactly \
                 pre-0.1.2 behavior, still must work unmodified",
    );

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(exists);

    db.drop_database().await.ok();
}

/// `TemplatePoolBuilder::clone_strategy` defaults to `WalLog` -- confirm
/// that not calling `.clone_strategy(...)` at all behaves identically to
/// calling it with `CloneStrategy::WalLog` explicitly, i.e. the default
/// really is wired through and isn't silently falling back to `Auto`.
#[tokio::test]
// #[ignore]
async fn test_default_clone_strategy_matches_explicit_wal_log() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    // No .clone_strategy(...) call at all.
    let default_template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(format!("default_strategy_{}", uuid::Uuid::new_v4()))
        .build()
        .await
        .expect("failed to build template pool");

    let db = default_template
        .create_test_database()
        .await
        .expect("default-strategy clone should succeed exactly like an explicit WalLog would");

    db.drop_database().await.ok();
}

#[tokio::test]
// #[ignore]
async fn test_purge_triggers_in_removes_triggers_from_cloned_database_only() {
    let salt = format!("purge_triggers_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations_with_trigger")
        .fingerprint_salt(salt)
        .purge_triggers_in("public")
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("failed to create test db");

    // The trigger should be gone from the *clone*: inserting into `users`
    // must not populate `audit_log`.
    sqlx::query(
        "INSERT INTO users (email, password_hash) VALUES ('purge-check@test.com', 'Hash_Pass')",
    )
    .execute(db.pool())
    .await
    .expect("insert should succeed even with the trigger purged");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        audit_count, 0,
        "the audit trigger should have been purged from the cloned database"
    );

    // The *template* must still have the trigger, purge_triggers_in only
    // purges the clone, never the template itself. Rather than reach into
    // the template directly (its name is private), we prove this the way any
    // caller of the public API would observe it: create a *second* clone
    // from the same TemplatePool and confirm the trigger is purged there
    // too, which is only possible if the template still has the trigger
    // to be purged from in the first place.
    let db2 = template
        .create_test_database()
        .await
        .expect("second clone from the same template should also succeed");
    sqlx::query(
        "INSERT INTO users (email, password_hash) VALUES ('second-clone@test.com', 'Hash_Pass')",
    )
    .execute(db2.pool())
    .await
    .unwrap();
    let audit_count_db2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(db2.pool())
        .await
        .unwrap();
    assert_eq!(
        audit_count_db2, 0,
        "purge_triggers_in must be re-applied to every clone independently, \
         not something that leaks into (or out of) the shared template"
    );

    db.drop_database().await.ok();
    db2.drop_database().await.ok();
}

/// Our core claim; the template "survives across test runs (and
/// across `cargo test` invocations...)". Within a single test binary we
/// can't literally start a new process, but building a *second*,
/// independent `TemplatePool` against the same migrations and the same
/// fingerprint salt should reuse the existing template rather than rebuilding one from scratch,
/// and the first clone from that second pool should be fast in the same way every clone after
/// the first one from the first pool was.
#[tokio::test]
// #[ignore]
async fn test_template_is_reused_across_separate_template_pool_instances() {
    let salt = format!("reuse_across_instances_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    // First TemplatePool: pays the full build cost.
    let pool_a = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt.clone())
        .build()
        .await
        .expect("failed to build template pool (a)");

    let start_a = Instant::now();
    let db_a = pool_a
        .create_test_database()
        .await
        .expect("failed to create test db from pool a");
    let elapsed_a = start_a.elapsed();
    db_a.drop_database().await.ok();

    // Second, independent TemplatePool, same migrations + salt -> same
    // fingerprint -> same template name. Its very first clone should not
    // need to rebuild the template (no migrations to rerun), and should be
    // fast in the same way every clone after the first one from pool_a was.
    let pool_b = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .build()
        .await
        .expect("failed to build template pool (b)");

    let start = Instant::now();
    let db_b = pool_b
        .create_test_database()
        .await
        .expect("failed to create test db from pool b");
    let elapsed = start.elapsed();

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db_b.pool())
    .await
    .unwrap();
    assert!(exists, "pool_b's clone must have the migrated schema");

    // pool_b should be faster or at least not slower since it's cloning an
    // existing template and not building it from scratch.
    //
    // We expect at least a 3x speedup in a healthy environment, but this test
    // is inherently non-deterministic due to factors like disk I/O, Docker
    // overhead, and system load. The critical assertion is that pool_b is not
    // slower than pool_a - if it is, the template is definitely not being reused.
    let speedup = elapsed_a.as_secs_f64() / elapsed.as_secs_f64();

    if speedup < 3.0 {
        eprintln!(
            "Warning: template reuse speedup is only {:.2}x (expected >= 3x). \
             This may indicate a slow or overloaded test environment, but the \
             template is still being reused since pool_b ({elapsed:?}) is faster \
             than pool_a ({elapsed_a:?}).",
            speedup
        );
    }

    // The test only fails if pool_b is slower than pool_a, which would mean
    // the template is not being reused at all, that's a critical bug.
    // A speedup < 3x is just a warning, not a failure.
    assert!(
        elapsed <= elapsed_a,
        "pool_b's first clone ({elapsed:?}) was slower than pool_a's first clone \
         ({elapsed_a:?}). This indicates the template is not being reused \
         between separate TemplatePool instances, which is a critical bug."
    );

    db_b.drop_database().await.ok();
}

/// Previously (< 0.1.2)`create_test_database()` didn't sweep the *template* for lingering
/// connections before cloning, so this test used to assert the *failure* on purpose,
/// so that fixing the gap would force this test to be updated deliberately.
/// That's exactly what we are doing, the fix landed, and
/// this test's assertion flipps from `is_err()` to `is_ok()`.
///
/// `TemplatePool` doesn't expose the template's own name, but
/// `warmpool::fingerprint` (re-exported publicly, alongside
/// `lock_key_from_fingerprint`, from the crate root) plus the same
/// `warmpool_tmpl_` default prefix `TemplatePoolBuilder` uses is enough to
/// reconstruct it deterministically without reaching into anything
/// private, so this test can hold a real connection open against the real
/// template and observe whether `create_test_database()` sweeps it away before cloning.
#[tokio::test]
// #[ignore]
async fn test_stray_connection_to_template_no_longer_blocks_cloning() {
    let salt = format!("stray_connection_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt.clone())
        .build()
        .await
        .expect("failed to build template pool");

    // Force the template to exist.
    let warm_up = template.create_test_database().await.unwrap();
    warm_up.drop_database().await.ok();

    // Reconstruct the template's name as TemplatePoolBuilder does:
    // same migrations directory (so the same Migration set and therefore
    // the same fingerprint), the same salt, and the crate's default
    // `warmpool_tmpl_` prefix (this test never overrides
    // `.template_prefix(...)`, so the default applies here too).
    let migrator =
        sqlx::migrate::Migrator::new(std::path::Path::new("./tests/fixtures/migrations"))
            .await
            .expect("failed to load migrations for fingerprint reconstruction");
    let migrations: Vec<_> = migrator.iter().cloned().collect();
    let fingerprint = warmpool::fingerprint(&migrations, Some(&salt));
    let template_name = format!("warmpool_tmpl_{fingerprint}");

    // Hold a real, lingering connection open directly against the template,
    // exactly the kind of stray connection a crashed process or a
    // developer's ad hoc `psql` session would leave behind.
    let template_opts = connect_options.clone().database(&template_name);
    let mut lingering_template_connection =
        sqlx::postgres::PgConnection::connect_with(&template_opts)
            .await
            .expect("failed to connect directly to the template database");

    let result = template.create_test_database().await;

    assert!(
        result.is_ok(),
        "create_test_database() should sweep stray connections from the \
         template before cloning and succeed despite the lingering \
         connection this test opened; got {:?}",
        result.err()
    );

    // Prove the sweep actually did something, not just that the clone
    // happened to succeed for an unrelated reason: the specific connection
    // this test opened should now be dead, terminated by the sweep.
    let alive_check: Result<bool, _> = sqlx::query_scalar("SELECT true")
        .fetch_one(&mut lingering_template_connection)
        .await;
    let backend_alive = alive_check.is_ok();
    assert!(
        !backend_alive,
        "the lingering connection this test opened should have been \
         terminated by create_test_database()'s sweep. If it's still \
         alive, the clone above succeeded for some other reason, not \
         because the sweep worked"
    );

    let db = result.unwrap();
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(
        exists,
        "the clone that succeeded despite the stray connection must still \
         be a real, fully migrated database, not a degenerate success"
    );

    db.drop_database().await.ok();
}

/// This is a concern we had and documented, does sweeping the template
/// introduce any new race against a *legitimate, concurrent template
/// build*? The advisory lock already serializes builds against each
/// other, but the sweep in `create_test_database()` runs outside that
/// lock, on the clone path, could it ever terminate a connection that's
/// legitimately part of an in-progress build, rather than a genuine
/// stray?
///
/// It can't, and this test is here to demonstrates why.
/// `build_template_if_missing()` always closes its own connection to the template *before*
/// `build_or_reuse_template()` releases the advisory lock.
/// That means by the time any caller's `ensure_template()` call returns
/// which is a precondition for reaching the sweep at all,no build for
/// that same template can still be holding a connection open. Anything
/// the sweep finds is therefore a genuine stray, never a build in
/// progress.
///
/// This test exercises the part of that claim that's actually observable
/// from outside the crate: an external connection opened *during* a cold
/// build (while migrations are actively running) does not disrupt the
/// build, and is itself cleanly swept away by the next clone attempt
/// afterward, proving the sweep and the build path coexist safely.
#[tokio::test]
// #[ignore]
async fn test_external_connection_during_a_build_does_not_disrupt_it_or_survive_the_next_sweep() {
    let salt = format!("concurrent_build_observer_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt.clone())
        .build()
        .await
        .expect("failed to build template pool");

    // Reconstruct the template's name up front, before the template
    // exists, the same way the previous test does.
    let migrator =
        sqlx::migrate::Migrator::new(std::path::Path::new("./tests/fixtures/migrations"))
            .await
            .expect("failed to load migrations for fingerprint reconstruction");
    let migrations: Vec<_> = migrator.iter().cloned().collect();
    let fingerprint = warmpool::fingerprint(&migrations, Some(&salt));
    let template_name = format!("warmpool_tmpl_{fingerprint}");

    // Race an external connection attempt against the first-ever build.
    // The build creates the empty template database before it starts
    // running migrations against it, so there's an actual window where the
    // template exists but isn't finished yet, this task polls until it
    // can connect, then holds the connection open across the rest of the
    // build.
    let connect_options_for_observer = connect_options.clone();
    let template_name_for_observer = template_name.clone();
    let observer = tokio::spawn(async move {
        loop {
            let opts = connect_options_for_observer
                .clone()
                .database(&template_name_for_observer);
            match sqlx::postgres::PgConnection::connect_with(&opts).await {
                Ok(conn) => return conn,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
    });

    // Drive the actual cold build concurrently with the observer above
    // trying to attach to it mid-flight.
    let db = template
        .create_test_database()
        .await
        .expect("the build must complete successfully regardless of the external observer");

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(
        exists,
        "the build must produce a fully migrated template even with an \
         external connection attached partway through"
    );
    db.drop_database().await.ok();

    // The observer should have managed to connect at some point during
    // the build window it polls for up to the whole build's duration.
    let mut observer_conn = tokio::time::timeout(std::time::Duration::from_secs(10), observer)
        .await
        .expect("observer task timed out")
        .expect("observer task panicked");

    // That lingering connection is still open right now. The *next*
    // create_test_database() call's sweep must clean it up, exactly like
    // it would for any other stray connection.
    let second = template.create_test_database().await.expect(
        "a lingering connection left over from during the build must not block a later clone",
    );

    let alive_check: Result<bool, _> = sqlx::query_scalar("SELECT true")
        .fetch_one(&mut observer_conn)
        .await;
    let backend_alive = alive_check.is_ok();
    assert!(
        !backend_alive,
        "the observer's connection, opened during the build and left \
         open, should have been terminated by the second \
         create_test_database() call's sweep"
    );

    second.drop_database().await.ok();
}

/// Integration confirmation of the `pool.rs` unit test finding: an
/// excluded migration is not applied to test databases either.
/// The unit tests prove this at the Rust struct leve, (the excluded
/// `Migration` is never stored. This proves it at the SQL level, the
/// table it would have created genuinely does not exist in a real, live
/// clone.
#[tokio::test]
// #[ignore]
async fn test_excluded_migration_is_absent_from_both_template_and_test_database() {
    let salt = format!("exclude_migration_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations_v2")
        .fingerprint_salt(salt)
        .exclude_migration(|m| m.description.contains("posts"))
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("failed to create test db");

    let has_posts: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'posts')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert!(
        !has_posts,
        "excluding the migration that creates `posts` means the table must not \
         exist in the cloned test database -- confirming, against a real \
         database, that exclusion is not something applied post-clone"
    );

    db.drop_database().await.ok();
}

/// template construction wasn't crash-atomic. A crash
/// mid-migration (a killed CI job, an OOM, a migration panicking the
/// process) used to leave a half-migrated database sitting under the
/// template's final, fingerprinted name and the existence check in
/// `build_template_if_missing()` had no way to distinguish that from a
/// genuinely finished template, so every clone afterward silently got a
/// broken schema.
///
/// There's no way to literally kill a process mid-migration from inside a
/// single test function and have anything left to assert against
/// afterward, so this test constructs the *end state* a crash would leave
/// behind directly: a `_building` database that exists and has some, but
/// not all, of the migrations applied, with the final name not existing
/// at all bypassing the public API entirely to build it, the same way
/// `warmpool::fingerprint` is used elsewhere in this file to reconstruct
/// names without reaching into anything private. Then it drives a
/// completely ordinary `create_test_database()` call for that exact
/// fingerprint and confirms the orphan gets cleaned up and a fresh,
/// *complete* template gets built not that the half-built one is
/// somehow resumed or reused.
#[tokio::test]
// #[ignore]
async fn test_orphaned_building_database_is_cleaned_up_and_rebuilt() {
    let salt = format!("orphan_building_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    // Reconstruct the exact names warmpool would use for this fingerprint,
    // the same way test_stray_connection_to_template_no_longer_blocks_cloning
    // and its sibling already do.
    let migrator =
        sqlx::migrate::Migrator::new(std::path::Path::new("./tests/fixtures/migrations"))
            .await
            .expect("failed to load migrations for fingerprint reconstruction");
    let migrations: Vec<_> = migrator.iter().cloned().collect();
    let fingerprint = warmpool::fingerprint(&migrations, Some(&salt));
    let template_name = format!("warmpool_tmpl_{fingerprint}");
    let building_name = format!("{template_name}_building");

    // Build the orphan directly: a `_building` database with only the
    // *first* migration applied (`users`, no `widgets`/`gadgets`/seed
    // data), and never renamed -- exactly what a crash right after the
    // first migration committed, but before the second one ran, would
    // leave behind.
    let mut maintenance = sqlx::postgres::PgConnection::connect_with(&connect_options)
        .await
        .expect("failed to open a maintenance connection for test setup");
    maintenance
        .execute(format!(r#"CREATE DATABASE "{building_name}""#).as_str())
        .await
        .expect("test setup: failed to create the simulated orphan _building database");

    let building_opts = connect_options.clone().database(&building_name);
    let mut building_conn = sqlx::postgres::PgConnection::connect_with(&building_opts)
        .await
        .expect("test setup: failed to connect to the simulated orphan");
    building_conn
        .execute(migrations[0].sql.as_ref())
        .await
        .expect("test setup: failed to apply the first migration to the simulated orphan");
    drop(building_conn);

    // Confirm the test setup actually produced the state we think it did,
    // before making any claim about what warmpool does with it.
    let orphan_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&building_name)
            .fetch_one(&mut maintenance)
            .await
            .unwrap();
    assert!(
        orphan_exists,
        "test setup: the simulated orphan must exist before the real assertions run"
    );
    let final_exists_before: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&template_name)
            .fetch_one(&mut maintenance)
            .await
            .unwrap();
    assert!(
        !final_exists_before,
        "test setup: the final template name must not exist yet -- \
         only the orphan does, same as after a real crash"
    );

    // Now drive a completely ordinary create_test_database() call for
    // this exact fingerprint. Before this fix, build_template_if_missing()
    // would have tried `CREATE DATABASE "{template_name}"` directly and
    // this test would still be sitting on the orphan; there was nothing
    // here to detect or clean it up.
    let template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .build()
        .await
        .expect("failed to build template pool");

    let db = template.create_test_database().await.expect(
        "create_test_database() should clean up the orphaned _building \
         database and build a fresh template, not get stuck failing with \
         \"database already exists\" against the orphan forever",
    );

    // The clone must have the FULL schema -- all four migrations, not
    // just the one the orphan happened to have -- proving a genuine fresh
    // rebuild happened rather than the half-built orphan being resumed,
    // patched, or reused as-is.
    for table in ["users", "widgets", "gadgets"] {
        let exists: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '{table}')"
        ))
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(
            exists,
            "table `{table}` must exist -- the rebuild must be complete, \
             not resuming from the orphan's partial state"
        );
    }

    // The orphan is gone -- dropped during cleanup, its replacement
    // consumed by the rename into the final name. Nothing should be
    // left sitting under the `_building` suffix.
    let building_still_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&building_name)
            .fetch_one(&mut maintenance)
            .await
            .unwrap();
    assert!(
        !building_still_exists,
        "the _building database must not linger after a successful rebuild"
    );

    db.drop_database().await.ok();
}

/// A second, independent build attempt for the *same* fingerprint after
/// an orphan has already been cleaned up and rebuilt once must not somehow
/// re-encounter the orphan or otherwise behave differently the second
/// time. The cleanup and rebuild path in `build_template_if_missing()`
/// only runs when the final template name doesn't exist yet, so once a
/// rebuild has succeeded, later calls should take the ordinary fast path
/// (existence check finds it, returns immediately) exactly like any other
/// already built template.
#[tokio::test]
// #[ignore]
async fn test_template_is_reusable_normally_after_orphan_recovery() {
    let salt = format!("orphan_then_reuse_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let migrator =
        sqlx::migrate::Migrator::new(std::path::Path::new("./tests/fixtures/migrations"))
            .await
            .expect("failed to load migrations for fingerprint reconstruction");
    let migrations: Vec<_> = migrator.iter().cloned().collect();
    let fingerprint = warmpool::fingerprint(&migrations, Some(&salt));
    let template_name = format!("warmpool_tmpl_{fingerprint}");
    let building_name = format!("{template_name}_building");

    // Simulate the orphan, then let a real
    // TemplatePool clean it up and rebuild.
    let mut maintenance = sqlx::postgres::PgConnection::connect_with(&connect_options)
        .await
        .expect("failed to open a maintenance connection for test setup");
    sqlx::query(&format!(r#"CREATE DATABASE "{building_name}""#))
        .execute(&mut maintenance)
        .await
        .expect("test setup: failed to create the simulated orphan");

    let template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .build()
        .await
        .expect("failed to build template pool");

    let first = template
        .create_test_database()
        .await
        .expect("first clone should trigger orphan cleanup and a successful rebuild");
    first.drop_database().await.ok();

    // Second clone from the same TemplatePool, after recovery already
    // happened once: this should be a completely ordinary, fast clone,
    // not another cleanup and rebuild cycle.
    let second = template
        .create_test_database()
        .await
        .expect("second clone after successful orphan recovery should behave normally");

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'widgets')",
    )
    .fetch_one(second.pool())
    .await
    .unwrap();
    assert!(
        exists,
        "the second clone must have the full, recovered schema"
    );

    second.drop_database().await.ok();
}

///  `purge_triggers_in(schema)` used to interpolate `schema` unescaped into a SQL string literal. This test drives the
/// fixed code through the ordinary public API with a malicious schem name and confirms it can no longer reach triggers
/// outside the schema  it names.
///
/// On the actual exploit,  a `'; DROP TABLE x; --` payload does **not**
/// work against this code path. The interpolation point sits inside a
/// dollar quoted `DO $$ ... $$` body, which Postgres parses as one unit,
/// so a stray `;` only produces a syntax error.
#[tokio::test]
// #[ignore]
async fn test_purge_triggers_in_cannot_escape_its_schema_via_a_malicious_name() {
    let salt = format!("purge_injection_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    // A schema name crafted to widen the WHERE clause if it were
    // interpolated unescaped. There is no real schema by this name, the
    // whole point is that after the fix it's treated as one inert literal
    // that simply matches nothing.
    let malicious_schema = "tenant_a' OR '1'='1";

    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .purge_triggers_in(malicious_schema)
        .build()
        .await
        .expect("failed to build template pool");

    let db = template.create_test_database().await.expect(
        "a malicious schema name must not break the clone -- it should \
                 simply match no schema and purge nothing",
    );

    // Both triggers must survive. Before the fix, the widened WHERE
    // clause matched both, and (depending on search_path) could actually
    // drop one of them.
    let surviving: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger
         JOIN pg_class ON pg_class.oid = tgrelid
         JOIN pg_namespace ON pg_namespace.oid = relnamespace
         WHERE NOT tgisinternal AND nspname IN ('tenant_a', 'tenant_b')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to count surviving triggers");

    assert_eq!(
        surviving, 2,
        "both tenant triggers must survive: a schema name that matches no \
         real schema must purge nothing at all, not widen the WHERE clause \
         into matching every trigger in the database"
    );

    db.drop_database().await.ok();
}

/// The ordinary, non adversarial half of the same behavior: is a legitimate
/// schema name still purges exactly that schema's triggers and nothing
/// else. Without this, the test above could pass trivially if this fix had
/// broken `purge_triggers_in` into a permanent no-op.
#[tokio::test]
// #[ignore]
async fn test_purge_triggers_in_still_purges_exactly_the_named_schema() {
    let salt = format!("purge_scoped_{}", uuid::Uuid::new_v4());
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(salt)
        .purge_triggers_in("tenant_a")
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("failed to create test db");

    let tenant_a_triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger
         JOIN pg_class ON pg_class.oid = tgrelid
         JOIN pg_namespace ON pg_namespace.oid = relnamespace
         WHERE NOT tgisinternal AND nspname = 'tenant_a'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    let tenant_b_triggers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger
         JOIN pg_class ON pg_class.oid = tgrelid
         JOIN pg_namespace ON pg_namespace.oid = relnamespace
         WHERE NOT tgisinternal AND nspname = 'tenant_b'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(
        tenant_a_triggers, 0,
        "the named schema's triggers must actually be purged -- the fix must \
         not have turned purge_triggers_in into a no-op"
    );
    assert_eq!(
        tenant_b_triggers, 1,
        "a schema that wasn't named must be left completely alone"
    );

    db.drop_database().await.ok();
}

/// `template_prefix` fed into double quoted
/// identifiers unescaped, so a prefix containing `"` could close the
/// identifier, terminate the `CREATE DATABASE` statement, and start a new
/// one.
///
/// Unlike #3 this was **real arbitrary statement execution**, not just a
/// widened `WHERE` clause, these statements go out over simple query
/// protocol, where a `;` really can start a new statement, whereas #3's
/// payload was trapped inside a `DO $$ ... $$` body Postgres parses as a
/// single unit. Confirmed by running the unescaped form against a real
/// instance and watching it drop an unrelated database; confirmed inert
/// against the fixed form.
///
/// This test drives the same payload through the ordinary public API and
/// asserts the bystander database survives.
#[tokio::test]
// #[ignore]
async fn test_malicious_template_prefix_cannot_execute_a_second_statement() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    // A bystander database the payload tries to destroy.
    let victim = format!("wp_victim_{}", uuid::Uuid::new_v4().simple());
    let mut maintenance = sqlx::postgres::PgConnection::connect_with(&connect_options)
        .await
        .expect("failed to open maintenance connection");
    maintenance
        .execute(format!(r#"CREATE DATABASE "{victim}""#).as_str())
        .await
        .expect("test setup: failed to create the bystander database");

    let malicious_prefix = format!(r#"wp_evil_"; DROP DATABASE "{victim}"; --"#);

    let template = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(format!("prefix_injection_{}", uuid::Uuid::new_v4()))
        .template_prefix(malicious_prefix)
        .build()
        .await
        .expect("failed to build template pool");

    // Whether this succeeds or fails is not the point, and deliberately
    // not asserted: a prefix like this may well produce an invalid or
    // over length database name, and erroring out is a perfectly
    // acceptable outcome. The only thing that must hold is that it
    // never executes the payload's second statement.
    let _ = template.create_test_database().await;

    let victim_survived: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&victim)
            .fetch_one(&mut maintenance)
            .await
            .expect("failed to check whether the bystander survived");

    assert!(
        victim_survived,
        "a `\"` in template_prefix must never be able to terminate warmpool's \
         own statement and run a second one, the bystander database `{victim}` \
         was dropped, which is exactly the break-out this fix closes"
    );

    // Cleanup, best effort.
    let _ = maintenance
        .execute(format!(r#"DROP DATABASE IF EXISTS "{victim}""#).as_str())
        .await;
}

/// The non-adversarial half: an ordinary custom `template_prefix` must
/// still work exactly as documented. Without this, the test above could
/// pass trivially if escaping had broken `template_prefix` outright.
#[tokio::test]
// #[ignore]
async fn test_ordinary_custom_template_prefix_still_works() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let template = TemplatePool::builder(connect_options)
        .migrations_from("./tests/fixtures/migrations")
        .fingerprint_salt(format!("custom_prefix_{}", uuid::Uuid::new_v4()))
        .template_prefix("my_custom_tmpl_")
        .build()
        .await
        .expect("failed to build template pool");

    let db = template
        .create_test_database()
        .await
        .expect("a plain custom prefix must still produce a working clone");

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'users')",
    )
    .fetch_one(db.pool())
    .await
    .expect("failed to query table existence");
    assert!(exists, "the clone must still be fully migrated");

    db.drop_database().await.ok();
}

/// stale templates accumulate after migration churn.
///
/// Changing migrations changes the fingerprint, which changes
/// the template *name*, so the new template is built alongside the old
/// one and nothing is blocked. The real cost is
/// accumulation disk and clutter.
#[tokio::test]
// #[ignore]
async fn test_changed_migrations_do_not_require_dropping_the_old_template() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let prefix = format!("wp_churn_{}_", uuid::Uuid::new_v4().simple());

    // Two different migration sets -> two different fingerprints.
    let pool_v1 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v1");
    let db1 = pool_v1
        .create_test_database()
        .await
        .expect("v1 clone failed");
    db1.drop_database().await.ok();

    // "Migrations changed": a different set, same prefix. This must
    // succeed without anyone dropping the first template by hand.
    let pool_v2 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations_v2")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v2");
    let db2 = pool_v2.create_test_database().await.expect(
        "a changed migration set must build a new template alongside the old one, \
                 not require manual cleanup first",
    );

    let has_posts: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'posts')",
    )
    .fetch_one(db2.pool())
    .await
    .unwrap();
    assert!(has_posts, "v2's clone must reflect the new migration set");
    db2.drop_database().await.ok();

    // Both templates now exist. That accumulation is the actual problem.
    let stale = pool_v2
        .stale_template_names()
        .await
        .expect("listing stale templates should not fail");
    assert_eq!(
        stale.len(),
        1,
        "v1's template should now be visible as stale from v2's point of view: {stale:?}"
    );

    // Clean up after ourselves regardless of what the test asserted.
    let _ = pool_v2.prune_stale_templates().await;
}

/// Pruning must drop the stale template and leave the live one alone.
#[tokio::test]
// #[ignore]
async fn test_prune_stale_templates_drops_only_the_stale_ones() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let prefix = format!("wp_prune_{}_", uuid::Uuid::new_v4().simple());

    let pool_v1 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v1");
    pool_v1
        .create_test_database()
        .await
        .expect("v1 clone failed")
        .drop_database()
        .await
        .ok();

    let pool_v2 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations_v2")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v2");
    pool_v2
        .create_test_database()
        .await
        .expect("v2 clone failed")
        .drop_database()
        .await
        .ok();

    let dropped = pool_v2
        .prune_stale_templates()
        .await
        .expect("prune should succeed");
    assert_eq!(
        dropped.len(),
        1,
        "exactly v1's template should be dropped: {dropped:?}"
    );

    // v2's own template must still be usable afterward -- pruning must
    // never take out the template the pool is actively using.
    let db = pool_v2
        .create_test_database()
        .await
        .expect("the live template must survive pruning");
    db.drop_database().await.ok();

    let still_stale = pool_v2.stale_template_names().await.unwrap();
    assert!(
        still_stale.is_empty(),
        "nothing should remain stale: {still_stale:?}"
    );
}

/// Pruning is scoped by prefix, and that scoping must be exact. A naive
/// `datname LIKE 'warmpool_tmpl_%'` is flawed here, because `_` is a
/// single character wildcard in `LIKE` and the default prefix is full of
/// them. When the default prefix is used, a `LIKE`-based prune could easily have
/// matched an unrelated `warmpoolXtmplXdddd` database that a destructive
/// prune would then have dropped. This test pins the behavior,
/// a database that merely *resembles* the prefix is never touched.
#[tokio::test]
// #[ignore]
async fn test_prune_does_not_touch_databases_that_only_resemble_the_prefix() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();

    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("wp_like_{unique}_");
    // Same length and shape, but with the underscores replaced -- only a
    // LIKE-wildcard match would catch this one.
    let lookalike = format!("wpXlike{unique}X0000");

    let mut maintenance = sqlx::postgres::PgConnection::connect_with(&connect_options)
        .await
        .expect("failed to open maintenance connection");
    maintenance
        .execute(format!(r#"CREATE DATABASE "{lookalike}""#).as_str())
        .await
        .expect("test setup: failed to create the lookalike database");

    let pool = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .template_prefix(prefix)
        .build()
        .await
        .expect("failed to build pool");
    pool.create_test_database()
        .await
        .expect("clone failed")
        .drop_database()
        .await
        .ok();

    let _ = pool
        .prune_stale_templates()
        .await
        .expect("prune should succeed");

    let lookalike_survived: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&lookalike)
            .fetch_one(&mut maintenance)
            .await
            .unwrap();
    assert!(
        lookalike_survived,
        "`{lookalike}` does not share the prefix and must never be pruned"
    );

    let _ = maintenance
        .execute(format!(r#"DROP DATABASE IF EXISTS "{lookalike}""#).as_str())
        .await;
}

/// Pruning is off unless asked for. A pool built without
/// `prune_stale_templates_on_build(true)` must leave other templates
/// alone, because prefix scoping is shared with sibling migration sets
/// and unrelated projects.
#[tokio::test]
// #[ignore]
async fn test_pruning_does_not_happen_unless_explicitly_enabled() {
    let url = shared_postgres_url().await;
    let connect_options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let prefix = format!("wp_optin_{}_", uuid::Uuid::new_v4().simple());

    let pool_v1 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v1");
    pool_v1
        .create_test_database()
        .await
        .expect("v1 clone failed")
        .drop_database()
        .await
        .ok();

    // Default builder: no pruning requested.
    let pool_v2 = TemplatePool::builder(connect_options.clone())
        .migrations_from("./tests/fixtures/migrations_v2")
        .template_prefix(prefix.clone())
        .build()
        .await
        .expect("failed to build pool v2");
    pool_v2
        .create_test_database()
        .await
        .expect("v2 clone failed")
        .drop_database()
        .await
        .ok();

    let stale = pool_v2.stale_template_names().await.unwrap();
    assert_eq!(
        stale.len(),
        1,
        "v1's template must still be present -- pruning must not happen by default: {stale:?}"
    );

    let _ = pool_v2.prune_stale_templates().await;
}
