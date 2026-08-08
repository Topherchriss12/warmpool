/// Everything that can go wrong while building a template database or
/// cloning a test database from it. Each variant carries enough context
/// (names, versions, descriptions) to act on without re deriving it from a
/// generic `sqlx::Error`.
/// The `source` field of each variant is the underlying `sqlx::Error` that caused
/// the failure, which can be downcast to `sqlx::error::DatabaseError` 
/// when you need to inspect the Postgres error code or message.
#[derive(Debug, thiserror::Error)]
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

    #[error(
        "migration {version} (\"{description}\") failed while building template `{template}`"
    )]
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
}

pub type Result<T> = std::result::Result<T, Error>;
