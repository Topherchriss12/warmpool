use crate::error::{Error, Result};
use crate::fingerprint::{fingerprint, lock_key_from_fingerprint};
use crate::strategy::CloneStrategy;
use sqlx::migrate::{Migration, Migrator};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, Executor, PgConnection, PgPool};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use uuid::Uuid;

type ExcludeFn = Arc<dyn Fn(&Migration) -> bool + Send + Sync>;

/// The template's name plus the server's numeric version, cached together
/// so [`CloneStrategy::sql_clause`] doesn't need a second round trip on
/// every [`TemplatePool::create_test_database`] call.
#[derive(Clone)]
struct TemplateInfo {
    name: String,
    server_version_num: i32,
}

/// Configures a [`TemplatePool`]. Use this directly when you need control
/// beyond what `#[warm_test]` gets you including multiple migration sets in one
/// process, non-default trigger purging, a custom template name prefix...
#[derive(Clone)]
pub struct TemplatePoolBuilder {
    connect_options: PgConnectOptions,
    migrations_path: Option<PathBuf>,
    exclude: Option<ExcludeFn>,
    purge_schemas: Vec<String>,
    template_prefix: String,
    fingerprint_salt: Option<String>,
    clone_strategy: CloneStrategy,
}

impl TemplatePoolBuilder {
    pub fn new(connect_options: PgConnectOptions) -> Self {
        Self {
            connect_options,
            migrations_path: None,
            exclude: None,
            purge_schemas: Vec::new(),
            template_prefix: "warmpool_tmpl_".to_string(),
            fingerprint_salt: None,
            clone_strategy: CloneStrategy::default(),
        }
    }

    /// Directory containing `.sql` migration files. Defaults to `./migrations`
    /// relative to the process's working directory.
    pub fn migrations_from(mut self, path: impl Into<PathBuf>) -> Self {
        self.migrations_path = Some(path.into());
        self
    }

    /// Exclude migrations matching a predicate from the template build,
    /// the common case being seed data migrations you don't want baked into
    /// every test database.
    ///
    /// Currently, an excluded migration is dropped entirely:
    /// it is never applied to the template, it is **not** applied to
    /// test databases created via [`TemplatePool::create_test_database`]
    /// either, since cloning a test database is just `CREATE DATABASE ...
    /// TEMPLATE ...`; nothing runs any migrations against the clone
    /// afterward. If you need a migration that runs fresh on every test
    /// database rather than being baked into the shared template, that's
    /// not currently a feature this method provides, the predicate here
    /// only controls what's excluded from the template, full stop.
    ///
    /// The predicate does feed the fingerprint: for a *fixed* predicate,
    /// the resulting (filtered) migration set is deterministic, so the
    /// template name stays stable run to run. Changing which migrations a
    /// predicate excludes changes the resulting set, and therefore changes
    /// the fingerprint and template name, same as any other change to the
    /// effective migration set would.
    pub fn exclude_migration<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&Migration) -> bool + Send + Sync + 'static,
    {
        self.exclude = Some(Arc::new(predicate));
        self
    }

    /// Drop every user defined trigger in the given schema immediately after
    /// cloning each test database (not the template, the template keeps its
    /// triggers). Call multiple times to purge more than one schema.
    /// This extremly useful when triggers do things that are out of scope for your test case
    pub fn purge_triggers_in(mut self, schema: impl Into<String>) -> Self {
        self.purge_schemas.push(schema.into());
        self
    }

    /// Override the template database name prefix. Defaults to
    /// `warmpool_tmpl_`. The fingerprint hash is always appended.
    pub fn template_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.template_prefix = prefix.into();
        self
    }

    /// Mix additional bytes into the fingerprint, useful if two projects
    /// share a migrations directory but should never share a template.
    pub fn fingerprint_salt(mut self, salt: impl Into<String>) -> Self {
        self.fingerprint_salt = Some(salt.into());
        self
    }

    /// Controls the `CREATE DATABASE ... STRATEGY` used when cloning the
    /// template for each test database. Defaults to
    /// [`CloneStrategy::WalLog`] as of 0.1.1. see the README's "Clone
    /// strategy" section for the reasoning and the benchmark numbers
    /// behind this choice.
    pub fn clone_strategy(mut self, strategy: CloneStrategy) -> Self {
        self.clone_strategy = strategy;
        self
    }

    pub async fn build(self) -> Result<TemplatePool> {
        let migrations_path = self
            .migrations_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("./migrations"));

        let migrator = Migrator::new(migrations_path.as_path())
            .await
            .map_err(|source| Error::MigratorLoad {
                path: migrations_path.display().to_string(),
                source,
            })?;

        let migrations: Vec<Migration> = migrator
            .iter()
            .filter(|m| match &self.exclude {
                Some(f) => !f(m),
                None => true,
            })
            .cloned()
            .collect();

        Ok(TemplatePool {
            connect_options: self.connect_options,
            migrations: Arc::new(migrations),
            purge_schemas: Arc::new(self.purge_schemas),
            template_prefix: self.template_prefix,
            fingerprint_salt: self.fingerprint_salt,
            clone_strategy: self.clone_strategy,
            template_info: Arc::new(OnceCell::new()),
        })
    }
}

