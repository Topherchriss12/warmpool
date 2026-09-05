#!/usr/bin/env bash
#
# Benchmark PostgreSQL CREATE DATABASE ... TEMPLATE strategies.
#
# Measures CREATE DATABASE latency only.
# DROP DATABASE cleanup is excluded from the measurements.
#
# Usage:
#   ./bench_clone_strategy.sh HOST PORT USER TEMPLATE [N] [PREFIX]
#
# Example:
#   PGPASSWORD='secret' \
#   PGCONNECT_TIMEOUT=10 \
#   ./bench_clone_strategy.sh \
#       localhost 5430 postgres warmpooltemplatedb 20 wp_bench
#
# Optional environment variables:
#   PGDATABASE        Administrative database. Default: postgres
#   PGCONNECT_TIMEOUT Connection timeout in seconds. Default: 10
#   WARMUPS           Warm-up clones per strategy. Default: 1
#   KEEP_DATABASES    Set to 1 to retain benchmark databases. Default: 0
#   OUTPUT_CSV        CSV output path. Default: ./clone_strategy_results.csv
#   SEED              Randomization seed. Default: current Unix timestamp
#
# Requirements:
#   - bash
#   - psql
#   - PostgreSQL 15 or newer
#   - A role with CREATEDB privilege
#

set -Eeuo pipefail
IFS=$' \t\n'

