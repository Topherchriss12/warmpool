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