/// A configured, ready to use handle for cloning test databases. Cheap to
/// clone (everything expensive is behind an `Arc`). The first call to
/// [`TemplatePool::create_test_database`] on *any* clone pays the price and builds the template
/// if it doesn't already exist in Postgres; every other call, in this
/// process or a completely separate `cargo test` run, simply clones it.
#[derive(Clone)]
pub struct TemplatePool {
    connect_options: PgConnectOptions,
    migrations: Arc<Vec<Migration>>,
    purge_schemas: Arc<Vec<String>>,
    template_prefix: String,
    fingerprint_salt: Option<String>,
    clone_strategy: CloneStrategy,
    template_info: Arc<OnceCell<TemplateInfo>>,
}

impl TemplatePool {
    pub fn builder(connect_options: PgConnectOptions) -> TemplatePoolBuilder {
        TemplatePoolBuilder::new(connect_options)
    }

    /// Clone a fresh, already migrated database from the template and
    /// return a connected pool plus a handle for explicit cleanup. Building
    /// the template (if it doesn't already exist) happens transparently on
    /// whichever call gets there first, guarded by a Postgres advisory lock.
    pub async fn create_test_database(&self) -> Result<TestDatabase> {
        let template_info = self.ensure_template().await?;

        let mut maintenance = PgConnection::connect_with(&self.connect_options)
            .await
            .map_err(Error::MaintenanceConnect)?;

        // Clear stray connections to the template before attempting the
        // clone. Postgres refuses `CREATE DATABASE ... TEMPLATE` outright
        // if *anyone* is connected to the source database,. By the
        // time ensure_template() has returned here, the advisory lock for
        // this template's fingerprint has necessarily been released,
        // which means no legitimate build can still be holding a
        // connection open (build_template_if_missing always closes its
        // own connection before the lock is released). Anything found
        // here is a genuine stray, not a build in progress. See
        // `Error::TemplateConnectionSweep`.
        terminate_other_backends(&mut maintenance, &template_info.name)
            .await
            .map_err(|source| Error::TemplateConnectionSweep {
                name: template_info.name.clone(),
                source,
            })?;

        let db_name = format!("warmpool_test_{}", Uuid::new_v4().simple());

        // No-op on servers older than Postgres 15, which don't understand
        // STRATEGY at all, see CloneStrategy::sql_clause.
        let strategy_clause = self
            .clone_strategy
            .sql_clause(template_info.server_version_num)
            .unwrap_or("");

        maintenance
            .execute(
                create_test_database_sql(&db_name, &template_info.name, strategy_clause).as_str(),
            )
            .await
            .map_err(|source| Error::CreateTestDb {
                name: db_name.clone(),
                template: template_info.name.clone(),
                source,
            })?;

        let connect_opts = self.connect_options.clone().database(&db_name);

        if !self.purge_schemas.is_empty() {
            let mut conn = PgConnection::connect_with(&connect_opts)
                .await
                .map_err(|source| Error::TestDbConnect {
                    name: db_name.clone(),
                    source,
                })?;
            for schema in self.purge_schemas.iter() {
                purge_triggers(&mut conn, schema).await?;
            }
        }

        let pool =
            PgPool::connect_with(connect_opts)
                .await
                .map_err(|source| Error::TestDbConnect {
                    name: db_name.clone(),
                    source,
                })?;

        Ok(TestDatabase {
            pool,
            name: db_name,
            maintenance_options: self.connect_options.clone(),
        })
    }

    async fn ensure_template(&self) -> Result<TemplateInfo> {
        let connect_options = self.connect_options.clone();
        let migrations = self.migrations.clone();
        let prefix = self.template_prefix.clone();
        let salt = self.fingerprint_salt.clone();

        self.template_info
            .get_or_try_init(|| async move {
                build_or_reuse_template(&connect_options, &migrations, &prefix, salt.as_deref())
                    .await
            })
            .await
            .cloned()
    }
}

