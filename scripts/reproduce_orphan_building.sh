#!/usr/bin/env bash
set -Eeuo pipefail

PGURL="${PGURL:-postgres://postgres:postgres@127.0.0.1:5432/postgres}"
FINAL_DB="${FINAL_DB:-warmpool_tmpl_demo}"
BUILDING_DB="${BUILDING_DB:-${FINAL_DB}_building}"

parse_pgurl() {
    local url="$1"
    local rest auth hostport dbname host port user pass

    rest="${url#*://}"
    auth="${rest%@*}"

    if [[ "$rest" == "$auth" ]]; then
        hostport="${rest%%/*}"
        dbname="${rest#*/}"
        user="${PGUSER:-postgres}"
        pass="${PGPASSWORD:-}"
    else
        hostport="${rest#*@}"
        dbname="${hostport#*/}"
        hostport="${hostport%%/*}"

        if [[ "$auth" == *:* ]]; then
            user="${auth%%:*}"
            pass="${auth#*:}"
        else
            user="$auth"
            pass="${PGPASSWORD:-}"
        fi
    fi

    if [[ "$hostport" == *:* ]]; then
        host="${hostport%:*}"
        port="${hostport##*:}"
    else
        host="$hostport"
        port="5432"
    fi

    export PGHOST="${PGHOST:-$host}"
    export PGPORT="${PGPORT:-$port}"
    export PGUSER="${PGUSER:-$user}"
    export PGDATABASE="${PGDATABASE:-${dbname:-postgres}}"

    if [[ -n "$pass" ]]; then
        export PGPASSWORD="${PGPASSWORD:-$pass}"
    fi
}

parse_pgurl "$PGURL"

validate_db_name() {
    [[ "$1" =~ ^[a-zA-Z_][a-zA-Z0-9_]*$ ]] || {
        printf 'Invalid database name: %s\n' "$1" >&2
        exit 2
    }
}

validate_db_name "$FINAL_DB"
validate_db_name "$BUILDING_DB"

if [[ "$FINAL_DB" == "$BUILDING_DB" ]]; then
    printf 'FINAL_DB and BUILDING_DB must be different\n' >&2
    exit 2
fi

cleanup() {
    local status=$?

    if [[ "$status" -ne 0 ]]; then
        printf '\nThe reproduction failed; removing demo databases...\n' >&2
    else
        printf '\nRemoving demo databases...\n'
    fi

    psql \
        -h "$PGHOST" \
        -p "$PGPORT" \
        -U "$PGUSER" \
        -d "$PGDATABASE" \
        -v "final_db=$FINAL_DB" \
        -v "building_db=$BUILDING_DB" \
        -v ON_ERROR_STOP=1 \
        <<'SQL' >/dev/null 2>&1 || true
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE datname IN (:'final_db', :'building_db')
  AND pid <> pg_backend_pid();

DROP DATABASE IF EXISTS :"building_db";
DROP DATABASE IF EXISTS :"final_db";
SQL

    exit "$status"
}

trap cleanup EXIT

cat <<EOF
Reproducing the orphaned _building database scenario against a live PostgreSQL instance.

Final database:   ${FINAL_DB}
Building database: ${BUILDING_DB}
EOF

printf '\nRemoving any previous demo databases...\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$PGDATABASE" \
    -v "final_db=$FINAL_DB" \
    -v "building_db=$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE datname IN (:'final_db', :'building_db')
  AND pid <> pg_backend_pid();

DROP DATABASE IF EXISTS :"building_db";
DROP DATABASE IF EXISTS :"final_db";
SQL

printf '\nCreating partial orphan database...\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$PGDATABASE" \
    -v "building_db=$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
CREATE DATABASE :"building_db";
SQL

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE
);
SQL

printf 'Partial orphan state:\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
ORDER BY table_name;
SQL

printf '\nVerifying that the final database does not exist...\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$PGDATABASE" \
    -v "final_db=$FINAL_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT CASE
    WHEN EXISTS (
        SELECT 1
        FROM pg_database
        WHERE datname = :'final_db'
    )
    THEN pg_catalog.format(
        'ERROR: database %I already exists',
        :'final_db'
    )
    ELSE 'OK: final database does not exist'
END;
SQL

printf '\nRunning cleanup, rebuild, and rename flow...\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$PGDATABASE" \
    -v "building_db=$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE datname = :'building_db'
  AND pid <> pg_backend_pid();

DROP DATABASE IF EXISTS :"building_db";
CREATE DATABASE :"building_db";
SQL

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE
);

CREATE TABLE widgets (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE gadgets (
    id SERIAL PRIMARY KEY,
    value TEXT NOT NULL
);
SQL

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$PGDATABASE" \
    -v "final_db=$FINAL_DB" \
    -v "building_db=$BUILDING_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT pg_terminate_backend(pid)
FROM pg_stat_activity
WHERE datname = :'building_db'
  AND pid <> pg_backend_pid();

ALTER DATABASE :"building_db" RENAME TO :"final_db";
SQL

printf '\nFinal template state:\n'

psql \
    -h "$PGHOST" \
    -p "$PGPORT" \
    -U "$PGUSER" \
    -d "$FINAL_DB" \
    -v ON_ERROR_STOP=1 \
    <<'SQL'
SELECT table_name
FROM information_schema.tables
WHERE table_schema = 'public'
ORDER BY table_name;
SQL

printf '\nReproduction succeeded.\n'
printf 'The partial %s database was discarded, rebuilt, and renamed to %s.\n' \
    "$BUILDING_DB" "$FINAL_DB"