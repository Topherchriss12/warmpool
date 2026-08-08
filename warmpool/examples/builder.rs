//! Run with: DATABASE_URL=postgres://postgres:password@localhost/postgres cargo run --example builder
//! Expects a `./migrations` directory relative to the crate root.
//! This example demonstrates the `TemplatePoolBuilder` API, which is what the `#[warm_test]` macro uses under the hood.
//! See the README for more details.

use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

#[tokio::main]
async fn main() -> warmpool::Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL, postgres://postgres:password@localhost/postgres");
    let connect_options = PgConnectOptions::from_str(&database_url).expect("invalid DATABASE_URL");

    let template = warmpool::TemplatePool::builder(connect_options)
        .migrations_from("./migrations")
        .exclude_migration(|m| m.description.contains("seed_data"))
        .purge_triggers_in("public")
        .build()
        .await?;

    let test_db = template.create_test_database().await?;
    println!("cloned test database: {}", test_db.name());

    // ... exercise `test_db.pool()` here ...

    test_db.drop_database().await?;
    Ok(())
}