async fn build_or_reuse_template(
    connect_options: &PgConnectOptions,
    migrations: &[Migration],
    prefix: &str,
    salt: Option<&str>,
) -> Result<TemplateInfo> {
    let fp = fingerprint(migrations, salt);
    let template_name = format!("{prefix}{fp}");
    let lock_key = lock_key_from_fingerprint(&fp);

    let mut maintenance = PgConnection::connect_with(connect_options)
        .await
        .map_err(Error::MaintenanceConnect)?;

    // Read once per process (this function only ever runs once per
    // TemplatePool, guarded by the OnceCell in ensure_template) rather than
    // on every create_test_database() call.
    let server_version_num = fetch_server_version_num(&mut maintenance).await?;

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_key)
        .execute(&mut maintenance)
        .await
        .map_err(|source| Error::LockAcquire {
            key: lock_key,
            source,
        })?;

    // Always try to release the lock, even if the build step failed, so a
    // build error on one caller doesn't wedge every other process waiting
    // on the same lock key.
    let build_result = build_template_if_missing(
        &mut maintenance,
        connect_options,
        &template_name,
        migrations,
    )
    .await;

    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_key)
        .execute(&mut maintenance)
        .await
        .map_err(|source| Error::LockRelease {
            key: lock_key,
            source,
        });

    build_result?;
    unlock_result?;

    Ok(TemplateInfo {
        name: template_name,
        server_version_num,
    })
}

/// `server_version_num` is the numeric form Postgres exposes for exactly
/// this kind of feature-gating (e.g. `160003` for 16.3), as opposed to the
/// human-readable `server_version` setting.
async fn fetch_server_version_num(maintenance: &mut PgConnection) -> Result<i32> {
    let raw: String = sqlx::query_scalar("SELECT current_setting('server_version_num')")
        .fetch_one(&mut *maintenance)
        .await
        .map_err(Error::ServerVersionCheck)?;

    raw.parse::<i32>()
        .map_err(|_| Error::ServerVersionParse { raw })
}

async fn build_template_if_missing(
    maintenance: &mut PgConnection,
    connect_options: &PgConnectOptions,
    template_name: &str,
    migrations: &[Migration],
) -> Result<()> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(template_name)
            .fetch_one(&mut *maintenance)
            .await
            .map_err(|source| Error::TemplateExistsCheck {
                name: template_name.to_string(),
                source,
            })?;

    if exists {
        return Ok(());
    }

    // Build under a `_building` suffix and only rename into the final
    // name after every migration succeeds, so a crash mid-build (a killed
    // CI job, an OOM, a migration panicking the process) can never leave
    // a half migrated database sitting under `template_name`, where the
    // existence check above would mistake it for a finished template.
    let building_name = format!("{template_name}_building");

    // Clean up a `_building` database left over from a previous crashed
    // attempt at this exact fingerprint, if there is one. We're inside
    // the advisory lock for this fingerprint right now, so anything found
    // under this name is necessarily an orphan from a *past* attempt, not
    // a build in progress elsewhere, same reasoning as the
    // template connection sweep in create_test_database(). Skipping this is very bad,
    // because it would mean a crashed build permanently blocks every future attempt
    // at this fingerprint: warmpool's own CREATE DATABASE below would keep
    // failing with "already exists" against the orphan, forever.
    sweep_and_drop_database(maintenance, &building_name)
        .await
        .map_err(|source| Error::CleanupStaleBuildingDb {
            name: building_name.clone(),
            source,
        })?;

    maintenance
        .execute(create_template_database_sql(&building_name).as_str())
        .await
        .map_err(|source| Error::CreateTemplateDb {
            name: building_name.clone(),
            source,
        })?;

    let building_opts = connect_options.clone().database(&building_name);

    let template_pool = PgPool::connect_with(building_opts)
        .await
        .map_err(|source| Error::TemplatePoolConnect {
            name: building_name.clone(),
            source,
        })?;

    // Errors from here report `building_name`, not `template_name`: at
    // this point `building_name` is the only one of the two that actually
    // exists, and it's what a dev would need to connect to in order
    // to inspect exactly how far a failed build got.
    run_migrations(&template_pool, &building_name, migrations).await?;

    // Postgres refuses `CREATE DATABASE ... TEMPLATE x` and `ALTER
    // DATABASE x RENAME` alike while any connection remains open against x.
    // Every clone in create_test_database() already depends on the first half of that;
    // the rename immediately below depends on it too.
    template_pool.close().await;

    // A stray connection could have attached to `_building` while
    // migrations were running (the equivalent scenario is exercised for
    // the clone path by `test_external_connection_during_a_build_...` in
    // the integration suite) and lingered past our own close() above.
    // Sweep again immediately before the rename, for the same reason
    // create_test_database() sweeps immediately before its own
    // connection sensitive statement.
    terminate_other_backends(maintenance, &building_name)
        .await
        .map_err(|source| Error::TemplateConnectionSweep {
            name: building_name.clone(),
            source,
        })?;

    // The atomic handoff. From Postgres's catalog perspective this rename
    // is a single operation: either it succeeds and `template_name` is
    // now the fully migrated database, or it fails and `template_name`
    // still doesn't exist at all, there is no window where a
    // half migrated database exists under the final name. If it fails,
    // `building_name` (now fully migrated but not yet promoted) is left
    // for the next build attempt's `CleanupStaleBuildingDb` sweep to drop
    // and rebuild from scratch, same as any other orphan.
    maintenance
        .execute(rename_database_sql(&building_name, template_name).as_str())
        .await
        .map_err(|source| Error::TemplateRename {
            from: building_name,
            to: template_name.to_string(),
            source,
        })?;

    Ok(())
}

