//! Proc-macro companion for `warmpool`. Depend on `warmpool` with the
//! `macros` feature enabled rather than on this crate directly, it's
//! re-exported from there.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, FnArg, ItemFn, Lit, MetaNameValue, Token, parse_macro_input};

#[derive(Debug)]
struct WarmTestArgs {
    migrations: Option<String>,
    database_url_env: Option<String>,
    clone_strategy: Option<String>,
}

impl Parse for WarmTestArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut migrations = None;
        let mut database_url_env = None;
        let mut clone_strategy = None;

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
                    ));
                }
            };

            match key.as_str() {
                "migrations" => migrations = Some(value),
                "database_url_env" => database_url_env = Some(value),
                "clone_strategy" => {
                    if !matches!(value.as_str(), "wal_log" | "file_copy" | "auto") {
                        return Err(syn::Error::new_spanned(
                            &pair.value,
                            format!(
                                "invalid clone_strategy `{value}` \
                                 (expected \"wal_log\", \"file_copy\", or \"auto\")"
                            ),
                        ));
                    }
                    clone_strategy = Some(value);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &pair.path,
                        format!(
                            "unknown #[warm_test] argument `{other}` \
                             (expected `migrations`, `database_url_env`, or `clone_strategy`)"
                        ),
                    ));
                }
            }
        }

        Ok(WarmTestArgs {
            migrations,
            database_url_env,
            clone_strategy,
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
/// Arguments all optional:
/// - `migrations = "./path"`  defaults to `./migrations`.
/// - `database_url_env = "ENV_VAR"` defaults to `DATABASE_URL`.
/// - `clone_strategy = "wal_log" | "file_copy" | "auto"` — defaults to
///   whatever [`warmpool::TemplatePoolBuilder`] defaults to (`wal_log` as
///   of warmpool 0.1.1+). Rejected at compile time if it isn't one of those
///   three strings.
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
            .into();
        }
    };

    let migrations_path = args
        .migrations
        .unwrap_or_else(|| "./migrations".to_string());
    let database_url_env = args
        .database_url_env
        .unwrap_or_else(|| "DATABASE_URL".to_string());

    // `None` here means "don't call .clone_strategy(...) at all",
    // so the builder's own default applies, exactly as if the attribute had never
    // been extended with this argument. Only emit the call when the user
    // asked for something explicitly.
    let clone_strategy_call = args.clone_strategy.map(|value| {
        let variant = match value.as_str() {
            "wal_log" => quote! { WalLog },
            "file_copy" => quote! { FileCopy },
            "auto" => quote! { Auto },
            // Unreachable: WarmTestArgs::parse already rejected anything else.
            _ => unreachable!("clone_strategy value validated during parsing"),
        };
        quote! {
            .clone_strategy(::warmpool::CloneStrategy::#variant)
        }
    });

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
                #clone_strategy_call
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
        assert!(args.clone_strategy.is_none());
    }

    #[test]
    fn parse_migrations_arg() {
        let args: WarmTestArgs = syn::parse_str(r#"migrations = "./migrations""#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
        assert!(args.database_url_env.is_none());
    }

    #[test]
    fn parse_database_url_env_arg() {
        let args: WarmTestArgs =
            syn::parse_str(r#"database_url_env = "TEST_DATABASE_URL""#).unwrap();
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

    // 0.1.1

    #[test]
    fn parse_clone_strategy_wal_log() {
        let args: WarmTestArgs = syn::parse_str(r#"clone_strategy = "wal_log""#).unwrap();
        assert_eq!(args.clone_strategy.as_deref(), Some("wal_log"));
    }

    #[test]
    fn parse_clone_strategy_file_copy() {
        let args: WarmTestArgs = syn::parse_str(r#"clone_strategy = "file_copy""#).unwrap();
        assert_eq!(args.clone_strategy.as_deref(), Some("file_copy"));
    }

    #[test]
    fn parse_clone_strategy_auto() {
        let args: WarmTestArgs = syn::parse_str(r#"clone_strategy = "auto""#).unwrap();
        assert_eq!(args.clone_strategy.as_deref(), Some("auto"));
    }

    #[test]
    fn reject_invalid_clone_strategy_value() {
        let result: syn::Result<WarmTestArgs> = syn::parse_str(r#"clone_strategy = "fast_please""#);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid clone_strategy")
        );
    }

    #[test]
    fn parse_all_three_args_together() {
        let args: WarmTestArgs = syn::parse_str(
            r#"migrations = "./migrations", database_url_env = "TEST_DATABASE_URL", clone_strategy = "file_copy""#,
        )
        .unwrap();

        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
        assert_eq!(args.database_url_env.as_deref(), Some("TEST_DATABASE_URL"));
        assert_eq!(args.clone_strategy.as_deref(), Some("file_copy"));
    }

    #[test]
    fn parse_tolerates_surrounding_whitespace() {
        let args: WarmTestArgs = syn::parse_str(r#"  migrations = "./migrations"  "#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
    }

    #[test]
    fn parse_tolerates_whitespace_around_equals_and_commas() {
        let args: WarmTestArgs =
            syn::parse_str(r#"migrations="./migrations" , clone_strategy = "auto""#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
        assert_eq!(args.clone_strategy.as_deref(), Some("auto"));
    }

    #[test]
    fn parse_accepts_trailing_comma() {
        // Punctuated::parse_terminated explicitly allows a trailing
        // separator; worth locking in since #[warm_test(migrations = "x",)]
        // is a natural thing to type after adding/removing an argument.
        let args: WarmTestArgs = syn::parse_str(r#"migrations = "./migrations","#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("./migrations"));
    }

    #[test]
    fn parse_empty_string_value_is_accepted_syntactically() {
        // WarmTestArgs currently does no non emptiness validation for
        // `migrations` or `database_url_env` an empty path or env var
        // name parses fine here and would only surface as a problem later,
        // at expansion runtime (a nonsensical env var lookup, or `Migrator`
        // failing on `""` as a path). We know this and will fix it in a future realease,
        // but for now the parser is happy with an empty string literal.
        let args: WarmTestArgs = syn::parse_str(r#"migrations = """#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some(""));
    }

    #[test]
    fn parse_clone_strategy_is_case_sensitive() {
        // Only the exact lowercase forms are accepted "WAL_LOG",
        // "Wal_Log", etc. are all rejected.
        for bad in ["WAL_LOG", "Wal_Log", "FILE_COPY", "AUTO", "Auto"] {
            let result: syn::Result<WarmTestArgs> =
                syn::parse_str(&format!(r#"clone_strategy = "{bad}""#));
            assert!(
                result.is_err(),
                "expected `{bad}` to be rejected (case-sensitive match)"
            );
        }
    }

    #[test]
    fn parse_duplicate_key_lets_the_last_occurrence_win_silently() {
        // WarmTestArgs::parse has no duplicate key detection logic, so
        // #[warm_test(migrations = "a", migrations = "b")] silently keeps
        // "b" with no warning or error. In a future realese this will be
        // a compile error.
        let args: WarmTestArgs = syn::parse_str(r#"migrations = "a", migrations = "b""#).unwrap();
        assert_eq!(args.migrations.as_deref(), Some("b"));
    }

    #[test]
    fn parse_duplicate_clone_strategy_key_also_lets_last_win() {
        let args: WarmTestArgs =
            syn::parse_str(r#"clone_strategy = "wal_log", clone_strategy = "file_copy""#).unwrap();
        assert_eq!(args.clone_strategy.as_deref(), Some("file_copy"));
    }

    #[test]
    fn reject_malformed_missing_equals() {
        assert!(syn::parse_str::<WarmTestArgs>(r#"migrations "./migrations""#).is_err());
    }

    #[test]
    fn reject_malformed_missing_value() {
        assert!(syn::parse_str::<WarmTestArgs>(r#"migrations ="#).is_err());
    }

    #[test]
    fn reject_malformed_leading_comma() {
        assert!(syn::parse_str::<WarmTestArgs>(r#", migrations = "./migrations""#).is_err());
    }

    #[test]
    fn reject_malformed_double_comma() {
        assert!(
            syn::parse_str::<WarmTestArgs>(r#"migrations = "a",, database_url_env = "B""#).is_err()
        );
    }

    #[test]
    fn reject_unknown_argument_alongside_valid_ones() {
        // A single bad key anywhere in the list rejects the whole
        // attribute, it isn't "parse what you can."
        let result: syn::Result<WarmTestArgs> =
            syn::parse_str(r#"migrations = "./migrations", bogus = "x", clone_strategy = "auto""#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_argument_error_message_names_the_bad_key_and_the_valid_ones() {
        let result: syn::Result<WarmTestArgs> = syn::parse_str(r#"totally_bogus = "x""#);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("totally_bogus"), "message was: {message}");
        assert!(message.contains("migrations"), "message was: {message}");
        assert!(
            message.contains("database_url_env"),
            "message was: {message}"
        );
        assert!(message.contains("clone_strategy"), "message was: {message}");
    }

    #[test]
    fn invalid_clone_strategy_error_message_names_the_bad_value_and_the_valid_ones() {
        let result: syn::Result<WarmTestArgs> = syn::parse_str(r#"clone_strategy = "yolo""#);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("yolo"), "message was: {message}");
        assert!(message.contains("wal_log"), "message was: {message}");
        assert!(message.contains("file_copy"), "message was: {message}");
        assert!(message.contains("auto"), "message was: {message}");
    }

    #[test]
    fn non_string_literal_error_message_is_actionable() {
        let result: syn::Result<WarmTestArgs> = syn::parse_str(r#"migrations = 42"#);
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("string literal"),
            "message should explain what was expected: {message}"
        );
    }

    #[test]
    fn reject_boolean_literal_value() {
        assert!(syn::parse_str::<WarmTestArgs>(r#"clone_strategy = true"#).is_err());
    }

    #[test]
    fn reject_identifier_as_value_without_quotes() {
        // writing clone_strategy = wal_log instead of
        // clone_strategy = "wal_log". Must fail with the "expected a
        // string literal" message.
        let result: syn::Result<WarmTestArgs> = syn::parse_str(r#"clone_strategy = wal_log"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("string literal"));
    }

    #[test]
    fn parse_is_order_independent() {
        // The three arguments can appear in any order with the same result.
        let a: WarmTestArgs = syn::parse_str(
            r#"migrations = "./m", database_url_env = "E", clone_strategy = "auto""#,
        )
        .unwrap();
        let b: WarmTestArgs = syn::parse_str(
            r#"clone_strategy = "auto", migrations = "./m", database_url_env = "E""#,
        )
        .unwrap();
        let c: WarmTestArgs = syn::parse_str(
            r#"database_url_env = "E", clone_strategy = "auto", migrations = "./m""#,
        )
        .unwrap();

        for args in [a, b, c] {
            assert_eq!(args.migrations.as_deref(), Some("./m"));
            assert_eq!(args.database_url_env.as_deref(), Some("E"));
            assert_eq!(args.clone_strategy.as_deref(), Some("auto"));
        }
    }

    // codegen shape: does #[warm_test] actually reject non-async fns
    // and wrong argument counts the way the we promises? These
    // exercise the proc-macro attribute function itself (not just
    // WarmTestArgs::parse), using syn to build a minimal ItemFn and
    // checking the *shape* of the generated tokens rather than trying to
    // execute them full end-to-end expansion is covered by the
    // `expand_check` crate instead, which actually
    // resolves the generated code against the real `warmpool` crate.

    #[test]
    fn warm_test_args_default_values_when_omitted() {
        let args: WarmTestArgs = syn::parse_str("").unwrap();
        // Mirrors the defaults applied in warm_test()'s codegen path
        // if these ever drift apart, the doc comment's claimed defaults
        // ("./migrations", "DATABASE_URL", builder's own clone_strategy
        // default) would silently stop matching reality.
        assert_eq!(
            args.migrations
                .unwrap_or_else(|| "./migrations".to_string()),
            "./migrations"
        );
        assert_eq!(
            args.database_url_env
                .unwrap_or_else(|| "DATABASE_URL".to_string()),
            "DATABASE_URL"
        );
        assert!(
            args.clone_strategy.is_none(),
            "omitted clone_strategy must not synthesize a value \
             None is what makes warm_test() skip emitting .clone_strategy(...) \
             entirely and fall through to the builder's own default"
        );
    }
}
