# Benchmarking and template setup (scripts)

Purpose
-------
This document describes how to prepare a template database for the benchmark and how to run the included clone strategy benchmark (`bench_clone_strategy.sh`). It provides commands and a concise explanation of how to interpret the results.

Prerequisites
-------------
- PostgreSQL server (>= 15 required to exercise `STRATEGY` explicitly; script checks this).
- `psql` and `createdb` client tools installed and compatible with the server (use libpq via `PGCONNECT_TIMEOUT`).
- Shell access to this repository root.
- A role with `CREATEDB` privilege for the benchmark operations.

Files
-----
- `init_db.sh` — convenience script to create the template database and apply migrations (if present).
- `001_create_tables.sql`, `002_seed_values.sql` — example SQL migrations used to populate the template.
- `bench_clone_strategy.sh` — benchmark that measures `CREATE DATABASE ... TEMPLATE` under `WAL_LOG` and `FILE_COPY`.

Quick sequence
-------------------------
From the repository root, run this single command to create the template DB idempotently, apply the two example migrations, and run the benchmark for 10 iterations per strategy:

```bash
PGPASSWORD='warmpooltemplatedbpass123' \
createdb -h localhost -p 5430 -U postgres warmpooltemplatedb 2>/dev/null || true && \
PGPASSWORD='warmpooltemplatedbpass123' \
psql -h localhost -p 5430 -U postgres -d warmpooltemplatedb -v ON_ERROR_STOP=1 -f ./scripts/migrations/001_create_tables.sql && \
PGPASSWORD='warmpooltemplatedbpass123' \
psql -h localhost -p 5430 -U postgres -d warmpooltemplatedb -v ON_ERROR_STOP=1 -f ./scripts/migrations/002_seed_values.sql && \
PGPASSWORD='warmpooltemplatedbpass123' \
./scripts/bench_clone_strategy.sh localhost 5430 postgres warmpooltemplatedb 10
```

Notes
-----
- The `createdb` invocation is safe to run repeatedly (`|| true` swallows "already exists").
- Prefer supplying credentials via `PGPASSFILE` or environment variables in CI
- If your `psql` client rejects `--connect-timeout`, set `PGCONNECT_TIMEOUT` in the environment and ensure `psql` on `PATH` is compatible with the server.

Interpretation of benchmark output
----------------------------------
The benchmark prints per sample latencies and a summary for each strategy. Example excerpt:

```
WAL_LOG     1     550.000 ms
FILE_COPY   1   15730.000 ms
...

Summary
WAL_LOG   avg:   1056.000 ms  p50:    540.000 ms  p95:    550.000 ms  min:    520.000 ms  max:   3150.000 ms  n=5
FILE_COPY avg:  14440.000 ms  p50:  14030.000 ms  p95:  15730.000 ms  min:  12420.000 ms  max:  16590.000 ms  n=5

Results written to ./clone_strategy_results.csv
```

Meaning of fields
- Per sample lines: `STRATEGY <tab> iteration <tab> latency_ms` (latency measured around the `CREATE DATABASE` call).
- Summary fields:
  - `avg`: arithmetic mean of measured latencies for that strategy.
  - `p50`: median (50th percentile).
  - `p95`: 95th percentile, useful for tail behavior.
  - `min`, `max`: range observed.
  - `n`: number of measured samples (does not include warm-ups).

How to interpret results
- If `WAL_LOG` shows substantially lower `avg` and tighter `p95`/`max` than `FILE_COPY`, prefer `WAL_LOG` for test/CI cloning on this environment.
- If `FILE_COPY` is faster, the environment or template size may favour filesystem level copying (large templates or copy-on-write capable filesystems).
- The benchmark is environment dependent; re-run when template content, PostgreSQL version, filesystem, or concurrency characteristics change.

- Run at least one warm-up and 10+ measured iterations for more stable results (adjust the `N` parameter).
- For CI, run the benchmark on representative runners (same disk type, container config) not on development laptop.
- Consider measuring concurrent cloning scenarios if your tests run in parallel.

Troubleshooting
---------------
- `psql: unrecognized option '--connect-timeout=...'` — set `PGCONNECT_TIMEOUT` and/or use a newer `psql` on `PATH`.
- `template database does not exist` — run the `createdb` + migrations step first.
- Cleanup warnings about leftover `wp_bench_*` databases — run the cleanup loop in `bench_clone_strategy.sh` again or drop those databases manually; ensure the role has appropriate permissions.

Contact
-------
For questions about the benchmark or interpretation, please open an issue in this repository.
Or email the maintainer at `n42611740@gmail.com`. 
