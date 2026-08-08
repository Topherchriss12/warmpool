//! Fast Postgres integration tests via fingerprinted, template cloned
//! databases.
//!
//! Running migrations from scratch for every integration test is the usual
//! bottleneck in a Postgres backed test suites. warmpool runs them exactly
//! once per distinct migration set hashing your migrations into a
//! deterministic template database name and every test after that simply
//! clones the already migrated template, which Postgres does in
//! milliseconds regardless of schema size. The cache survives across test
//! runs (and across `cargo test` invocations, and across CI jobs against the
//! same Postgres instance) because the template name only changes when the
//! migrations do.
//!
//! # For more control use [`TemplatePool`] directly.
//!
//! ```ignore
//! use sqlx::postgres::PgConnectOptions;
//!
//! # async fn example() -> warmpool::Result<()> {
//! let connect_options: PgConnectOptions = std::env::var("DATABASE_URL")
//!     .unwrap()
//!     .parse()
//!     .unwrap();
//!
//! let template = warmpool::TemplatePool::builder(connect_options)
//!     .migrations_from("./migrations")
//!     .exclude_migration(|m| m.description.contains("seed_data"))
//!     .build()
//!     .await?;
//!
//! let test_db = template.create_test_database().await?;
//! let pool = test_db.pool();
//!
//! // ... run your test against `pool` ... 
//!
//! test_db.drop_database().await?;
//! # Ok(())
//! // Encapsulate this bit of boilerplate in a function 
//! // or use the `#[warm_test]` macro below
//! # }
//! ```
//!
//! For most use cases use `#[warm_test]` which Requires the `macros` feature.
//!
//! ```ignore
//! #[warmpool::warm_test(migrations = "./migrations")]
//! async fn creates_a_post(pool: sqlx::PgPool) {
//!     // `pool` is a fresh, migrated clone of the template. It's dropped
//!     // automatically after this function returns on success or panic.
//! }
//! ```
//!
//! The macro reads `DATABASE_URL` for connection info by default; override
//! with `#[warm_test(database_url_env = "TEST_DATABASE_URL")]`.
//! The migrations path defaults to `./migrations` relative to the crate root; override 
//! with `#[warm_test(migrations = "./my_migrations")]`.

mod error;
mod fingerprint;
mod pool;

pub use error::{Error, Result};
pub use fingerprint::{fingerprint, lock_key_from_fingerprint};
pub use pool::{TemplatePool, TemplatePoolBuilder, TestDatabase};

#[cfg(feature = "macros")]
pub use warmpool_macros::warm_test;

/// Re exports used by macro generated code. Not part of the stable public
/// API the proc-macro crate depends on these paths, application code
/// does not.
#[doc(hidden)]
pub mod __private {
    pub use futures_util::FutureExt;
    pub use sqlx::postgres::PgConnectOptions;
    pub use std::panic::AssertUnwindSafe;
}
