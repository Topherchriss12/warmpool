# Sharp edges

This document exists so nobody has to discover warmpool's rough edges by being burned by one. Everything below is a **confirmed** limitation reproduced some via tests, and not suspected. Everything in here is a backlog of known issues that we intend to fix, but we don't want to block the current release on them. The goal is to make the rough edges visible, so that users can plan accordingly, and so that contributors can pick them up and work through in public, one reviewed patch at a time.

This is also a guide through the 0.1.x series. Each item below gets its own patch, its own review, and its own changelog entry and never bundled with another fix, never landed silently inside a release whose headline is something else. When an item is closed, it moves out of "Open" and into "Resolved," with a link to the change that closed it. Nothing gets deleted from this file; the resolved section is as much a part of the record as the open one.

If you hit something below, you're not holding it wrong, you found something warmpool is not handling cleanly. If you hit something that *isn't* below, please open an issue; that's a gap in this document, which is its own kind of bug.

## Principles behind the ordering

Four things determine where an item sits in the sequence below, roughly in this order of weight:

1. **How often would a real user actually hit this** in a normal workflow, not an adversarial one.
2. **How loud is the failure when it happens**. A clear error a developer can act on immediately is better than a passing test suite running against broken state. For warmpool, Silent and wrong outranks loud and broken for priority, even at lower frequency.
3. **Is the fix a mechanical bug fix, or does it require a design decision first.** Bug fixes go first; anything that needs an RFC or a trade-off discussion waits until there's an actual decision to implement, and should not block the queue.
4. **Does the fix touch code another queued fix also touches.** If two items share a locked section or a code path, we sequence them so each can be reviewed against a stable base, not against a moving target.

## At a glance

| # | Sharp edge | Status | Target | Kind |
|---|---|---|---|---|
| 1 | No stale connection sweep on the template before cloning | **Resolved** | 0.1.2 | Bug fix |
| 2 | Template construction isn't crash atomic | **Resolved** | 0.1.3 | Bug fix |
| 3 | `purge_triggers_sql` doesn't escape the schema literal | **Resolved** | 0.1.4 | Bug fix |
| 4 | `create_test_database_sql` doesn't escape the template prefix identifier | **Reolved** | 0.1.5 | Bug fix |
| 5 | `exclude_migration`'s actual behavior may not be the behavior it appears to be, atleast for now | Open | pending | Design decision |

Four items **Resolved**. This table is the first thing that changes when something is.

---

## Open

### 1. No stale connection sweep on the template before cloning

**Status:** Resolved · **Target:** 0.1.2 · Failed loudly.

**What's broken:** `create_test_database()` issues `CREATE DATABASE ... TEMPLATE <name> ...` with no guard against other connections to the template. Postgres refuses that statement outright if *anyone* is
connected to the source database not just warmpool's own connections.

Confirmed directly:

```
$ psql -d warmpool_tmpl_xxxx -c 'SELECT pg_sleep(30)'   # held open
$ psql -c 'CREATE DATABASE t TEMPLATE warmpool_tmpl_xxxx STRATEGY = WAL_LOG'
ERROR:  source database "warmpool_tmpl_xxxx" is being accessed by other users
DETAIL:  There is 1 other session using the database.
```

A crashed test process that didn't unwind cleanly, a developer's `psql` session left open while poking at the template to debug something, a monitoring query, a connection pooler's health check. Any of these
blocks *every* subsequent clone until the stray connection closes on its own. 

`TestDatabase::drop_database()` already guards against exactly this class of problem for the database it owns:

```rust
sqlx::query(
    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
     WHERE datname = $1 AND pid <> pg_backend_pid()"
)
```

`create_test_database()` has no equivalent sweep for the template it clones *from*. A project can most likely hit this, as it depends on developer discipline and CI env, but when it does, it fails loudly and completely. Which is why we prioritize it.