readonly SCRIPT_NAME=${0##*/}
readonly ADMIN_DB=${PGDATABASE:-postgres}
readonly CONNECT_TIMEOUT=${PGCONNECT_TIMEOUT:-10}
readonly WARMUPS=${WARMUPS:-1}
readonly KEEP_DATABASES=${KEEP_DATABASES:-0}
readonly OUTPUT_CSV=${OUTPUT_CSV:-./clone_strategy_results.csv}
readonly SEED=${SEED:-$(date +%s)}

HOST=${1:?Usage: $SCRIPT_NAME HOST PORT USER TEMPLATE [N] [PREFIX]}
PORT=${2:?Usage: $SCRIPT_NAME HOST PORT USER TEMPLATE [N] [PREFIX]}
PGUSER=${3:?Usage: $SCRIPT_NAME HOST PORT USER TEMPLATE [N] [PREFIX]}
TEMPLATE=${4:?Usage: $SCRIPT_NAME HOST PORT USER TEMPLATE [N] [PREFIX]}
N=${5:-20}
PREFIX=${6:-wp_bench}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

info() {
    printf '%s\n' "$*" >&2
}

command -v psql >/dev/null 2>&1 ||
    die "psql is required"

command -v awk >/dev/null 2>&1 ||
    die "awk is required"

command -v mktemp >/dev/null 2>&1 ||
    die "mktemp is required"

command -v date >/dev/null 2>&1 ||
    die "date is required"

[[ "$PORT" =~ ^[0-9]+$ ]] ||
    die "port must be numeric"

(( PORT >= 1 && PORT <= 65535 )) ||
    die "port must be between 1 and 65535"

[[ "$N" =~ ^[0-9]+$ ]] ||
    die "N must be a positive integer"

(( N >= 1 )) ||
    die "N must be at least 1"

[[ "$WARMUPS" =~ ^[0-9]+$ ]] ||
    die "WARMUPS must be a non-negative integer"

[[ "$CONNECT_TIMEOUT" =~ ^[0-9]+$ ]] ||
    die "PGCONNECT_TIMEOUT must be an integer"

(( CONNECT_TIMEOUT >= 2 )) ||
    die "PGCONNECT_TIMEOUT must be at least 2 seconds"

[[ "$KEEP_DATABASES" == "0" || "$KEEP_DATABASES" == "1" ]] ||
    die "KEEP_DATABASES must be 0 or 1"

# psql does not accept --connect-timeout.
# libpq reads this value from PGCONNECT_TIMEOUT.
export PGCONNECT_TIMEOUT="$CONNECT_TIMEOUT"

validate_component() {
    local name=$1
    local value=$2

    [[ "$value" =~ ^[a-z_][a-z0-9_]*$ ]] ||
        die "$name must contain only lowercase letters, digits, and underscores, and start with a letter or underscore"

    ((${#value} <= 40)) ||
        die "$name must be at most 40 characters"
}

validate_component "template database name" "$TEMPLATE"
validate_component "benchmark prefix" "$PREFIX"

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/clone-bench.XXXXXX")
RESULTS="$WORKDIR/results.tsv"
ORDER="$WORKDIR/order.txt"
PSQL_LOG="$WORKDIR/psql.log"

cleanup_databases() {
    local strategy
    local iteration
    local db

    if [[ "$KEEP_DATABASES" == "1" ]]; then
        return 0
    fi

    for strategy in WAL_LOG FILE_COPY; do
        for ((iteration = 0; iteration <= N + WARMUPS; iteration++)); do
            db=$(createdb_name "$strategy" "$iteration")

            psql_admin -c \
                "DROP DATABASE IF EXISTS ${db} WITH (FORCE)" \
                >/dev/null 2>&1 || {
                    warn "could not drop $db; manual cleanup may be required"
                }
        done
    done
}

on_exit() {
    local status=$?

    cleanup_databases || true
    rm -rf "$WORKDIR"

    exit "$status"
}

on_signal() {
    exit 130
}

trap on_exit EXIT
trap on_signal INT TERM HUP

PSQL_BASE=(
    psql
    --no-password
    --no-psqlrc
    --quiet
    --no-align
    --tuples-only
    --set=ON_ERROR_STOP=1
    --host="$HOST"
    --port="$PORT"
    --username="$PGUSER"
    --dbname="$ADMIN_DB"
)

psql_admin() {
    "${PSQL_BASE[@]}" "$@"
}

createdb_name() {
    local strategy=$1
    local iteration=$2

    printf '%s_%s_%s_%s' \
        "$PREFIX" \
        "$$" \
        "$strategy" \
        "$iteration"
}

check_connection() {
    psql_admin -c 'SELECT 1' >/dev/null ||
        die "could not connect to database '$ADMIN_DB'"
}

check_connection

server_version_num=$(psql_admin -Atc 'SHOW server_version_num')
server_version=$(psql_admin -Atc 'SHOW server_version')

(( server_version_num >= 150000 )) ||
    die "PostgreSQL 15 or newer is required for CREATE DATABASE STRATEGY"

template_exists=$(psql_admin -Atc \
    "SELECT EXISTS (
        SELECT 1
        FROM pg_database
        WHERE datname = '${TEMPLATE}'
    )")

[[ "$template_exists" == "t" ]] ||
    die "template database does not exist: $TEMPLATE"

template_size=$(psql_admin -Atc \
    "SELECT pg_size_pretty(pg_database_size('${TEMPLATE}'))")

current_user=$(psql_admin -Atc 'SELECT current_user')

can_createdb=$(psql_admin -Atc \
    "SELECT rolcreatedb
       FROM pg_roles
      WHERE rolname = current_user")

[[ "$can_createdb" == "t" ]] ||
    die "role '$current_user' does not have CREATEDB privilege"

template_connections=$(psql_admin -Atc \
    "SELECT count(*)
       FROM pg_stat_activity
      WHERE datname = '${TEMPLATE}'
        AND pid <> pg_backend_pid()")

if [[ "$template_connections" != "0" ]]; then
    warn "template database has $template_connections active connection(s)"
    warn "the benchmark may fail if the template is being modified"
fi

drop_database() {
    local db=$1

    psql_admin -c "DROP DATABASE IF EXISTS ${db} WITH (FORCE)" \
        >/dev/null 2>"$PSQL_LOG" || {
            cat "$PSQL_LOG" >&2
            return 1
        }
}

probe_strategy() {
    local strategy=$1
    local db

    db=$(createdb_name "$strategy" 0)

    if ! psql_admin -c \
        "CREATE DATABASE ${db}
         TEMPLATE ${TEMPLATE}
         STRATEGY = ${strategy}" \
        >/dev/null 2>"$PSQL_LOG"; then
        cat "$PSQL_LOG" >&2
        drop_database "$db" || true
        die "$strategy probe failed"
    fi

    drop_database "$db" ||
        die "could not remove $strategy probe database $db"
}

probe_strategy WAL_LOG
probe_strategy FILE_COPY

monotonic_ms() {
    local value

    if [[ -r /proc/uptime ]]; then
        awk '{ printf "%.3f\n", $1 * 1000 }' /proc/uptime
        return
    fi

    value=$(date +%s%3N 2>/dev/null) ||
        die "could not obtain a clock reading"

    [[ "$value" =~ ^[0-9]+$ ]] ||
        die "clock did not return a numeric value"

    printf '%s\n' "$value"
}

create_database() {
    local strategy=$1
    local db=$2

    psql_admin -c \
        "CREATE DATABASE ${db}
         TEMPLATE ${TEMPLATE}
         STRATEGY = ${strategy}" \
        >"$PSQL_LOG" 2>&1
}

generate_order() {
    awk -v n="$N" -v warmups="$WARMUPS" -v seed="$SEED" '
        BEGIN {
            srand(seed)

            for (i = 1; i <= warmups; i++) {
                if (rand() < 0.5) {
                    print "WARMUP WAL_LOG " i
                    print "WARMUP FILE_COPY " i
                } else {
                    print "WARMUP FILE_COPY " i
                    print "WARMUP WAL_LOG " i
                }
            }

            for (i = 1; i <= n; i++) {
                if (rand() < 0.5) {
                    print "MEASURE WAL_LOG " i
                    print "MEASURE FILE_COPY " i
                } else {
                    print "MEASURE FILE_COPY " i
                    print "MEASURE WAL_LOG " i
                }
            }
        }
    '
}

: > "$ORDER"
generate_order > "$ORDER"

printf 'strategy\titeration\tlatency_ms\n' > "$RESULTS"

info "PostgreSQL clone strategy benchmark"
info "host:              $HOST"
info "port:              $PORT"
info "user:              $current_user"
info "admin database:    $ADMIN_DB"
info "server version:    $server_version"
info "template:          $TEMPLATE"
info "template size:     $template_size"
info "iterations:        $N per strategy"
info "warmups:           $WARMUPS per strategy"
info "connect timeout:   ${CONNECT_TIMEOUT}s"
info "seed:              $SEED"
info "results:           $OUTPUT_CSV"
info

while read -r phase strategy iteration; do
    db=$(createdb_name "$strategy" "$iteration")

    start_ms=$(monotonic_ms)

    if ! create_database "$strategy" "$db"; then
        cat "$PSQL_LOG" >&2
        die "$strategy iteration $iteration failed"
    fi

    end_ms=$(monotonic_ms)

    latency_ms=$(awk \
        -v start="$start_ms" \
        -v end="$end_ms" \
        'BEGIN { printf "%.3f\n", end - start }')

    if [[ "$phase" == "MEASURE" ]]; then
        printf '%s\t%s\t%s\n' \
            "$strategy" \
            "$iteration" \
            "$latency_ms" >> "$RESULTS"

        printf '%-9s %3d  %10.3f ms\n' \
            "$strategy" \
            "$iteration" \
            "$latency_ms" >&2
    fi

    if [[ "$KEEP_DATABASES" != "1" ]]; then
        drop_database "$db" ||
            die "cleanup failed for $db"
    fi
done < "$ORDER"

output_dir=$(dirname "$OUTPUT_CSV")
mkdir -p "$output_dir"

{
    printf 'strategy,iteration,latency_ms\n'

    awk -F '\t' '
        NR > 1 {
            printf "%s,%s,%s\n", $1, $2, $3
        }
    ' "$RESULTS"
} > "$OUTPUT_CSV"

print_summary() {
    local wanted=$1

    awk -F '\t' -v wanted="$wanted" '
        NR > 1 && $1 == wanted {
            values[++n] = $3
            sum += $3

            if (n == 1 || $3 < min) min = $3
            if (n == 1 || $3 > max) max = $3
        }

        END {
            if (n == 0) {
                printf "%-9s no samples\n", wanted
                exit
            }

            for (i = 1; i <= n; i++) {
                for (j = i + 1; j <= n; j++) {
                    if (values[j] < values[i]) {
                        tmp = values[i]
                        values[i] = values[j]
                        values[j] = tmp
                    }
                }
            }

            p50_index = int((n - 1) * 0.50) + 1
            p95_index = int((n - 1) * 0.95) + 1

            printf "%-9s avg: %10.3f ms  p50: %10.3f ms  p95: %10.3f ms  min: %10.3f ms  max: %10.3f ms  n=%d\n",
                wanted,
                sum / n,
                values[p50_index],
                values[p95_index],
                min,
                max,
                n
        }
    ' "$RESULTS"
}

printf '\nSummary\n' >&2
print_summary WAL_LOG >&2
print_summary FILE_COPY >&2

if [[ "$KEEP_DATABASES" == "1" ]]; then
    warn "benchmark databases were retained because KEEP_DATABASES=1"
fi

printf '\nResults written to %s\n' "$OUTPUT_CSV"