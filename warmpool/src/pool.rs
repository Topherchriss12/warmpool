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

        let db_name = format!("warmpool_test_{}", Uuid::new_v4().simple());

        // No-op on servers older than Postgres 15, which don't understand
        // STRATEGY at all — see CloneStrategy::sql_clause.
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

    maintenance
        .execute(create_template_database_sql(template_name).as_str())
        .await
        .map_err(|source| Error::CreateTemplateDb {
            name: template_name.to_string(),
            source,
        })?;

    let template_opts = connect_options.clone().database(template_name);

    let template_pool = PgPool::connect_with(template_opts)
        .await
        .map_err(|source| Error::TemplatePoolConnect {
            name: template_name.to_string(),
            source,
        })?;

    run_migrations(&template_pool, template_name, migrations).await?;

    // Postgres refuses `CREATE DATABASE ...
    // TEMPLATE x` while any connection remains open against x. Every clone
    // in create_test_database() depends on this having already happened.
    template_pool.close().await;

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

//- Pure SQL-building helpers-
//
// Extracted so the exact statement text is unit-testable without a live
// Postgres connection. None of these do any escaping beyond wrapping
// identifiers in double quotes / literals in single quotes — see the test
// module below for what that does and doesn't protect against.

fn create_test_database_sql(db_name: &str, template_name: &str, strategy_clause: &str) -> String {
    format!(r#"CREATE DATABASE "{db_name}" WITH TEMPLATE "{template_name}"{strategy_clause};"#)
}

fn create_template_database_sql(template_name: &str) -> String {
    format!(r#"CREATE DATABASE "{template_name}";"#)
}

/// `schema` is interpolated directly into a single-quoted SQL string
/// literal (`WHERE nspname = '{schema}'`) rather than passed as a bound
/// parameter — this DO block can't take one, since `EXECUTE format(...)`
/// only parameterizes the identifiers it formats, not the literal driving
/// the `WHERE` clause itself. In practice `schema` is a compile-time-ish
/// config value from [`TemplatePoolBuilder::purge_triggers_in`], not
/// end-user input, so this hasn't been a practical problem — but it *is*
/// unescaped, and a schema name containing a `'` breaks out of the
/// literal. See `purge_triggers_sql_does_not_escape_embedded_quotes`
/// below, which documents this rather than pretending it isn't there.
fn purge_triggers_sql(schema: &str) -> String {
    format!(
        r#"
        DO $$
        DECLARE
            r RECORD;
        BEGIN
            FOR r IN
                SELECT tgname, relname
                FROM pg_trigger
                JOIN pg_class ON pg_class.oid = tgrelid
                JOIN pg_namespace ON pg_namespace.oid = relnamespace
                WHERE nspname = '{schema}' AND NOT tgisinternal
            LOOP
                EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I CASCADE', r.tgname, r.relname);
            END LOOP;
        END
        $$;
        "#
    )
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

        // Terminating lingering backends first and then issuing a plain
        // `DROP DATABASE` works on every supported version.
        sqlx::query(
            r#"
            SELECT pg_terminate_backend(pid)
            FROM pg_stat_activity
            WHERE datname = $1 AND pid <> pg_backend_pid()
            "#,
        )
        .bind(&self.name)
        .execute(&mut maintenance)
        .await
        .map_err(|source| Error::DropTestDb {
            name: self.name.clone(),
            source,
        })?;

        maintenance
            .execute(format!(r#"DROP DATABASE IF EXISTS "{}";"#, self.name).as_str())
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
            vec!["initial", "create widgets", "create gadgets", "seed data"]
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
            vec!["initial", "create widgets", "create gadgets"],
            "the excluded migration must not appear in the stored set"
        );
    }

    #[tokio::test]
    async fn excluded_migration_is_not_retained_anywhere_on_the_built_pool() {
        // TemplatePool has exactly one
        // field that ever holds Migration values (`migrations`), and it
        // only ever holds the already filtered set there is nowhere
        // else in the struct an excluded migration's SQL could be hiding,
        // ready to run against a freshly cloned test database later.
        // create_test_database() confirms this: it never reads
        // `self.migrations` at all, only `self.template_prefix` (via the
        // cached template name) and `self.clone_strategy`.
        let pool = TemplatePoolBuilder::new(dummy_connect_options())
            .migrations_from("./tests/fixtures/migrations")
            .exclude_migration(|m| m.description.contains("seed data"))
            .build()
            .await
            .expect("build should not touch the network");

        assert_eq!(
            pool.migrations.len(),
            3,
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

        assert_eq!(pool.migrations.len(), 4);
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
    fn purge_triggers_sql_targets_the_given_schema() {
        let sql = purge_triggers_sql("public");
        assert!(sql.contains("nspname = 'public'"));
        assert!(sql.contains("DROP TRIGGER IF EXISTS"));
    }

    #[test]
    fn purge_triggers_sql_does_not_escape_embedded_quotes() {
        // Documents current limitation: a schema name containing a single quote breaks out of the string
        // literal. `schema` comes from TemplatePoolBuilder::purge_triggers_in,
        // a config value rather than typical end-user input, which is why
        // this hasn't been and doesn't seem to be a practical problem but the function does not
        // defend against it, and this test exists so that changes to
        // purge_triggers_sql don't silently start "fixing" this without it
        // being a deliberate decision that has undergone review.
        let sql = purge_triggers_sql("public'; DROP TABLE users;");
        assert!(
            sql.contains("nspname = 'public'; DROP TABLE users;'"),
            "current behavior: the quote is not escaped and breaks out of the literal \
             (this assertion is intentionally documenting the gap, not endorsing it)"
        );
    }

    #[test]
    fn create_test_database_sql_does_not_escape_embedded_quotes_in_names_either() {
        // Same caveat, for the identifier side: db_name and
        // template_name are wrapped in double quotes but not escaped. Both
        // values are warmpool generated in practice (a UUID and a
        // prefix+fingerprint), except template_prefix is user configurable
        // via TemplatePoolBuilder::template_prefix, which means a prefix
        // containing `"` would break out of the quoted identifier.
        let sql = create_test_database_sql(
            "warmpool_test_abc",
            r#"warmpool_tmpl_"; DROP TABLE users;"#,
            "",
        );
        assert!(
            sql.contains(r#""warmpool_tmpl_"; DROP TABLE users;""#),
            "current behavior: the embedded quote is not escaped"
        );
    }
}
