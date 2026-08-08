//! Proc-macro companion for `warmpool`. Depend on `warmpool` with the
//! `macros` feature enabled rather than on this crate directly, it's
//! re-exported from there.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parse_macro_input, Expr, ExprLit, FnArg, ItemFn, Lit, MetaNameValue, Token};

struct WarmTestArgs {
    migrations: Option<String>,
    database_url_env: Option<String>,
}

impl Parse for WarmTestArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut migrations = None;
        let mut database_url_env = None;

        let pairs = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        for pair in pairs {
            let key = pair
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            let value = match &pair.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) => s.value(),
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected a string literal, e.g. migrations = \"./migrations\"",
                    ))
                }
            };

            match key.as_str() {
                "migrations" => migrations = Some(value),
                "database_url_env" => database_url_env = Some(value),
                other => {
                    return Err(syn::Error::new_spanned(
                        &pair.path,
                        format!(
                            "unknown #[warm_test] argument `{other}` \
                             (expected `migrations` or `database_url_env`)"
                        ),
                    ))
                }
            }
        }

        Ok(WarmTestArgs {
            migrations,
            database_url_env,
        })
    }
}

///  Wraps an async test function so it receives a freshly cloned, migrated
/// `sqlx::PgPool` cloned from a template that's built once.
///  The test database is dropped after the function returns, whether it panics or succeeds. 
///  The template is built once per unique migration set, and reused across every test and every run,
///  so tests run in milliseconds regardless of schema size.
///
/// ```ignore
/// #[warmpool::warm_test(migrations = "./migrations")]
/// async fn creates_a_post(pool: sqlx::PgPool) {
///     let row: (i64,) = sqlx::query_as("SELECT count(*) FROM posts")
///         .fetch_one(&pool)
///         .await
///         .unwrap();
///     assert_eq!(row.0, 0);
/// }
/// ```
///
/// Arguments (both optional):
/// - `migrations = "./path"`  defaults to `./migrations`.
/// - `database_url_env = "ENV_VAR"` defaults to `DATABASE_URL`.
#[proc_macro_attribute]
pub fn warm_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as WarmTestArgs);
    let input = parse_macro_input!(item as ItemFn);

    let attrs = &input.attrs;
    let vis = &input.vis;
    let sig = &input.sig;
    let block = &input.block;
    let fn_name = &sig.ident;

    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(sig, "#[warm_test] functions must be `async fn`")
            .to_compile_error()
            .into();
    }

    let pool_ident = match sig.inputs.iter().collect::<Vec<_>>().as_slice() {
        [FnArg::Typed(pat_type)] => &pat_type.pat,
        _ => {
            return syn::Error::new_spanned(
                &sig.inputs,
                "#[warm_test] functions must take exactly one argument: \
                 `async fn my_test(pool: sqlx::PgPool)`",
            )
            .to_compile_error()
            .into()
        }
    };

    let migrations_path = args.migrations.unwrap_or_else(|| "./migrations".to_string());
    let database_url_env = args
        .database_url_env
        .unwrap_or_else(|| "DATABASE_URL".to_string());

    let expanded = quote! {
        #(#attrs)*
        #[::tokio::test]
        #vis async fn #fn_name() {
            let __warmpool_database_url = ::std::env::var(#database_url_env)
                .unwrap_or_else(|_| panic!(
                    "warmpool: environment variable `{}` is not set", #database_url_env
                ));

            let __warmpool_connect_options: ::warmpool::__private::PgConnectOptions =
                __warmpool_database_url.parse().unwrap_or_else(|e| panic!(
                    "warmpool: `{}` is not a valid Postgres connection string: {}",
                    #database_url_env, e
                ));

            let __warmpool_template = ::warmpool::TemplatePool::builder(__warmpool_connect_options)
                .migrations_from(#migrations_path)
                .build()
                .await
                .expect("warmpool: failed to build template pool");

            let __warmpool_test_db = __warmpool_template
                .create_test_database()
                .await
                .expect("warmpool: failed to clone test database from template");

            let #pool_ident = __warmpool_test_db.pool().clone();

            let __warmpool_result = ::warmpool::__private::FutureExt::catch_unwind(
                ::warmpool::__private::AssertUnwindSafe(async #block)
            )
            .await;

            if let Err(__warmpool_drop_err) = __warmpool_test_db.drop_database().await {
                eprintln!("warmpool: failed to drop test database: {__warmpool_drop_err}");
            }

            if let Err(__warmpool_panic) = __warmpool_result {
                ::std::panic::resume_unwind(__warmpool_panic);
            }
        }
    };

    expanded.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_no_args() {
        let args: WarmTestArgs = syn::parse_str("").unwrap();
        assert!(args.migrations.is_none());
        assert!(args.database_url_env.is_none());
    }

    #[test]
    fn parse_migrations_arg() {
        let args: WarmTestArgs = syn::parse_str(r#"migrations = "./migrations""#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
        assert!(args.database_url_env.is_none());
    }

    #[test]
    fn parse_database_url_env_arg() {
        let args: WarmTestArgs = syn::parse_str(r#"database_url_env = "TEST_DATABASE_URL""#).unwrap();
        assert!(args.migrations.is_none());
        assert_eq!(args.database_url_env.as_deref(), Some("TEST_DATABASE_URL"));
    }

    #[test]
    fn parse_both_args() {
        let args: WarmTestArgs = syn::parse_str(
            r#"migrations = "./migrations", database_url_env = "TEST_DATABASE_URL""#,
        )
        .unwrap();

        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
        assert_eq!(args.database_url_env.as_deref(), Some("TEST_DATABASE_URL"));
    }

    #[test]
    fn reject_unknown_argument() {
        assert!(syn::parse_str::<WarmTestArgs>(r#"foo = \"bar\""#).is_err());
    }

    #[test]
    fn reject_non_string_literal_argument() {
        assert!(syn::parse_str::<WarmTestArgs>(r#"migrations = 42"#).is_err());
    }
}