async fn run_migrations(
    pool: &PgPool,
    template_name: &str,
    migrations: &[Migration],
) -> Result<()> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        );
        "#,
    )
    .await
    .map_err(|source| Error::MigrationFailed {
        version: 0,
        description: "_sqlx_migrations bootstrap".to_string(),
        template: template_name.to_string(),
        source,
    })?;

    for migration in migrations {
        pool.execute(migration.sql.as_ref())
            .await
            .map_err(|source| Error::MigrationFailed {
                version: migration.version,
                description: migration.description.to_string(),
                template: template_name.to_string(),
                source,
            })?;

        sqlx::query(
            r#"
            INSERT INTO _sqlx_migrations
                (version, description, success, checksum, execution_time)
            VALUES ($1, $2, true, $3, 0)
            ON CONFLICT (version) DO NOTHING
            "#,
        )
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .execute(pool)
        .await
        .map_err(|source| Error::MigrationFailed {
            version: migration.version,
            description: migration.description.to_string(),
            template: template_name.to_string(),
            source,
        })?;
    }

    Ok(())
}

async fn purge_triggers(conn: &mut PgConnection, schema: &str) -> Result<()> {
    let statement = purge_triggers_sql(schema);

    conn.execute(statement.as_str())
        .await
        .map_err(|source| Error::PurgeTriggers {
            schema: schema.to_string(),
            source,
        })?;

    Ok(())
}

// SQL / connection management helpers
//
// The three `*_sql` functions are extracted so the exact statement text is
// unit testable without a live Postgres connection. None of these do any
// escaping beyond wrapping identifiers in double quotes / literals in
// single quotes.
//
// `terminate_other_backends` is different in kind: it's an async
// operation, not a pure string builder (its one dynamic value is a bound
// parameter, `$1`, not string interpolated, so no escaping gap here to
// document or test). Shared between the template sweep in
// `create_test_database()` and the test database sweep in
// `TestDatabase::drop_database()`: same query, two different reasons to
// run it, two different `Error` variants at the two call sites.

/// Escape `value` for safe interpolation into a double quoted Postgres
/// identifier. The rule is simpler than the string literal case
/// ([`escape_sql_literal`]): inside `"..."`, a literal `"` is written by
/// doubling it, and backslash has no special meaning at all, so there is
/// no `E'...'`-style escape mode question to worry about here.
///
/// This matters more than the string literal gap did. `purge_triggers_sql`'s
/// output goes into a `DO $$ ... $$` body, which Postgres parses as a
/// single unit, so a break out there could only widen a `WHERE` clause.
/// These identifiers go into statements sent via simple query protocol,
/// where a break out *can* terminate the statement and start a new one,
/// so a break out here could be used to drop arbitrary databases. This is
/// confirmed by actually dropping an unrelated database with a crafted
/// `template_prefix`. See `create_test_database_sql_neutralizes_an_identifier_break_out`.
fn escape_sql_identifier(value: &str) -> String {
    value.replace('"', "\"\"")
}