**Planned fix:** a `pg_terminate_backend` sweep against the template's `datname`, mirroring `drop_database()`'s existing pattern, run immediately before the `CREATE DATABASE ... TEMPLATE` statement in `create_test_database()`.

**Concerns** Does sweeping the template introduces any new race against a *concurrent template build* the advisory lock already serializes builds against each other, but this sweep runs outside that lock, on the clone path, and needs to be checked against the build path's own connection lifecycle so it can't
accidentally terminate a build that's legitimately in progress. Test coverage should include a test that opens a connection to the template and verifies that `create_test_database()` terminates it and proceeds, and a test that opens a connection to the template while a build is in progress and verifies that the build completes successfully and the clone fails with the expected "template is being accessed by other users" error.

**Tracking:** was filed as its own issue before work began.

---

### 2. Template construction isn't crash-atomic

**Status:** Resolved · **Target:** 0.1.3 · Failed silently, but was worse than it sounds.

**What's broken:** `build_template_if_missing()` creates the template database under its final, fingerprinted name and runs migrations against it in place. If the process building it dies mid-migration like a killed CI
job, an OOM, a migration that panics the process instead of returning an `Err`, the template exists under its expected name with only some migrations applied. The next process to call `ensure_template()` runs its existence check (`SELECT EXISTS (... FROM pg_database WHERE datname =$1)`), finds the row, and treats the template as ready. Every clone from that point on is missing whatever didn't finish applying.

This is worse than it sounds because of *how* it fails downstream: tests don't get an infrastructure error, they get `relation "..." does not exist` from what looks like a real schema bug, on some but not all
tables, and every clone shares the same broken template until someone notices and manually drops it. The existence check has no way to distinguish "fully built" from "build started and never finished" the way it does for "never built" vs. "fully built." The only way to recover is to drop the template and rebuild it, which is a manual operation that requires someone to notice the problem and know how to fix it.

**Why it's ranked second, not first:** the trigger (a crash landing specifically mid-migration) is much rarer than "someone left a psql session open." But a silent, confusing failure mode that poisons a shared
template for an entire team outranks a loud one at lower frequency, this is the more dangerous failure to leave open, just a less likely one.

**Planned fix:** build the template under a `<name>_building` suffix, run every migration against that, and only `ALTER DATABASE <name>_building RENAME TO <name>` after every migration succeeds. The rename is atomic
from Postgres's catalog perspective there is no window where a half migrated database exists under the final name.

**Concerns**. This touches the same advisory lock guarded section that item #1's fix and 0.1.1's
`server_version_num` caching both already touch. Sequencing this after #1 means it reviews against a stable version of that section and not a moving one.
Also, what happens to an orphaned `<name>_building` database left behind by a crash under the *new* code ?
Does a later build attempt clean it up, or does it need its own explicit handling to avoid accumulating half built templates under `_building` names the same way stale full templates already can.

**Tracking:** was filed as its own issue before work began.

---

### 3. `purge_triggers_sql` doesn't escape the schema literal

**Status:** Resolved · **Target:** 0.1.4 · Practical risk is low, but it's a real defect.

**What's broken:** `TemplatePoolBuilder::purge_triggers_in(schema)` interpolates `schema` directly into a single quoted SQL string literal inside a `DO $$ ... $$` block:

```rust
format!("... WHERE nspname = '{schema}' AND NOT tgisinternal ...")
```

A schema name containing a `'` breaks out of the literal. In practice `schema` is a small, fixed, developer supplied configuration value passed once at builder construction time, not end-user or runtime input, which
is the only reason this hasn't been a real-world problem. It's still unescaped, and pool.rs's test suite documents the gap explicitly see`purge_triggers_sql_does_not_escape_embedded_quotes`.

**Why this is its own patch, not bundled with #4:** these are two different code paths a string literal context here, a quoted identifier context in #4 with different escaping rules (doubling `'` vs. doubling
`"`) and different test surfaces. Fixing them together would make either fix harder to review in isolation, and a mistake in one wouldn't be caught by the other's review.

