#!/usr/bin/env bash

set -x
set -eo pipefail


CONTAINER_NAME="warmpooltemplatedb"
IMAGE="postgres"
DEFAULT_DB_USER="postgres"
DEFAULT_DB_PASSWORD="warmpooltemplatedbpass123"
DEFAULT_DB_NAME="warmpooltemplatedb"
DEFAULT_DB_PORT="5430"
MAX_CONNECTIONS="${POSTGRES_MAX_CONNECTIONS:=1000}"

command -v psql >/dev/null 2>&1 || {
    echo >&2 "Error: psql is not installed."
    exit 1
}

command -v sqlx >/dev/null 2>&1 || {
    echo >&2 "Error: sqlx is not installed."
    echo >&2 "Install using:"
    echo >&2 "  cargo install --version=0.5.7 sqlx-cli --no-default-features --features postgres"
    exit 1
}


DB_USER="${POSTGRES_USER:=$DEFAULT_DB_USER}"
DB_PASSWORD="${POSTGRES_PASSWORD:=$DEFAULT_DB_PASSWORD}"
DB_NAME="${POSTGRES_DB:=$DEFAULT_DB_NAME}"
DB_PORT="${POSTGRES_PORT:=$DEFAULT_DB_PORT}"


if [[ -z "${SKIP_DOCKER}" ]]; then

    # Docker availability checks
    command -v docker >/dev/null 2>&1 || {
        echo >&2 "Error: Docker is not installed."
        exit 1
    }

    docker info >/dev/null 2>&1 || {
        echo >&2 "Error: Docker is not running."
        exit 1
    }

    # Handle existing container
    if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
            echo "Postgres container '${CONTAINER_NAME}' is already running."
        else
            echo "Starting existing Postgres container '${CONTAINER_NAME}'..."
            docker start "${CONTAINER_NAME}"
        fi
    else
        # Ensure port is free
        if lsof -i:"${DB_PORT}" -t >/dev/null; then
            echo >&2 "Error: Port ${DB_PORT} is already in use."
            exit 1
        fi

        echo "Creating Postgres container '${CONTAINER_NAME}'..."

        docker run \
            --name "${CONTAINER_NAME}" \
            -e POSTGRES_USER="${DB_USER}" \
            -e POSTGRES_PASSWORD="${DB_PASSWORD}" \
            -e POSTGRES_DB="${DB_NAME}" \
            -p "${DB_PORT}":5432 \
            -d "${IMAGE}" \
            postgres -N "${MAX_CONNECTIONS}"
    fi
fi


export PGPASSWORD="${DB_PASSWORD}"

echo "Waiting for Postgres to become available..."

until psql -h "localhost" -U "${DB_USER}" -p "${DB_PORT}" -d "postgres" -c '\q' >/dev/null 2>&1; do
    echo "Postgres is not yet ready — sleeping..."
    sleep 1
done


export DATABASE_URL="postgres://${DB_USER}:${DB_PASSWORD}@localhost:${DB_PORT}/${DB_NAME}"


# export DATABASE_URL="postgres://postgres:warmpooltemplatedbpass123@localhost:5430/warmpooltemplatedb"

# Uncomment the following lines to initialize the database, apply migrations, and run the benchmark.
# Adjust the number of iterations as needed. 


PGPASSWORD='warmpooltemplatedbpass123' \
createdb -h localhost -p 5430 -U postgres warmpooltemplatedb 2>/dev/null || true && \
PGPASSWORD='warmpooltemplatedbpass123' \
psql -h localhost -p 5430 -U postgres -d warmpooltemplatedb -v ON_ERROR_STOP=1 -f ./scripts/migrations/001_create_tables.sql && \
PGPASSWORD='warmpooltemplatedbpass123' \
psql -h localhost -p 5430 -U postgres -d warmpooltemplatedb -v ON_ERROR_STOP=1 -f ./scripts/migrations/002_seed_values.sql && \
PGPASSWORD='warmpooltemplatedbpass123' \
./scripts/bench_clone_strategy.sh localhost 5430 postgres warmpooltemplatedb 20