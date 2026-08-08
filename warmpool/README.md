# warmpool

Fast, isolated Postgres integration tests built on fingerprinted template databases.

warmpool helps you keep Postgres backed integration tests fast and reliable without rebuilding the same schema from scratch on every run. It creates a template database once for a given migration set, then clones that template for each test case. Because Postgres can clone a template database efficiently, the expensive migration step is entirley avoided.

## Why this exists

A typical integration test suite has two common choices:

1. Share one database and deal with cross test state contamination.
2. Create a fresh database per test and re-run every migration for each one.

The second approach is always the correct one, but it becomes expensive quickly. On a real schema, migrations will dominate the test runtime.

warmpool addresses this by separating the problem into two simple phases:

- Build a template database once for a given migration set.
- Clone that template for each test database.

## How it works

When you configure warmpool, it:

- loads your migration files from a directory you provide,
- computes a deterministic fingerprint from the migration set,
- uses that fingerprint to derive a template database name,
- creates the template once if it does not already exist,
- clones it for each new test database.

The fingerprint is based on the ordered migration metadata and checksums, so a change to your schema produces a different template name automatically.

## Usage

### Option 1: builder API

Use this when you want explicit control over the pool, migration set, or cleanup behavior.

```rust
use sqlx::postgres::PgConnectOptions;

# async fn example() -> warmpool::Result<()> {
let connect_options: PgConnectOptions = std::env::var("DATABASE_URL")
    .unwrap()
    .parse()
    .unwrap();

let template = warmpool::TemplatePool::builder(connect_options)
    .migrations_from("./migrations")
    .exclude_migration(|m| m.description.contains("seed_data"))
    .build()
    .await?;

let test_db = template.create_test_database().await?;
let pool = test_db.pool();

// run your test against `pool`

test_db.drop_database().await?;
# Ok(())
# }
```

### Option 2: `#[warm_test]`

For the common case, use the proc-macro. It creates a fresh, migrated `sqlx::PgPool` for your test and drops the database after the test returns, including on panic.

```rust
#[warmpool::warm_test(migrations = "./migrations")]
async fn creates_a_post(pool: sqlx::PgPool) {
    // use the pool here
}
```

Add the macro feature in your dev dependencies:

```toml
[dev-dependencies]
warmpool = { version = "0.1", features = ["macros"] }
```

## Important considerations

- Template databases must not have active connections when you clone from them. warmpool closes its own maintenance connection before cloning.
- Cleanup is explicit. Because Rust cannot run async code in `Drop`, you should call `drop_database().await` yourself or use `#[warm_test]`.
- Template databases can accumulate over time if you change your migration set often. Periodic cleanup is recommended.
- The advisory lock key is derived from the fingerprint, which helps avoid collisions between unrelated projects that share the same Postgres instance.

### Removing stale template databases

To clean up old warmpool templates:

```sql
SELECT datname FROM pg_database WHERE datname LIKE 'warmpool_tmpl_%';
```

Then drop the stale templates explicitly:

```sql
DROP DATABASE IF EXISTS warmpool_tmpl_<fingerprint>;
```

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