**Planned fix:** escape `schema` before interpolation (double any embedded `'`), or switch to Postgres's `quote_literal()` inside the generated SQL rather than doing string escaping in Rust. Replace the
test that currently documents the gap with one asserting the previously vulnerable input is now handled safely.

**Tracking:** was filed as its own issue before work began.

---

### 4. `create_test_database_sql` doesn't escape the template prefix identifier

**Status:** Open · **Target:** 0.1.5 · Low practical risk, but it's a real defect.

**What's broken:** `TemplatePoolBuilder::template_prefix(prefix)` feeds into a double quoted identifier in the generated `CREATE DATABASE` statement:

```rust
format!(r#"CREATE DATABASE "{db_name}" WITH TEMPLATE "{template_name}"{strategy_clause};"#)
```

`db_name` (a UUID) and the fingerprint half of `template_name` are always safe by construction they're generated by warmpool itself, not supplied by a caller. `template_prefix` is caller supplied, and a prefix
containing a `"` breaks out of the quoted identifier. Same practical risk profile as #3 (a config value, not runtime input), same treatment in the test suite, see `create_test_database_sql_does_not_escape_embedded_quotes_in_names_either`.

**Planned fix:** escape `template_prefix` at the point `TemplatePoolBuilder::template_prefix()` is called, or apply Postgres identifier quoting rules (double any embedded `"`) when constructing the SQL. 
Replace the documenting test with one asserting safe handling.

**Tracking:** was filed as its own issue after #3 to keep the two escaping fixes reviewable independently not as a pair.

---

### 5. `exclude_migration`'s actual behavior may not be the behavior you might mistake it for.

**Status:** Open · **Target:** not yet scheduled to a
version · Design question, not a bug

**What's broken:** nothing, as of 0.1.1, but this feature is a candidate for a design decision that needs to be made before it can be scheduled to a version. The current behavior is exactly as documented: migrations listed in `exclude_migration` are skipped when building the shared template, and never applied to any clone. The question is whether that behavior is actually what people expect when they use `exclude_migration`, or if they expect those migrations to be re-applied fresh against every clone instead of baked into the shared template.

The candidate feature which is re-applying excluded migrations fresh against every cloned test database, rather than baking them into the shared template has a real cost: you'd be paying that migration's execution
time on every clone, which can reintroduce exactly the per-test migration tax warmpool exists to amortize away. For a small seed-data insert that might be negligible; for anything heavier it might not be. That's a
trade-off with an actual right answer that depends on what people are using `exclude_migration` for, which we don't have enough signal on yet.

**Why this doesn't have a target version:** unlike 1-4, there's no mechanical fix waiting to be scheduled. An RFC style issue laying out the trade-off, gathering input from anyone actually
using `exclude_migration` today except from myself, and reaching a decision keep it exactly as documented now, or add an opt-in per-clone re-application mode before any code gets written. It gets a target version once that
decision exists. Until then, it's a design question, not a bug, and it doesn't block the 0.1.x series from shipping.

**Tracking:** RFC issue to be filed; this entry will be updated with a
link once it exists.

---

## Resolved
 
### 4. Identifier interpolation is unescaped across all name-building helpers
 
**Resolved in:** 0.1.5
 
`template_prefix` is caller supplied and flows into every database name warmpool builds. A prefix containing `"` closed the quoted identifier, terminated warmpool's statement, and started a new one.
 
**The fix:** `escape_sql_identifier()` (doubles `"`; leaves backslash alone, since it has no special meaning inside `"..."`), applied to all four helpers `create_test_database_sql`, `create_template_database_sql`,
`rename_database_sql`, and `drop_database_if_exists_sql`. The fourth wasn't in this entry's original scope; it has the same exposure and would have been left open by a narrower fix.
 
**This entry's inherited severity was wrong too, in the opposite direction from #3's.** In fact this was
**real arbitrary SQL execution** strictly worse than #3 and the difference is structural, not incidental:
 
