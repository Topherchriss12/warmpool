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
| 4 | `create_test_database_sql` doesn't escape the template prefix identifier | **Resolved** | 0.1.5 | Bug fix |
| 5 | `exclude_migration`'s actual behavior may not be the behavior it appears to be, atleast for now | Open | pending | Design decision |
| 6 | Stale warmpool templates linger after migration churn and require manual cleanup | **Resolved** | 0.1.6 | DevX |
| 7 | Identifier truncation makes `_building` collide with the template name | **Resolved** | 0.1.7 | Bug fix |

Six items **Resolved**. This table is the first thing that changes when something is.

---

## Open

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
 
### 7. Identifier truncation makes `_building` collide with the template name
 
**Resolved in:** 0.1.7
 
Postgres truncates identifiers to 63 bytes with a `NOTICE`, not an error. `build_template_if_missing()` derives `format!("{template_name}_building")`; with a long enough prefix the suffix truncated away and the two names became byte-identical, turning the crash-atomic rename into a rename to self that failed with `database "..." already exists` then *self-healed* on the next call, since `CREATE DATABASE` had already left a migrated database under the final name. An unexplained first call failure followed by everything working.
 
**The fix:** reject over long prefixes at `build()` with `Error::TemplatePrefixTooLong { prefix, actual, limit }`.
 
The budget is `63 - fingerprint_len - len("_building")` — 38 bytes for the standard 16-character fingerprint, because the `_building` name, not the template name, is the longest identifier warmpool constructs. A 38 bytes produces exactly 63 and no truncation NOTICE; 39 truncates. Measured in **bytes**, since Postgres truncates by byte and not encoding aware, so a char based check would wrongly accept multi byte prefixes.
 
Validation lives in `build()` not `template_prefix()` because the budget depends on the fingerprint length, which isn't known until migrations are loaded, still before any database work, so the caller gets a typed error immediately.

**[ISSUE#7](https://github.com/Topherchriss12/warmpool/issues/6#issue-5522260054)**
 
---
 
### 6. Stale warmpool templates linger after migration churn
 
**Resolved in:** 0.1.6

Changing migrations changes the template fingerprint, which changes the template name. The new template is built alongside the old one under a different name, so there is no "drop before create" step to reclaim space. The real cost is accumulation: stale templates pile up on long-lived dev or CI instances, wasting disk space and cluttering the namespace.

**The fix:** added cleanup helpers and a builder option to make the operation intentional. `TemplatePool::stale_template_names()` lists what pruning would remove without deleting anything; `TemplatePool::prune_stale_templates()` deletes the stale templates and returns the names it dropped; and `TemplatePoolBuilder::prune_stale_templates_on_build(bool)` runs the prune once per pool while the template is being built or reused, but defaults to `false`.

This is a lifecycle/housekeeping problem, not a correctness bug, so the default remains off. The name prefix is shared by sibling migration sets in the same process and by unrelated projects on a shared Postgres instance, and a destructive default would silently delete another pool's live template on every build. The implementation uses plain byte-prefix equality rather than `LIKE`, skips names ending in `_building`, and excludes template databases so the prune stays scoped to stale warmpool templates instead of other databases that merely resemble the prefix.

The option is fully opt-in, dry run friendly, and does not change the normal fast path for users who do not want automatic cleanup. It solves the stale template accumulation problem without forcing a destructive default onto shared environments.

**[ISSUE#6](https://github.com/Topherchriss12/warmpool/issues/5#issue-5522247570)**

---

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

---


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

---


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
