/// Everything that can go wrong while building a template database or
/// cloning a test database from it. Each variant carries enough context
/// (names, versions, descriptions) to act on without re deriving it from a
/// generic `sqlx::Error`.
/// The `source` field of each variant is the underlying `sqlx::Error` that caused
/// the failure, which can be downcast to `sqlx::error::DatabaseError`
/// when you need to inspect the Postgres error code or message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("failed to connect to postgres for maintenance operations")]
    MaintenanceConnect(#[source] sqlx::Error),

    #[error("failed to load migrations from `{path}`")]
    MigratorLoad {
        path: String,
        #[source]
        source: sqlx::migrate::MigrateError,
    },

    #[error("failed to acquire template build advisory lock (key {key})")]
    LockAcquire {
        key: i64,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to release template build advisory lock (key {key})")]
    LockRelease {
        key: i64,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to check whether template database `{name}` already exists")]
    TemplateExistsCheck {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to create template database `{name}`")]
    CreateTemplateDb {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to connect to template database `{name}` while building it")]
    TemplatePoolConnect {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("migration {version} (\"{description}\") failed while building template `{template}`")]
    MigrationFailed {
        version: i64,
        description: String,
        template: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to purge triggers in schema `{schema}`")]
    PurgeTriggers {
        schema: String,
        #[source]
        source: sqlx::Error,
    },

    /// Fired from `create_test_database()`, immediately before the
    /// `CREATE DATABASE ... TEMPLATE ...` statement, while clearing stray
    /// connections from the template. If this fails, the clone attempt is aborted before it starts.
    ///
    /// However, this sweep does closes the window on a *pre-existing* stray connection,
    /// a crashed process, a leftover `psql` session, anything that was already connected before this
    /// call started, it cannot prevent a brand new connection from racing
    /// in during the (very small) window between this sweep completing and
    /// the `CREATE DATABASE` statement that follows it.
    #[error("failed to clear stray connections from `{name}`")]
    TemplateConnectionSweep {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    /// Fired at the start of `build_template_if_missing()`'s slow path,
    /// while clearing away a `_building` database left over from a
    /// previous crashed build attempt at this exact fingerprint.
    /// Ak illed CI job, an OOM, a migration panicking the process before it
    /// had a chance to fail cleanly. This cleanup runs unconditionally at
    /// the start of every fresh build, not just when an orphan is
    /// suspected: `DROP DATABASE IF EXISTS` against a name that was never
    /// created is a no-op, so there's no meaningful cost to always trying,
    /// and skipping it would mean a crashed build permanently blocks every
    /// future attempt at this fingerprint (`CREATE DATABASE` would keep
    /// failing with "already exists" against the orphan forever).
    #[error("failed to clean up a leftover `{name}` from a previous build attempt")]
    CleanupStaleBuildingDb {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    /// Fired from `build_template_if_missing()`'s final step: renaming the
    /// fully migrated `_building` database into its final, fingerprinted
    /// name. This rename is what makes template construction crash atomic
    /// as of 0.1.3. there is no window, from Postgres's catalog perspective,
    /// where a half-migrated database exists under the final name. If this fails, `_building` remains
    /// under its `_building` name, fully migrated but not yet promoted;
    /// the next build attempt for this fingerprint finds it via
    /// `CleanupStaleBuildingDb`'s sweep, drops it, and rebuilds from
    /// scratch and not try to resume or reuse it.
    #[error("failed to rename `{from}` to `{to}`")]
    TemplateRename {
        from: String,
        to: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to create test database `{name}` from template `{template}`")]
    CreateTestDb {
        name: String,
        template: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to connect to test database `{name}`")]
    TestDbConnect {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("failed to drop test database `{name}`")]
    DropTestDb {
        name: String,
        #[source]
        source: sqlx::Error,
    },

    /// Fired from `TemplatePoolBuilder::build()` when the template name
    /// this configuration would produce is too long for Postgres.
    ///
    /// Postgres truncates identifiers to 63 bytes and only emits a
    /// `NOTICE`, never an error, so without this check the failure is
    /// silent and deeply confusing. The name that actually has to fit is
    /// not the template name itself but the `_building` name derived from
    /// it by the crash-atomic build path.
    /// If `<template>_building` truncates back down to `<template>`, the two
    /// become byte identical and the build's final rename turns into a
    /// rename to self that fails with `database "..." already exists`.
    ///
    /// `limit` is the budget for `prefix` specifically: the 63-byte
    /// identifier limit minus the fingerprint length and the `_building`
    /// suffix.
    #[error(
        "template prefix `{prefix}` is {actual} bytes, which leaves no room for the \
         fingerprint and the `_building` suffix within Postgres's 63-byte identifier \
         limit (maximum usable prefix here: {limit} bytes)"
    )]
    TemplatePrefixTooLong {
        prefix: String,
        actual: usize,
        limit: usize,
    },

    /// Fired while listing or dropping stale templates, either from
    /// [`TemplatePool::stale_template_names`] /
    /// [`TemplatePool::prune_stale_templates`] called directly, or from
    /// the optional post build hook enabled by
    /// `TemplatePoolBuilder::prune_stale_templates_on_build`.
    ///
    /// Pruning is deliberately **opt-in and never automatic by default**.
    /// Templates are scoped only by their name prefix, and the default
    /// prefix is shared by every warmpool user on a given Postgres
    /// instance including sibling migration sets in the *same* process
    /// (which `TemplatePoolBuilder` explicitly supports) and unrelated
    /// projects on a shared CI instance. A template that is "stale" from
    /// one pool's point of view may be the live, in-use template of
    /// another.
    #[error("failed to prune stale templates with prefix `{prefix}`")]
    PruneStaleTemplates {
        prefix: String,
        #[source]
        source: sqlx::Error,
    },

    /// Fired from `build_or_reuse_template()`, on the same
    /// maintenance connection already opened to check for / build the
    /// template, before the advisory lock is taken. Cached for the
    /// lifetime of the `TemplatePool` afterward, so this only fires (if it
    /// fires at all) on the first `create_test_database()` call.
    #[error("failed to read the connected server's `server_version_num`")]
    ServerVersionCheck(#[source] sqlx::Error),

    /// Postgres returned `server_version_num` as a plain
    /// integer string in every version that's ever shipped it, so seeing
    /// this in practice would mean something unusual is answering on the
    /// other end of the connection, not a normal server version edge case.
    /// further investigation is warranted if you see this error, but it should be
    /// extremely rare in practice.
    #[error("server returned an unparseable `server_version_num`: `{raw}`")]
    ServerVersionParse { raw: String },
}

pub type Result<T> = std::result::Result<T, Error>;
