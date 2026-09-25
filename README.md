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
warmpool = { version = "0.1.6", features = ["macros"] }
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

### Stale template connection safety

`CREATE DATABASE ... TEMPLATE ...` requires that no connection remains open to the source template. In other words, a stray connection to the template database is enough to make cloning fail even when the template itself is otherwise valid. This was treated as a real bug and addressed in 0.1.2.

As 0f 0.1.2+ warmpool runs a `pg_terminate_backend` sweep against the template's `datname` on the same maintenance connection already open in `create_test_database()`, immediately before the clone statement. Similar to the cleanup `TestDatabase::drop_database()` already performs for the database it owns. The sweep is shared via a `terminate_other_backends()` helper, but the call sites serve different purposes; one clears a database warmpool owns, the other clears the template it is about to clone from. 

However, it does not prevent a brand new connection from racing in between the sweep finishing and the `CREATE DATABASE` statement executing. That residual window is accepted by design, and `Error::TemplateConnectionSweep` exists to make that failure mode explicit, warmpool cannot guarantee no new connection appears in that tiny race window.

## Template lifecycle
 
Templates are cheap to keep and are never auto invalidated. A template that isn't the current one for its migration set just becomes unreachable, not deleted. As of 0.1.6+ warmpool offers  two ways to reclaim that space. 

### Manual cleanup (the default recommendation)
 
```sql
SELECT datname FROM pg_database
WHERE left(datname, length('warmpool_tmpl_')) = 'warmpool_tmpl_';
```
 
```sql
DROP DATABASE IF EXISTS warmpool_tmpl_<fingerprint>;
```
 
This is what we recommend if you're unsure whether the automatic option below is safe for your setup and environment, or if you'd simply rather look at the list before anything gets dropped. You get to see the names before you drop them, and you can check the timestamps in `pg_stat_database` to see which ones are actually stale.

A `warmpool_tmpl_<fingerprint>_building` name found this way is almost always a genuine leftover from a build that crashed before finishing, and dropping it is exactly the right thing to do. However, if a build for that same migration set is actively running elsewhere
right now, its `_building` database is legitimately in use, not stale.
The automated option knows the difference but a one-off manual `DROP` does not, so if you're running this against a shared instance while other devs or CI jobs might be building, check
first. If you see a `_building` name and you're unsure, leave it alone.

### Automatic Opt-in pruning
 
`TemplatePool` can drop its own stale siblings;
 
```rust
let template = warmpool::TemplatePool::builder(connect_options)
    .migrations_from("./migrations")
    .prune_stale_templates_on_build(true)
    .build()
    .await?;
```
 
With this enabled, the first time this `TemplatePool` builds or reuses its template, it also drops every other database sharing its template prefix once per `TemplatePool`, not once per clone.

Before turning it on anywhere, see what it would actually delete:
 
```rust
let stale = template.stale_template_names().await?;
println!("{stale:?}"); // nothing has been dropped yet
 
let dropped = template.prune_stale_templates().await?; // now it has
```

This is a garbage collection kinda operation over a shared database namespace, not a normal part of template creation. It does not make sense as a destructive default because the default prefix `warmpool_tmpl_` is shared by by more than just the old templates:

- **Multiple migration sets in one process** are an a supported
  use of the builder API, two `TemplatePool`s, two different
  `migrations_from(...)` directories, same default prefix, two different
  fingerprints. Each one's template looks exactly like a stale leftover
  from the other's point of view. With pruning on for both, they would
  spend their time deleting each other's live templates.
- **A shared dev Postgres instance or a shared CI instance**.The
  fingerprint based naming and the advisory lock design in prticular assumes
  unrelated projects might land on the same instance. The pruning logic doesn't
  know "unrelated", it only knows "shares this prefix". Someone else's live
  template, using the same default prefix, is indistinguishable from the stale one
  you are targeting.

If you turn this on, give the `TemplatePool` its own
[`template_prefix`](#builder-api) first, so "shares this prefix" and "belongs to me" mean the same thing. Then you can safely prune stale templates on build without risking collateral damage to other projects. The default prefix is `warmpool_tmpl_`, so if you don't change it, you are sharing a namespace with every other warmpool user on the same Postgres instance.
 
```rust
let template = warmpool::TemplatePool::builder(connect_options)
    .migrations_from("./migrations")
    .template_prefix("myapp_tmpl_")
    .prune_stale_templates_on_build(true)
    .build()
    .await?;
```
 
However, even with a unique prefix, the pruning logic is still careful about what it drops:

- It never drops the template this pool is currently using.
- It never drops a name ending in `_building` for that suffix belongs to
  the crash atomic build logic and may be someone else's build in
  progress right now, for a different fingerprint under the same prefix.
- It matches by plain prefix equality, so lookalike names are never at risk.
 
If you're not sure which option is right for you use manual cleanup.

The macro is a convenience wrapper for the common case, one freshly built or cloned database per test function. A prune on build step is a cross test maintenance operation against a shared template prefix. If every test did this implicitly, different tests in the same run would race to delete templates another test may still be cloning from. The builder API is the right place for this behavior because it is explicit, pool scoped, and opt-in.

## Important considerations

- Cleanup is explicit. Because Rust cannot run async code inside `Drop`, call `drop_database().await` yourself or use `#[warm_test]`.
- `STRATEGY = WAL_LOG`/`FILE_COPY` pinning only applies on Postgres 15+; older servers are unaffected by the `clone_strategy` setting.
- The advisory lock key is derived from the fingerprint, avoiding lock contention between unrelated projects sharing the same Postgres instance.

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
