use std::fmt::Display;

use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};

pub use input::QueryMacroInput;
// pub use query::expand_query;

use std::env;

use crate::database::DatabaseExt;

use crate::query_macros::data::QueryData;
use crate::query_macros::input::RecordType;
use crate::runtime::block_on;
use sqlx::connection::Connect;
use sqlx::connection::Connection;
use sqlx::database::Database;
use std::borrow::Cow;
use std::path::PathBuf;
use syn::Type;
use url::Url;

mod args;
mod data;
mod input;
mod output;
// mod query;

pub fn expand_input(input: QueryMacroInput) -> crate::Result<TokenStream> {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").map_err(|_| "`CARGO_MANIFEST_DIR` must be set")?;

    // If a .env file exists at CARGO_MANIFEST_DIR, load environment variables from this,
    // otherwise fallback to default dotenv behaviour.
    let env_path = std::path::Path::new(&manifest_dir).join(".env");
    if env_path.exists() {
        dotenv::from_path(&env_path)
            .map_err(|e| format!("failed to load environment from {:?}, {}", env_path, e))?
    }

    // if `dotenv` wasn't initialized by the above we make sure to do it here
    match dotenv::var("DATABASE_URL").ok() {
        Some(db_url) => expand_from_db(input, &db_url),
        #[cfg(feature = "offline")]
        None => {
            let data_file_path = std::path::Path::new(&manifest_dir).join("sqlx-data.json");

            if data_file_path.exists() {
                expand_from_file(input, data_file_path)
            } else {
                Err(
                    "`DATABASE_URL` must be set, or `cargo sqlx prepare` must have been run \
                     and sqlx-data.json must exist, to use query macros"
                        .into(),
                )
            }
        }
        #[cfg(not(feature = "offline"))]
        None => Err("`DATABASE_URL` must be set to use query macros".into()),
    }
}

fn expand_from_db(input: QueryMacroInput, db_url: &str) -> crate::Result<TokenStream> {
    let db_url = Url::parse(db_url)?;
    match db_url.scheme() {
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let data = block_on(async {
                let mut conn = sqlx::postgres::PgConnection::connect(db_url).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },
        #[cfg(not(feature = "postgres"))]
        "postgres" | "postgresql" => Err(format!("database URL has the scheme of a PostgreSQL database but the `postgres` feature is not enabled").into()),
        #[cfg(feature = "mysql")]
        "mysql" | "mariadb" => {
            let data = block_on(async {
                let mut conn = sqlx::mysql::MySqlConnection::connect(db_url).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },
        #[cfg(not(feature = "mysql"))]
        "mysql" | "mariadb" => Err(format!("database URL has the scheme of a MySQL/MariaDB database but the `mysql` feature is not enabled").into()),
        #[cfg(feature = "sqlite")]
        "sqlite" => {
            let data = block_on(async {
                let mut conn = sqlx::sqlite::SqliteConnection::connect(db_url).await?;
                QueryData::from_db(&mut conn, &input.src).await
            })?;

            expand_with_data(input, data)
        },
        #[cfg(not(feature = "sqlite"))]
        "sqlite" => Err(format!("database URL has the scheme of a SQLite database but the `sqlite` feature is not enabled").into()),
        scheme => Err(format!("unknown database URL scheme {:?}", scheme).into())
    }
}

#[cfg(feature = "offline")]
pub fn expand_from_file(input: QueryMacroInput, file: PathBuf) -> crate::Result<TokenStream> {
    use data::offline::DynQueryData;

    let query_data = DynQueryData::from_data_file(file, &input.src)?;
    assert!(!query_data.db_name.is_empty());

    match query_data {
        #[cfg(feature = "postgres")]
        sqlx::postgres::Postgres::NAME => expand_with_data(
            input,
            QueryData::<sqlx::postgres::Postgres>::from_dyn_data(query_data)?,
        ),
        #[cfg(feature = "mysql")]
        sqlx::mysql::MySql::NAME => expand_with_data(
            input,
            QueryData::<sqlx::mysql::MySql>::from_dyn_data(query_data)?,
        ),
        #[cfg(feature = "sqlite")]
        sqlx::sqlite::Sqlite::NAME => expand_with_data(
            input,
            QueryData::<sqlx::sqlite::Sqlite>::from_dyn_data(query_data)?,
        ),
        _ => Err(format!(
            "found query data for {} but the feature for that database was not enabled"
        )
        .into()),
    }
}

fn expand_with_data<DB: DatabaseExt>(
    input: QueryMacroInput,
    data: QueryData<DB>,
) -> crate::Result<TokenStream> {
    // validate at the minimum that our args match the query's input parameters
    if input.arg_names.len() != data.describe.param_types.len() {
        return Err(syn::Error::new(
            Span::call_site(),
            format!(
                "expected {} parameters, got {}",
                data.describe.param_types.len(),
                input.arg_names.len()
            ),
        )
        .into());
    }

    let args_tokens = args::quote_args(&input, &data.describe)?;

    let query_args = format_ident!("query_args");

    let columns = output::columns_to_rust::<DB>(&data.describe)?;

    let (out_ty, record_tokens) = match input.record_type {
        RecordType::Generated => {
            let record_name: Type = syn::parse_str("Record").unwrap();

            let record_fields = columns.iter().map(
                |&output::RustColumn {
                     ref ident,
                     ref type_,
                 }| quote!(#ident: #type_,),
            );

            let record_tokens = quote! {
                #[derive(Debug)]
                struct #record_name {
                    #(#record_fields)*
                }
            };

            (Cow::Owned(record_name), record_tokens)
        }
        RecordType::Given(ref out_ty) => (Cow::Borrowed(out_ty), quote!()),
    };

    let output = output::quote_query_as::<DB>(&input, &out_ty, &query_args, &columns);

    let arg_names = &input.arg_names;

    let ret_tokens = quote! {
        macro_rules! macro_result {
            (#($#arg_names:expr),*) => {{
                use sqlx::arguments::Arguments as _;

                #record_tokens

                #args_tokens

                #output
            }}
        }
    };

    #[cfg(feature = "offline")]
    {
        let target_dir = env::var("TARGET_DIR")
            .map_or_else(
                || std::path::Path::new(&manifest_dir).join("target"),
                std::path::PathBuf::from,
            )
            .join("sqlx");

        data.save_in(target_dir, input.src_span)?;
    }

    Ok(ret_tokens)
}
