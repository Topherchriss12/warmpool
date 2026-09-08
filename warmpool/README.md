# warmpool

[![CI](https://github.com/Topherchriss12/warmpool/actions/workflows/ci.yml/badge.svg)](https://github.com/Topherchriss12/warmpool/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/Topherchriss12/warmpool/branch/main/graph/badge.svg)](https://codecov.io/gh/Topherchriss12/warmpool)
[![crates.io](https://img.shields.io/crates/v/warmpool.svg)](https://crates.io/crates/warmpool)
[![docs.rs](https://img.shields.io/docsrs/warmpool)](https://docs.rs/warmpool)

Fast, isolated Postgres integration tests built on fingerprinted template databases.

warmpool helps you keep Postgres backed integration tests fast and reliable without rebuilding the same schema from scratch on every run. It creates a template database once for a given migration set, then clones that template for each test case. Because Postgres can clone a template database efficiently, the expensive migration step is entirley avoided.

## The problem

A typical integration test suite has two common choices:

1. Share one database and accept cross test state contamination.
2. Create a fresh database per test and re-run every migration for each one.

The second approach is always the correct one, but it becomes expensive quickly. On a real schema, re-running migrations for every test will dominate the total runtime of the suite.

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

## Clone strategy

*New in 0.1.1.*

Postgres 15 added a choice of two strategies for `CREATE DATABASE ... TEMPLATE`:

- **`FILE_COPY`** — forces a checkpoint, then copies the template's on-disk files. This is the only strategy that existed before Postgres 15, and it's what template cloning always did on older servers.
- **`WAL_LOG`** — copies by replaying page changes through WAL instead of a full file copy, without forcing a checkpoint first.

When you don't specify a strategy, PostgreSQL applies its server-side default behavior. That behavior may vary between PostgreSQL versions and environments. On a disk backed instance, `FILE_COPY` can occasionally stall for hundreds of milliseconds even on a small template, while `WAL_LOG` is consistently fast. Prior to 0.1.1, warmpool never specified a strategy and simply took whatever Postgres decided. As of 0.1.1, warmpool defaults to explicitly requesting `STRATEGY = WAL_LOG` on every clone. Environments that need predictable behavior should choose a strategy explicitly. 

**Why:** for template sizes typical of an integration test schema (low tens of MB), `WAL_LOG` is both faster on average and the part that matters more for CI dramatically more consistent. Benchmarked on this
crate's own test schema (a handful of tables, indexes, and a couple hundred seeded rows, ~8MB as a template) over 15 sequential clones on a single persistent connection, isolating the clone operation itself from connection/process overhead:

| Storage         | Strategy    | avg      | min     | max            |
|------------------|-------------|----------|---------|----------------|
| disk (ext4)      | `WAL_LOG`   | 22.1 ms  | 20.7 ms | 23.6 ms        |
| disk (ext4)      | `FILE_COPY` | 78.1 ms  | 33.7 ms | **553.9 ms**   |
| tmpfs            | `WAL_LOG`   | 14.4 ms  | 10.9 ms | 41.5 ms        |
| tmpfs            | `FILE_COPY` | 14.7 ms  | 6.6 ms  | 108.8 ms       |

On real disk the common case for most CI runners, which don't give you control over the underlying filesystem `FILE_COPY`'s checkpoint requirement produces occasional stalls that have nothing to do with your
schema size and everything to do with whatever else is touching the disk at that moment. `WAL_LOG` doesn't have that failure mode. On tmpfs the two converge on average (there's no real disk latency for `FILE_COPY` to be
punished by), but `WAL_LOG`'s tail is still tighter.

**These numbers are from one environment with one schema.** They're not a promise about yours. A benchmark script is included in this repo (`scripts/bench_clone_strategy.sh`), run it against your own instance and
your own template before assuming the defaults are doing the right thing for you on your infrastructure:

```sh
./scripts/bench_clone_strategy.sh <host> <port> <user> <template_db_name>
```

**How it decides:** `WAL_LOG` requires Postgres 15+. warmpool checks `server_version_num` once per `TemplatePool` the same maintenance connection already opened to build or verify the template pays this one extra query, cached for the process's lifetime and silently falls back to the pre 0.1.1 behavior (no `STRATEGY` clause, Postgres decides) on
older servers. You never see an error from this; older servers just get
the same behavior they always had.

**Overriding it via builder API:**

```rust
use warmpool::CloneStrategy;

let template = warmpool::TemplatePool::builder(connect_options)
    .migrations_from("./migrations")
    // if you've benchmarked your own (larger) schema and FILE_COPY wins:
    .clone_strategy(CloneStrategy::FileCopy)
    // or to go back to letting Postgres's internal heuristic decide,
    // exactly like before 
    // .clone_strategy(CloneStrategy::Auto)
    .build()
    .await?;
```

**Overriding it via`#[warm_test]`:**

```rust
#[warmpool::warm_test(migrations = "./migrations", clone_strategy = "file_copy")]
async fn creates_a_post(pool: sqlx::PgPool) {
    // ...
}
```

`clone_strategy` accepts `"wal_log"`, `"file_copy"`, or `"auto"`, and is checked at compile time, a typo there is a compile error, simple as that. The default is `"wal_log"` on Postgres 15+ and `"auto"` on older servers.

`WAL_LOG` writes WAL for the entire copy rather than skipping straight to file bytes, so for templates well beyond typical test schema size, the WAL volume itself can become the bottleneck and `FILE_COPY` can start winning
again. If your schema is unusually large or you're seeing WAL-related pressure (archiving, replication lag, disk usage) from a large template, benchmark both with the script `scripts/bench_clone_strategy.sh` and pick the one that works best for you.

## Important considerations

- `CREATE DATABASE ... TEMPLATE ...` requires that no connection remains open to the source template. warmpool closes its maintenance connection before cloning.
- Cleanup is explicit. Because Rust cannot run async code inside `Drop`, call `drop_database().await` yourself or use `#[warm_test]`.
- Template databases can accumulate over time if your migration set changes frequently. Periodic cleanup is recommended.
- `STRATEGY = WAL_LOG`/`FILE_COPY` pinning only applies on Postgres 15+; older servers are unaffected by the `clone_strategy` setting.

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

There is an existing crate [`sqlx-pg-test-template`](https://crates.io/crates/sqlx-pg-test-template). It also uses Postgres template databases to speed up integration tests, but the approach is fundamentally different:

- It requires you to create and maintain the template database *outside* the test run (usually via `sqlx database create` + `sqlx migrate run`).
- Template invalidation is manual. When migrations change you must rebuild the template yourself.
- There is no automatic fingerprinting or self-invalidation.

warmpool takes a different path. It derives the template name from a deterministic fingerprint of the migration set (version + description + checksum). As a result:

- The cloned template persists across test runs.
- Cloned template is always up to date with the current migrations by structure.
- Template creation and invalidation are automatic.
- Schema changes naturally produce a new template name.
- Existing templates are safely reused when the migrations have not changed.
- No external setup step or rebuild script is required.
- Predictable and  clone strategy.
- The `#[warm_test]` macro requires no boilerplate or imports, it handles template creation, cloning, and cleanup automatically.

> More information is available at [unwrapui.onrender.com](https://unwrapui.onrender.com).

In the terminal, run;

```bash
grep warmpool # Then click on the link `cat /var/www/blog/posts/introducing-warmpool`

# Or type the cat command directly and run it.
cat /var/www/blog/posts/introducing-warmpool
```

If you find warmpool useful, please consider starring the repo on GitHub. It helps others discover it and motivates me to keep improving it.

## License

MIT

