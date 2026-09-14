# Changelog

## 0.1.0

- Initial release of warmpool.
- Added template based Postgres test database creation.
- Added fingerprint based template invalidation.
- Added builder API and `#[warm_test]` macro support.


## 0.1.1

### Added

- `TemplatePoolBuilder::clone_strategy(CloneStrategy)` — controls the `CREATE DATABASE ... STRATEGY` clause warmpool sends when cloning the template for each test database.
- New `CloneStrategy` enum with three variants: `WalLog`, `FileCopy`, `Auto`.
- `#[warm_test(clone_strategy = "wal_log" | "file_copy" | "auto")]` — the macro exposes the same option. An invalid value is a explicit **compile-time** error (`WarmTestArgs` validates it during attribute parsing).
- `Error::ServerVersionCheck` / `Error::ServerVersionParse` — the two new failure modes introduced by the version detection this feature needs.

### Changed

- **Default clone behavior.** `create_test_database()` now explicitly requests `STRATEGY = WAL_LOG` on Postgres 15+ instead of omitting the clause and relying on Postgres's internal default behaviour. Benchmarked on a disk backed instance: ~3.5x lower average clone latency, and the bigger deal for CI, no more occasional
multi-hundred-millisecond stalls under `FILE_COPY`. See the "Clone strategy" section of the README for the full numbers and the script used.
- `TemplatePool::create_test_database()` now does one extra query the *first* time a given `TemplatePool` builds or reuses its template (`SELECT current_setting('server_version_num')`), cached for the lifetime of the pool via the same `OnceCell` that already caches the template name not a per-clone round trip that would have defeated the purpose of this release's performance improvement.
- **`Error` is now `#[non_exhaustive]`.** This is the deliberate, adding new variants to a public enum is a breaking change for any downstream code that exhaustively `match`es on `warmpool::Error` without a wildcard arm, regardless of whether the num is later marked `#[non_exhaustive]` or not (something we had overlooked). Marking it now means future variant additions (there will be more) won't force a major version bump each time, but it's *this* release that changes the exhaustiveness contract. We are being explicit and honest about it now, but the change is retroactive: if you have downstream code that exhaustively matches on `warmpool::Error`, it needs a wildcard arm as of 0.1.1.

### Compatibility

