# warmpool

Fast, isolated Postgres integration tests built on fingerprinted template databases.

warmpool helps you keep Postgres backed integration tests fast and reliable without rebuilding the same schema from scratch on every run. It creates a template database once for a given migration set, then clones that template for each test case. Because Postgres can clone a template database efficiently, the expensive migration step is entirley avoided.

## The problem

A typical integration test suite has two common choices:

1. Share one database and accept cross test state contamination.
2. Create a fresh database per test and re-run every migration for each one.

The second approach is always the correct one, but it becomes expensive quickly. On a real schema, re running migrations for every test will dominate the total runtime of the suite.

## The fix

warmpool splits the process into two simple phases:

- build a template database once for a specific migration set,
- clone that template for each test database.

The template is named using a deterministic fingerprint derived from the ordered migration metadata and checksums. That means a change to your migrations automatically produces a new template name, so your tests stay aligned with the current schema without a manual rebuild step. Postgres handles the clone operation efficiently. Only the first test case to run pays the price, the rest become blazingly fast.

## Two ways to use it

### Builder API

Use this when you want direct control over migration selection, trigger cleanup, or multiple migration sets in one process.

```rust
let template = warmpool::TemplatePool::builder(connect_options)
    .migrations_from("./migrations")
    .exclude_migration(|m| m.description.contains("seed_data"))
    .build()
    .await?;

let test_db = template.create_test_database().await?;
let pool = test_db.pool();

// ... test ...

test_db.drop_database().await?;
```

### `#[warm_test]`

For the common case, use the proc-macro. It creates a fresh, migrated `sqlx::PgPool` for your test and drops the database after the test returns, including on panic.

```rust
#[warmpool::warm_test(migrations = "./migrations")]
async fn creates_a_post(pool: sqlx::PgPool) {
    // use the pool here
}
```

```toml
[dev-dependencies]
warmpool = { version = "0.1", features = ["macros"] }
```

## Important considerations

- `CREATE DATABASE ... TEMPLATE ...` requires that no connection remains open to the source template. warmpool closes its maintenance connection before cloning.
- Cleanup is explicit. Because Rust cannot run async code inside `Drop`, call `drop_database().await` yourself or use `#[warm_test]`.
- Template databases can accumulate over time if your migration set changes frequently. Periodic cleanup is recommended.

### Removing stale template databases

To clean up old warmpool templates:

```sql
SELECT datname FROM pg_database WHERE datname LIKE 'warmpool_tmpl_%';
```

Then drop the stale templates explicitly:

```sql
DROP DATABASE IF EXISTS warmpool_tmpl_<fingerprint>;
```

- The advisory lock key is derived from the fingerprint, which helps avoid lock contention between unrelated projects sharing the same Postgres instance.

## Prior art

[`sqlx-pg-test-template`](https://crates.io/crates/sqlx-pg-test-template) solves a related problem for Postgres integration tests, but its workflow is different. It expects you to build and maintain a template database outside of the test runtime, typically by running `sqlx database create` and `sqlx migrate run` whenever migrations change.

warmpool’s template invalidation is automatic and built in. The template database name is derived from a fingerprint of the migration set, so schema changes automatically produce a new template name. That means warmpool can safely reuse existing templates when the migration set is unchanged, without requiring a separate manual rebuild step.

warmpool also improves the runtime path because it uses Postgres native template cloning directly for every test database, rather than depending on an externally managed template creation process.

## Why use warmpool ?

- no separate "build template" step to remember,
- no manual invalidation when the migration set changes,
- no risk of stale templates being reused silently,
- and no external workflow required to keep test templates current.

## License

MIT