- #3's payload sits inside a `DO $$ ... $$` body, parsed as one unit. A `;` cannot start a new statement there; the ceiling was a widened `WHERE` clause.

- #4's payloads go out as ordinary statements over simple query protocol, where a `;` genuinely does begin the next statement. With the unescaped code, a `template_prefix` of `wp_evil_"; DROP DATABASE wp_victim; --` **dropped `wp_victim`**, a database unrelated to warmpool. Against the fix the same payload is inert, and Postgres reports it as one nonexistent template *name*:
`template database "evil"; DROP DATABASE wp_victim; --" does not exist`.

**[ISSUE#4](https://github.com/Topherchriss12/warmpool/issues/4#issue-5494615289)**


### 3. `purge_triggers_sql` doesn't escape the schema literal
 
**Resolved in:** 0.1.4
 
`TemplatePoolBuilder::purge_triggers_in(schema)` interpolated `schema` directly into a single quoted SQL string literal inside a `DO $$ ... $$` block, unescaped.
 
**The fix:** a new internal `escape_sql_literal()` helper that doubles both `'` and `\` and wraps the result in Postgres's `E'...'` escape string syntax. `E'...'` instead of a plain literal because doubling quotes alone is only sufficient when the server has `standard_conforming_strings = on`, the default since Postgres 9.1, but
not something a pure string-building function can check, since it never
touches a connection. Using `E'...'` makes the semantics explicit and config independent, which is also *why* `\` has to be doubled, otherwise a value ending in a backslash lets `\'` read as an escaped quote instead of a 
closing one.
 
- **This entry's original severity description when we first discovered was underestimated**; we labeled it "low 
    practical risk" and illustrated with a `'; DROP TABLE users; --` payload. Testing the real exploit against a real Postgres instance before writing the fix showed: The interpolation point is inside a dollar quoted `DO $$ ... $$` body, which Postgres parses as a single unit; a stray `;` can't start a new top level statement, it just produces `ERROR: missing "LOOP" at end of SQL expression`. Verified the target table was untouched afterward. A different payload shape works and is genuinely destructive. Widening the `WHERE` clause rather than escaping the statement (`tenant_a' OR '1'='1`) keeps the SQL valid while making the loop iterate over every non-internal trigger in the database. 

    Because `format('%I')` emits an *unqualified* relation name, the generated `DROP TRIGGER` resolves through `search_path`, so with two tenant schemas each holding a trigger, purging `tenant_a` with that payload **destroyed `tenant_b`'s trigger.** Reproduced end to end, then confirmed inert against the fix.
    The accurate characterization is neither "arbitrary SQL execution" (impossible through this code path) nor "low practical risk" (demonstrably destructive), but **silent, out-of-scope trigger destruction in schemas the caller never named**. It still required a hostile or malformed value reaching `purge_triggers_in()`, a developer supplied config value, not runtime input , which is the part of the original assessment that holds up, and why we kept this ranked third.
 
**[ISSUE#3](https://github.com/Topherchriss12/warmpool/issues/3#issue-5451445887)**
 
---

 
### 2. Template construction isn't crash-atomic
 
**Resolved in:** 0.1.3 .
 
`build_template_if_missing()` created the template database under its final, fingerprinted name and ran migrations against it in place. A crash mid migration left a half migrated database sitting under that name; the existence check on the next attempt couldn't tell "fully built" from "started and never finished," so every clone afterward
silently got an incomplete schema failing downstream with `relation "..." does not exist` on some but not all tables, which reads like a real schema bug, not an infrastructure one.
 
**The fix:** build under a `<name>_building` suffix, migrate there in full, and only`ALTER DATABASE <name>_building RENAME TO <name>` after every migration succeeds. From Postgres's catalog perspective the rename
is a single operation, no window exists where a half migrated database sits under the final name.
 
**A concern raised when this was discovered**; what happens to an orphaned `_building` database left behind by a crash under the new code ? Cleanup of any leftover `_building` runs unconditionally at the start of every fresh
build (inside the same advisory lock, so anything found is necessarily an orphan from a past attempt, never a build in progress elsewhere, same reasoning behind #1's concurrency concern). Skipping this cleanup would have made the fix worse than the bug itself: a crashed build would permanently block every future attempt at that fingerprint, since `CREATE DATABASE` would keep failing against the orphan forever.

**Reproduce locally:** you can confirm the same flow on your own Postgres instance, run the script in `scripts/reproduce_orphan_building.sh`:

```bash
PGURL="postgres://postgres:postgres@127.0.0.1:5432/postgres" ./scripts/reproduce_orphan_building.sh
```

The script creates an orphaned `_building` database with a partially applied schema, replays the cleanup -> rebuild -> sweep -> rename flow, and prints the final table list so you can confirm the final template contains the complete schema rather than the orphan's partial state.

***Do not point PGURL at a database containing data you need to preserve and obviously not to a production database***.

**[ISSUE#2](https://github.com/Topherchriss12/warmpool/issues/2#issue-5408173016)**

### 1. No stale-connection sweep on the template before cloning

**Resolved in:** 0.1.2 .

`create_test_database()` issued `CREATE DATABASE ... TEMPLATE <name> ...` with no guard against other connections to the template. Postgres refuses that statement outright if *anyone* is connected to the source
database, not just warmpool's own connections. A crashed test process, a developer's leftover `psql` session, a monitoring query, anything, made every subsequent clone fail until the stray connection closed on its own.

**The fix:** a `pg_terminate_backend` sweep against the template's `datname`, run on the same maintenance connection already open in `create_test_database()`, immediately before the `CREATE DATABASE ...
TEMPLATE` statement mirroring the sweep `TestDatabase::drop_database()` already did for the database it owns. Shared via a `terminate_other_backends()` helper shared by both call sites but serves different purposes: `drop_database()` sweeps the database it owns, `create_test_database()` sweeps the template it clones from. Both are advisory, both are best effort, both are run on the same connection that executes the subsequent statement that needs the sweep to succeed.

**Concerns This Fix Raised**. Does the sweep introduce a race against a legitimate, concurrent template build? The advisory lock guarded section that both the build path and the clone path touch, verifies that `build_template_if_missing()` always closes its own connection to the template before releasing the lock. That means nothing that reaches the sweep (which requires the lock to already be released) can ever be a connection from a build still in progress only a genuine stray.

`test_external_connection_during_a_build_does_not_disrupt_it_or_survive_the_next_sweep` verifies that a connection opened to the template while a build is in progress is not terminated by the sweep, and that the build completes successfully. `test_stray_connection_to_template_no_longer_blocks_cloning` verifies that a connection opened to the template before `create_test_database()` is called is terminated by the sweep, and the clone succeeds.

**What this fix does not close:** a connection can still theoretically race in during the small window between the sweep completing and the `CREATE DATABASE` statement executing. See the `Error::TemplateConnectionSweep` error variant that was added to `warmpool::Error` to make this failure mode explicit. The sweep closes the window on a *pre-existing* stray connection, but it cannot prevent a brand new connection from racing in during that small window. That residual window is accepted, not eliminated, by this fix.

**[ISSUE#1](https://github.com/Topherchriss12/warmpool/issues/1#issue-5384723346)**

---

## What's explicitly out of scope for this series.

The longer-term direction, a `WarmPool` type that keeps pre-cloned databases ready in a background-replenished queue instead of cloning synchronously on every `create_test_database()` call is real, and it's where we're headed after this list is empty. It isn't tracked here on purpose: it's a different API shape, not a fix to an existing one, and mixing "bugs in the current design" with "the next design" in the same
sequential list would make both harder to reason about and debug. It'll get its own
document when work on it actually starts.