- Postgres < 15: no behavior change. Those servers don't understand `STRATEGY` at all, so warmpool detects this via `server_version_num` and silently falls back to the pre 0.1.1 behavior (`CloneStrategy::Auto`, no clause sent), regardless of what you configured via builder or macro.
- This is a **runtime behavior change on Postgres 15+, not an API break** aside from the `#[non_exhaustive]` note above. Existing repo compiles and runs unmodified; the only difference is which strategy Postgres uses internally to satisfy the same `CREATE DATABASE` call. If you have a reason to want the old behavior back (something like, you've already benchmarked your own schema and `FILE_COPY` wins for you), call `.clone_strategy(CloneStrategy::Auto)` on the builder or `#[warm_test(clone_strategy = "auto")]` on the macro.

### Related ideas

Two related ideas came out of the same investigation and are deliberately **not** included here, since they're a bigger structural change than a patch release should carry. 

- **Atomic template construction.** `build_template_if_missing()` currently creates the template database under its final fingerprinted name and runs migrations against it in place; a process crash mid migration however rare could in principle leave a half migrated database that a later run's `pg_database` existence check would treat as ready. The fix is to build under a `..._building` suffix, `ALTER DATABASE ... RENAME` on success which somewhat straightforward but touches the same locked section this release already touches, and deserves its own review pass.
- **Background-replenished warm pool** The larger direction is a `WarmPool` type that keeps N pre-cloned databases ready via a bounded channel, so `checkout()` latency drops to near zero instead of paying clone latency synchronously. This is a genuinely different API shape, not a tweak to the existing one, and is the direction we intend to take as the next goal after the current template/clone work is hardened. The execution path is to keep the present 0.1.x work focused and then build this as a separate, reviewed follow on effort: first defining the replenishment loop, then explicit pool sizing and backpressure semantics, and only then shipping the new `WarmPool` API as a 0.2.0 feature.

Anyone reading this: please note that the above two items are **not** part of this release, but are included here to document the thinking and tradeoffs that led to the current design. And to make it clear that the next steps are already on the roadmap, so that users can plan accordingly.

- No new dependencies. This release only adds code.


## 0.1.2

### Fixed

- **No stale connection sweep on the template before cloning** (EDGES.md #1). `create_test_database()` now clears stray connections from the template  via `pg_terminate_backend`, on the same maintenance connection already open, immediately before the `CREATE DATABASE ... TEMPLATE ...` statement before every clone. Previously, any connection to the template that wasn't warmpool's own (a crashed test process, a developer's leftover `psql` session, a monitoring query) made every subsequent clone fail outright with `source database "..." is being accessed by other users` until that connection closed on its own.


### Added

- `Error::TemplateConnectionSweep`; The new failure mode if the sweep itself can't run (the clone attempt is aborted before the `CREATE DATABASE` statement in that case, same as any other pre-clone failure).

- `terminate_other_backends()`; an internal helper shared between the new template sweep in `create_test_database()` and the existing test database sweep in `TestDatabase::drop_database()`, which already did this same kind of sweep for the database it owns. Same query, two call sites, two different `Error` variants at each. This is a small refactor to avoid duplicating the query text while implementing the fix.

- Two integration tests:
  - `test_stray_connection_to_template_no_longer_blocks_cloning`; Replaces the test that used to document this gap as a known, unfixed failure. Now asserts the clone succeeds despite a lingering connection, and that the specific connection the test opened was actually terminated by the sweep, not merely that the clone happened to succeed some other way.

  - `test_external_connection_during_a_build_does_not_disrupt_it_or_survive_the_next_sweep`; A concern raised in `EDGES.md` #1: does the sweep introduce a race against a legitimate, concurrent template build? Answer: it can't, and it is by construction. `build_template_if_missing()` always closes its own connection to the template before the advisory lock is released, so nothing reaching the sweep step (which requires the lock to have already been released) can ever be a connection from a build still in progress. This test proves the observable half of this claim: an external connection opened *during* a cold build doesn't disrupt the build, and is itself cleanly swept away on the next clone attempt afterward.

### Compatibility

No breaking changes. `Error::TemplateConnectionSweep` is additive for `Error` is `#[non_exhaustive]` as of 0.1.1.

No new dependencies. This release fixes a bug.


## 0.1.3
 
### Fixed
 
- **Template construction atomicity** (EDGES.md #2). `build_template_if_missing()` used to create the template database under its final, fingerprinted name and run migrations against it in place. A crash mid-migration like a killed CI job, an OOM, a migration panicking the process left a half migrated database sitting under that name; the existence check on the next attempt had no way to tell "fully built" from "started and never finished," so every clone afterward silently got an incomplete schema, failing downstream with `relation "..." does not exist` on some but not all tables rather than a clear infrastructure error.

The template is now built under a `<name>_building` suffix, migrated there in full, and only `ALTER DATABASE <name>_building RENAME TO <name>` after every migration succeeds. From Postgres's catalog perspective the rename is a single operation there is no window where a half migrated database exists under the final name.

### Added
 
- `Error::CleanupStaleBuildingDb`; fired if clearing away a `_building` database left over from a previous crashed attempt fails. This cleanup runs unconditionally at the start of every fresh build (not just when an orphan is suspected. `DROP DATABASE IF EXISTS` against a name that was never created is a no-op), because skipping it would mean a single crashed build permanently blocks every future attempt at that fingerprint: `CREATE DATABASE` would keep failing with "already exists" against the orphan, forever.

- `Error::TemplateRename`; fired if the final promotion step itself fails. On failure, `_building` is left fully migrated but not yet promoted; the next build attempt's `CleanupStaleBuildingDb` sweep finds it, drops it, and rebuilds from scratch and does not try to resume or reuse it.

- `terminate_other_backends()` is joined by a new `sweep_and_drop_database()` helper (sweep, then `DROP DATABASE IF EXISTS`) shared between `TestDatabase::drop_database()` and the new orphan cleanup, same small supporting refactor pattern as `terminate_other_backends()` itself in 0.1.2.

- Two new pure function unit tests (`rename_database_sql`, `drop_database_if_exists_sql`) alongside the existing SQL builder tests.

- Two new integration tests:
  - `test_orphaned_building_database_is_cleaned_up_and_rebuilt`; constructs the exact end state a crash would leave behind (a `_building` database with only the first of several migrations applied, no final name database at all) by going around the public API the same way `warmpool::fingerprint` is already used elsewhere in the suite to reconstruct names, then drives an ordinary `create_test_database()` call and confirms the orphan is dropped and a **complete** fresh template is built, not that the half built one is resumed or patched.

  - `test_template_is_reusable_normally_after_orphan_recovery`; confirms that once an orphan has been cleaned up and rebuilt once, later clones from the same `TemplatePool` take the ordinary fast path (existence check finds it, returns immediately) and does not re-triggering cleanup every time.

### Design context and physical verification

`ALTER DATABASE ... RENAME` has the same active connection restriction as `CREATE DATABASE ... TEMPLATE`, which Postgres enforces with the same SQLSTATE (`55006`). In practice, this means the final promotion step also needs a connection sweep immediately before it runs; a stray connection can attach to the `_building` database while migrations are still finishing and persist long enough to block the rename even after the build's own connection has been closed.

To keep the error reporting accurate in both places, `Error::TemplateConnectionSweep` uses the broader wording "failed to clear stray connections from `{name}`". The variant is the same one used before cloning and during the final rename step; this is a wording only adjustment to an existing, non-exhaustive enum variant, not a structural change.

In development the cleanup, rebuild, and rename flow was validated against a live Postgres instance before being added to the automated test suite. In that verification, a `_building` orphan with a partial schema was constructed by hand, the exact sequence used by the library was executed step by step, and the final state was confirmed: the orphan was removed, the final template database existed with the complete schema, and no partial state remained behind. This also confirmed the live Postgres behavior behind the rename restriction before it was relied on in the implementation.

### Compatibility

No breaking changes.

No new dependencies. This release fixes a bug.