fn create_test_database_sql(db_name: &str, template_name: &str, strategy_clause: &str) -> String {
    let db_name = escape_sql_identifier(db_name);
    let template_name = escape_sql_identifier(template_name);
    format!(r#"CREATE DATABASE "{db_name}" WITH TEMPLATE "{template_name}"{strategy_clause};"#)
}

fn create_template_database_sql(template_name: &str) -> String {
    let template_name = escape_sql_identifier(template_name);
    format!(r#"CREATE DATABASE "{template_name}";"#)
}

/// Used both to promote a fully migrated `_building` database into its
/// final name, and (were it ever needed elsewhere) any other database
/// rename. See `Error::TemplateRename` for what atomicity guarantee this
/// buys `build_template_if_missing()`.
fn rename_database_sql(from: &str, to: &str) -> String {
    let from = escape_sql_identifier(from);
    let to = escape_sql_identifier(to);
    format!(r#"ALTER DATABASE "{from}" RENAME TO "{to}";"#)
}

/// Escape `value` for safe interpolation into a Postgres string literal,
/// and wrap it in Postgres's "escape string" syntax (`E'...'`) rather
/// than a plain `'...'` literal. This is deliberate, because
/// doubling embedded `'` alone is only sufficient when the connected
/// server has `standard_conforming_strings = on` (the default since
/// Postgres 9.1, but not something this pure function can check cause it
/// never touches a connection). Using `E'...'` makes backslash escaping
/// semantics explicit and independent of that setting, so this will be correct
/// regardless of how the server is configured. Doubles both `'` (the
/// literal's delimiter) and `\` (the escape character `E'...'` syntax
/// gives meaning to), doubling only the former would let a schema name
/// ending in a backslash re-open the literal via `\'` being read as an
/// escaped quote rather than a closing one. Bad things happen if a schema name can break
/// out of the literal and inject arbitrary SQL into the `WHERE nspname = ...` clause of the `purge_triggers_sql()` DO block.
fn escape_sql_literal(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "''");
    format!("E'{escaped}'")
}

fn drop_database_if_exists_sql(name: &str) -> String {
    let name = escape_sql_identifier(name);
    format!(r#"DROP DATABASE IF EXISTS "{name}";"#)
}

/// `schema` is interpolated into a single quoted SQL string literal
/// (`WHERE nspname = ...`) rather than passed as a bound parameter,
/// this DO block can't take one, since `EXECUTE format(...)` only
/// parameterizes the identifiers it formats, not the literal driving the
/// `WHERE` clause itself, and anonymous code blocks don't accept `$1`
/// placeholders at all. `schema` is escaped via [`escape_sql_literal`]
/// before interpolation, so a schema name containing a `'` or `\` can no
/// longer break out of the literal. In practice `schema` is a
/// compile-time-ish config value from [`TemplatePoolBuilder::purge_triggers_in`],
/// not end-user input, which is the only reason this we consider thid low practical risk gap
/// not an exploitable one. See `purge_triggers_sql_neutralizes_a_quote_and_semicolon_injection_attempt`
/// test for what it looks like closed.
fn purge_triggers_sql(schema: &str) -> String {
    let schema_literal = escape_sql_literal(schema);
    format!(
        r#"
        DO $$
        DECLARE
            r RECORD;
        BEGIN
            FOR r IN
                SELECT tgname, relname, nspname AS rel_schema
                FROM pg_trigger
                JOIN pg_class ON pg_class.oid = tgrelid
                JOIN pg_namespace ON pg_namespace.oid = relnamespace
                WHERE nspname = {schema_literal} AND NOT tgisinternal
            LOOP
                EXECUTE format(
                    'DROP TRIGGER IF EXISTS %I ON %I.%I CASCADE',
                    r.tgname,
                    r.rel_schema,
                    r.relname
                );
            END LOOP;
        END
        $$;
        "#
    )
}

/// Terminate every other backend connected to `datname`. See the block
/// comment above this function's siblings for why this isn't a `*_sql`
/// pure string function like the others.
async fn terminate_other_backends(
    conn: &mut PgConnection,
    datname: &str,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        SELECT pg_terminate_backend(pid)
        FROM pg_stat_activity
        WHERE datname = $1 AND pid <> pg_backend_pid()
        "#,
    )
    .bind(datname)
    .execute(conn)
    .await?;
    Ok(())
}

/// Sweep stray connections from `name`, then drop it if it exists.
/// `DROP DATABASE IF EXISTS` against a name that was never created is a
/// no-op, so this is always safe to call whether or not `name` actually
/// exists. Shared between `TestDatabase::drop_database()` and the
/// leftover-`_building` cleanup in `build_template_if_missing()`, same
/// two step operation, two different reasons to run it, two different
/// `Error` variants at the two call sites (mapped by each caller, not
/// here, same pattern as `terminate_other_backends`).
async fn sweep_and_drop_database(
    conn: &mut PgConnection,
    name: &str,
) -> std::result::Result<(), sqlx::Error> {
    terminate_other_backends(conn, name).await?;
    conn.execute(drop_database_if_exists_sql(name).as_str())
        .await?;
    Ok(())
}

/// A cloned, migrated test database. Brcause Rust's `Drop` can't run async code,
/// this deliberately does *not* try to auto drop the Postgres database in a
/// `Drop` impl via a detached `tokio::spawn`.. Call
/// [`TestDatabase::drop_database`] explicitly, or use `#[warm_test]`, which
/// calls it on both success and panic.
pub struct TestDatabase {
    pool: PgPool,
    name: String,
    maintenance_options: PgConnectOptions,
}

impl TestDatabase {
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Close the pool and drop the underlying Postgres database. If you
    /// skip this (maybe while debugging a failing test), the database is
    /// left behind under its `warmpool_test_*` name for inspection, clean
    /// those up periodically with the query in the README.
    pub async fn drop_database(self) -> Result<()> {
        self.pool.close().await;
        let mut maintenance = PgConnection::connect_with(&self.maintenance_options)
            .await
            .map_err(Error::MaintenanceConnect)?;

        sweep_and_drop_database(&mut maintenance, &self.name)
            .await
            .map_err(|source| Error::DropTestDb {
                name: self.name.clone(),
                source,
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_connect_options() -> PgConnectOptions {
        // Never actually connected to in any test in this module build()
        // only touches the filesystem via Migrator::new, not the network.
        "postgres://postgres:postgres@127.0.0.1:1/postgres"
            .parse()
            .unwrap()
    }

    // TemplatePoolBuilder: defaults

    #[test]
    fn builder_defaults() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options());
        assert_eq!(builder.template_prefix, "warmpool_tmpl_");
        assert!(builder.migrations_path.is_none());
        assert!(builder.exclude.is_none());
        assert!(builder.purge_schemas.is_empty());
        assert!(builder.fingerprint_salt.is_none());
        assert_eq!(builder.clone_strategy, CloneStrategy::WalLog);
    }

    // TemplatePoolBuilder: each setter sets its field

    #[test]
    fn builder_migrations_from_sets_path() {
        let builder =
            TemplatePoolBuilder::new(dummy_connect_options()).migrations_from("./somewhere");
        assert_eq!(builder.migrations_path, Some(PathBuf::from("./somewhere")));
    }

    #[test]
    fn builder_template_prefix_overrides_default() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options()).template_prefix("custom_");
        assert_eq!(builder.template_prefix, "custom_");
    }

    #[test]
    fn builder_fingerprint_salt_sets_field() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options()).fingerprint_salt("pepper");
        assert_eq!(builder.fingerprint_salt.as_deref(), Some("pepper"));
    }

    #[test]
    fn builder_purge_triggers_in_accumulates_in_call_order() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options())
            .purge_triggers_in("public")
            .purge_triggers_in("other");
        assert_eq!(
            builder.purge_schemas,
            vec!["public".to_string(), "other".to_string()]
        );
    }

    #[test]
    fn builder_clone_strategy_overrides_default() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options())
            .clone_strategy(CloneStrategy::FileCopy);
        assert_eq!(builder.clone_strategy, CloneStrategy::FileCopy);
    }

    #[test]
    fn builder_exclude_migration_sets_predicate() {
        let builder = TemplatePoolBuilder::new(dummy_connect_options())
            .exclude_migration(|m| m.description.contains("x"));
        assert!(builder.exclude.is_some());
    }

    #[test]
    fn builder_methods_chain_and_compose() {
        // Every setter chains and the end state reflects every call, not
        // just the last one a fluent builder smoke test.
        let builder = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./m")
            .template_prefix("p_")
            .fingerprint_salt("s")
            .purge_triggers_in("a")
            .clone_strategy(CloneStrategy::Auto);

        assert_eq!(builder.migrations_path, Some(PathBuf::from("./m")));
        assert_eq!(builder.template_prefix, "p_");
        assert_eq!(builder.fingerprint_salt.as_deref(), Some("s"));
        assert_eq!(builder.purge_schemas, vec!["a".to_string()]);
        assert_eq!(builder.clone_strategy, CloneStrategy::Auto);
    }

    // exclude_migration + build(): what actually happens to excluded migrations
    //
    // build() never touches the network (Migrator::new only reads files),
    // so these run under plain `cargo test`

    #[tokio::test]
    async fn build_without_exclude_keeps_every_migration() {
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .build()
            .await
            .expect("build should not touch the network");

        let descriptions: Vec<&str> = pool
            .migrations
            .iter()
            .map(|m| m.description.as_ref())
            .collect();
        assert_eq!(
            descriptions,
            vec![
                "initial",
                "create widgets",
                "create gadgets",
                "seed data",
                "tenants",
                "create triggers"
            ]
        );
    }

    #[tokio::test]
    async fn exclude_migration_removes_matching_migrations_from_the_stored_set() {
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .exclude_migration(|m| m.description.contains("seed data"))
            .build()
            .await
            .expect("build should not touch the network");

        let descriptions: Vec<&str> = pool
            .migrations
            .iter()
            .map(|m| m.description.as_ref())
            .collect();
        assert_eq!(
            descriptions,
            vec![
                "initial",
                "create widgets",
                "create gadgets",
                "tenants",
                "create triggers"
            ],
            "the excluded migration must not appear in the stored set"
        );
    }

    #[tokio::test]
    async fn excluded_migration_is_not_retained_anywhere_on_the_built_pool() {
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .exclude_migration(|m| m.description.contains("seed data"))
            .build()
            .await
            .expect("build should not touch the network");

        assert_eq!(
            pool.migrations.len(),
            5,
            "only the non excluded migrations are kept"
        );
        assert!(
            pool.migrations.iter().all(|m| m.description != "seed data"),
            "the excluded migration is gone, not stashed somewhere for later"
        );
    }

    #[tokio::test]
    async fn exclude_predicate_matching_nothing_keeps_full_set() {
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .exclude_migration(|m| m.description.contains("does_not_match_anything"))
            .build()
            .await
            .expect("build should not touch the network");

        assert_eq!(pool.migrations.len(), 6);
    }

    #[tokio::test]
    async fn exclude_predicate_matching_everything_yields_empty_template() {
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .exclude_migration(|_| true)
            .build()
            .await
            .expect("build should not touch the network");

        assert!(pool.migrations.is_empty());
    }

    // SQL builder functions

    #[test]
    fn create_test_database_sql_without_strategy_clause() {
        let sql = create_test_database_sql("warmpool_test_abc", "warmpool_tmpl_xyz", "");
        assert_eq!(
            sql,
            r#"CREATE DATABASE "warmpool_test_abc" WITH TEMPLATE "warmpool_tmpl_xyz";"#
        );
    }

    #[test]
    fn create_test_database_sql_with_strategy_clause() {
        let sql = create_test_database_sql(
            "warmpool_test_abc",
            "warmpool_tmpl_xyz",
            " STRATEGY = WAL_LOG",
        );
        assert_eq!(
            sql,
            r#"CREATE DATABASE "warmpool_test_abc" WITH TEMPLATE "warmpool_tmpl_xyz" STRATEGY = WAL_LOG;"#
        );
    }

    #[test]
    fn create_template_database_sql_is_a_plain_create_database() {
        assert_eq!(
            create_template_database_sql("warmpool_tmpl_xyz"),
            r#"CREATE DATABASE "warmpool_tmpl_xyz";"#
        );
    }

    #[test]
    fn rename_database_sql_targets_the_right_names() {
        assert_eq!(
            rename_database_sql("warmpool_tmpl_xyz_building", "warmpool_tmpl_xyz"),
            r#"ALTER DATABASE "warmpool_tmpl_xyz_building" RENAME TO "warmpool_tmpl_xyz";"#
        );
    }

    #[test]
    fn drop_database_if_exists_sql_is_idempotent_by_construction() {
        assert_eq!(
            drop_database_if_exists_sql("warmpool_tmpl_xyz_building"),
            r#"DROP DATABASE IF EXISTS "warmpool_tmpl_xyz_building";"#
        );
    }

    #[test]
    fn purge_triggers_sql_targets_the_given_schema() {
        let sql = purge_triggers_sql("public");
        assert!(sql.contains("nspname = E'public'"));
        assert!(sql.contains("DROP TRIGGER IF EXISTS"));
    }

    #[test]
    fn escape_sql_literal_doubles_embedded_single_quotes() {
        assert_eq!(escape_sql_literal("O'Brien"), "E'O''Brien'");
    }

    #[test]
    fn escape_sql_literal_doubles_embedded_backslashes() {
        assert_eq!(escape_sql_literal(r"a\b"), r"E'a\\b'");
    }

    #[test]
    fn escape_sql_literal_handles_a_trailing_backslash_safely() {
        // The specific edge case that makes escaping only quotes
        // insufficient once E'...' syntax is in play: a trailing
        // backslash right before what should be the closing quote. If
        // the backslash weren't also doubled, `\'` would be read as an
        // *escaped* quote (string still open), swallowing everything
        // after it, including the rest of the generated SQL into
        // the literal.
        let escaped = escape_sql_literal(r"public\");
        assert_eq!(
            escaped, r"E'public\\'",
            "the trailing backslash must be doubled, if it weren't, \\' \
             at the end would be read as an escaped quote (string still \
             open) instead of a backslash followed by the closing quote"
        );
    }

    #[test]
    fn purge_triggers_sql_neutralizes_a_boolean_where_clause_injection() {
        // The *exploitable* payload shape here.
        //
        // Worth being precise about the mechanism, because the obvious
        // guess and assumption is wrong: a `'; DROP TABLE x; --` style payload does NOT
        // work against this function. The interpolation point sits inside
        // a dollar quoted `DO $$ ... $$` body, which Postgres parses as a
        // single unit, so a stray `;` can't start a new top-level
        // statement it just produces a syntax error and the whole
        // block fails loudly.
        //
        // What *did* work was widening the WHERE clause instead of
        // escaping the statement: `tenant_a' OR '1'='1` keeps the SQL
        // syntactically valid while making the FOR loop iterate over
        // every non-internal trigger in the database, not just the
        // requested schema's. Combined with `format('%I')` emitting an
        // unqualified relation name (so the DROP resolves against
        // `search_path`), this deleted a trigger in a schema the caller
        // never named. That's the real severity: not arbitrary statement
        // execution, but silently purging triggers outside the requested
        // schema.
        let payload = "tenant_a' OR '1'='1";
        let sql = purge_triggers_sql(payload);

        assert!(
            sql.contains(r#"nspname = E'tenant_a'' OR ''1''=''1'"#),
            "the payload must be contained in one escaped literal so it can \
             only ever match a (nonexistent) schema with that literal name, \
             never widen the WHERE clause: got {sql}"
        );
        assert!(
            !sql.contains("nspname = E'tenant_a' OR '1'='1'"),
            "the broken-out form, where OR becomes live SQL, must not appear"
        );
    }

    #[test]
    fn purge_triggers_sql_neutralizes_a_backslash_based_injection_attempt() {
        // A payload shaped to exploit E'...' syntax's
        // backslash escaping if only quotes were doubled: a trailing
        // backslash intended to escape the literal's closing quote so the
        // string stays open and the rest of the payload becomes live SQL.
        let payload = r"tenant_a\' OR '1'='1";
        let sql = purge_triggers_sql(payload);

        let expected_literal_body = payload.replace('\\', "\\\\").replace('\'', "''");
        assert!(
            sql.contains(&format!("nspname = E'{expected_literal_body}'")),
            "payload must be fully contained in one escaped literal: got {sql}"
        );
    }

    #[test]
    fn escape_sql_identifier_doubles_embedded_double_quotes() {
        assert_eq!(escape_sql_identifier(r#"a"b"#), r#"a""b"#);
    }

    #[test]
    fn escape_sql_identifier_leaves_backslashes_alone() {
        // Unlike the string literal case, backslash has no special
        // meaning inside a double-quoted identifier doubling it here
        // would corrupt the name rather than protect it.
        assert_eq!(escape_sql_identifier(r"a\b"), r"a\b");
    }

    #[test]
    fn create_test_database_sql_neutralizes_an_identifier_break_out() {
        // The payload that, before this fix, actually executed as a
        // separate statement and dropped an unrelated database.
        //
        // This is the key difference from SHARP_EDGES.md #3: that one's
        // output lives inside a `DO $$ ... $$` body, which Postgres parses
        // as a single unit, so a break-out could only ever widen a WHERE
        // clause. These statements go out over simple query protocol,
        // where a break-out really can start a new statement -- and did.
        let sql = create_test_database_sql(
            "warmpool_test_abc",
            r#"warmpool_tmpl_"; DROP DATABASE wp_victim; --"#,
            "",
        );

        assert!(
            sql.contains(r#"WITH TEMPLATE "warmpool_tmpl_""; DROP DATABASE wp_victim; --""#),
            "the payload must stay inside one escaped identifier: got {sql}"
        );
        // Counting `;` would be a bad check here, the payload's own
        // semicolons are still present in the output, just inert inside
        // the identifier. What actually matters is that every `"` is
        // balanced, so the identifier the payload tried to close is still
        // open at that point and the `;` never terminates anything. An
        // odd count would mean exactly the break out this guards against.
        assert_eq!(
            sql.matches('"').count() % 2,
            0,
            "double quotes must stay balanced, an odd count means an \
             identifier was closed early: got {sql}"
        );
    }

    #[test]
    fn every_identifier_quoting_helper_escapes_its_inputs() {
        // All four helpers share the same exposure, since all four derive
        // their names from template_prefix. A fix covering only
        // create_test_database_sql would leave the other three exactly as
        // open as they were.
        let payload = r#"x"; DROP DATABASE wp_victim; --"#;

        for sql in [
            create_test_database_sql("safe_db", payload, ""),
            create_template_database_sql(payload),
            rename_database_sql(payload, "safe_target"),
            rename_database_sql("safe_source", payload),
            drop_database_if_exists_sql(payload),
        ] {
            assert_eq!(
                sql.matches('"').count() % 2,
                0,
                "double quotes must stay balanced in every helper's output: got {sql}"
            );
            assert!(
                sql.contains(r#"x""; DROP DATABASE wp_victim; --"#),
                "payload must appear escaped, not broken out: got {sql}"
            );
        }
    }
}
