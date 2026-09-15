use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::bindable::{Bind, Column};
use crate::statement::{RemoteSqlValue, Statement};

/// Async SQL transport used by wasm `query!` and `db::prepare_web_database`.
///
/// The server methods are already async-capable (`Sql::*` over the existing RPC),
/// so this client awaits them instead of blocking the browser thread.
pub trait AsyncSqlClient: Send + Sync {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value>>;
}

static SQL_ENDPOINT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SQL_RPC_ENDPOINT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static ASYNC_CLIENT: OnceLock<Mutex<Option<Arc<dyn AsyncSqlClient>>>> = OnceLock::new();

fn sql_endpoint() -> &'static Mutex<Option<String>> {
    SQL_ENDPOINT.get_or_init(|| Mutex::new(None))
}

fn sql_rpc_endpoint() -> &'static Mutex<Option<String>> {
    SQL_RPC_ENDPOINT.get_or_init(|| Mutex::new(None))
}

fn async_client() -> &'static Mutex<Option<Arc<dyn AsyncSqlClient>>> {
    ASYNC_CLIENT.get_or_init(|| Mutex::new(None))
}

fn client_id() -> &'static str {
    "web"
}

pub fn set_sql_endpoint(endpoint: impl Into<String>) {
    *sql_endpoint().lock() = Some(endpoint.into());
}

pub fn set_sql_rpc_endpoint(endpoint: impl Into<String>) {
    *sql_rpc_endpoint().lock() = Some(endpoint.into());
}

pub fn set_async_sql_client(client: impl AsyncSqlClient + 'static) {
    *async_client().lock() = Some(Arc::new(client));
}

fn attach_client_id(mut params: Value) -> Value {
    if let Value::Object(map) = &mut params {
        map.insert("client_id".to_string(), json!(client_id()));
    }
    params
}

async fn sql_call(method: &str, params: Value) -> Result<Value> {
    let client = async_client().lock().clone().with_context(|| {
        format!(
            "async SQL client not configured (rpc {}, http {}); call set_async_sql_client",
            sql_rpc_endpoint().lock().as_deref().unwrap_or("unset"),
            sql_endpoint().lock().as_deref().unwrap_or("unset"),
        )
    })?;
    client.call(method, attach_client_id(params)).await
}

fn encode_remote_value(value: &RemoteSqlValue) -> Value {
    match value {
        RemoteSqlValue::Null => Value::Null,
        RemoteSqlValue::Integer(value) => json!({"type": "int", "value": value}),
        RemoteSqlValue::Real(value) => json!({"type": "float", "value": value}),
        RemoteSqlValue::Text(value) => json!({"type": "text", "value": value}),
        RemoteSqlValue::Blob(value) => json!({"type": "blob", "data": BASE64.encode(value)}),
    }
}

fn decode_row_value(value: &Value) -> Result<RemoteSqlValue> {
    match value {
        Value::Null => Ok(RemoteSqlValue::Null),
        Value::Bool(value) => Ok(RemoteSqlValue::Integer(i64::from(*value))),
        Value::Number(value) => value
            .as_i64()
            .map(RemoteSqlValue::Integer)
            .or_else(|| value.as_f64().map(RemoteSqlValue::Real))
            .context("SQL number was not int or float"),
        Value::String(value) => Ok(RemoteSqlValue::Text(value.clone())),
        Value::Array(values) => Ok(RemoteSqlValue::Blob(
            values
                .iter()
                .filter_map(Value::as_u64)
                .map(|value| value as u8)
                .collect(),
        )),
        Value::Object(value) => match value.get("type").and_then(Value::as_str) {
            Some("blob") => {
                let data = value
                    .get("data")
                    .and_then(Value::as_str)
                    .context("blob value missing data")?;
                Ok(RemoteSqlValue::Blob(
                    BASE64
                        .decode(data)
                        .context("blob value was not valid base64")?,
                ))
            }
            Some("int") => Ok(RemoteSqlValue::Integer(
                value
                    .get("value")
                    .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
                    .context("int value missing")?,
            )),
            Some("float") => Ok(RemoteSqlValue::Real(
                value
                    .get("value")
                    .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
                    .context("float value missing")?,
            )),
            Some("text") => Ok(RemoteSqlValue::Text(
                value
                    .get("value")
                    .and_then(Value::as_str)
                    .context("text value missing")?
                    .to_string(),
            )),
            _ => Ok(RemoteSqlValue::Null),
        },
    }
}

fn bind_params<B: Bind>(bindings: B) -> Result<Vec<Value>> {
    let statement = Statement::unbound();
    statement.bind(&bindings, 1)?;
    Ok(statement
        .into_remote_bindings()
        .iter()
        .map(encode_remote_value)
        .collect())
}

fn decode_rows(result: &Value) -> Result<Vec<Vec<RemoteSqlValue>>> {
    let rows = result
        .get("rows")
        .and_then(Value::as_array)
        .context("Sql::query result missing rows")?;
    rows.iter()
        .map(|row| {
            let cells = row
                .as_array()
                .with_context(|| format!("Sql::query row was not an array: {row}"))?;
            cells.iter().map(decode_row_value).collect()
        })
        .collect()
}

fn column_from_row<C: Column>(row: Vec<RemoteSqlValue>) -> Result<C> {
    Statement::from_remote_row(row).column()
}

async fn query_sql(sql: &str, params: Vec<Value>) -> Result<Value> {
    sql_call(
        "Sql::query",
        json!({
            "sql": sql,
            "params": params,
        }),
    )
    .await
}

pub async fn exec(sql: &str) -> Result<()> {
    query_sql(sql, Vec::new()).await?;
    Ok(())
}

pub async fn exec_bound<B: Bind>(sql: &str, bindings: B) -> Result<()> {
    query_sql(sql, bind_params(bindings)?).await?;
    Ok(())
}

pub async fn select<C: Column>(sql: &str) -> Result<Vec<C>> {
    let result = query_sql(sql, Vec::new()).await?;
    decode_rows(&result)?
        .into_iter()
        .map(column_from_row)
        .collect()
}

pub async fn select_bound<B: Bind, C: Column>(sql: &str, bindings: B) -> Result<Vec<C>> {
    let result = query_sql(sql, bind_params(bindings)?).await?;
    decode_rows(&result)?
        .into_iter()
        .map(column_from_row)
        .collect()
}

pub async fn select_row<C: Column>(sql: &str) -> Result<Option<C>> {
    select_row_from(query_sql(sql, Vec::new()).await?)
}

pub async fn select_row_bound<B: Bind, C: Column>(sql: &str, bindings: B) -> Result<Option<C>> {
    select_row_from(query_sql(sql, bind_params(bindings)?).await?)
}

fn select_row_from<C: Column>(result: Value) -> Result<Option<C>> {
    let rows = decode_rows(&result)?;
    anyhow::ensure!(
        rows.len() <= 1,
        "maybe called with a query that returns more than one row."
    );
    rows.into_iter().next().map(column_from_row).transpose()
}

pub async fn script(sql: &str) -> Result<Value> {
    sql_call("Sql::script", json!({ "sql": sql })).await
}

pub async fn batch(queries: Vec<Value>) -> Result<Value> {
    sql_call("Sql::batch", json!({ "queries": queries })).await
}

#[derive(Debug)]
pub struct MigrationDrift {
    pub index: usize,
    pub stored: String,
    pub proposed: String,
}

#[derive(Debug)]
pub enum MigrateResult {
    Applied { applied: u64 },
    Drift { changes: Vec<MigrationDrift> },
}

pub async fn migrate(
    domain: &str,
    migrations: &[&str],
    allowed_changes: &[u64],
) -> Result<MigrateResult> {
    let result = sql_call(
        "Sql::migrate",
        json!({
            "domain": domain,
            "migrations": migrations,
            "allowed_changes": allowed_changes,
        }),
    )
    .await?;
    match result.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(MigrateResult::Applied {
            applied: result
                .get("applied")
                .and_then(Value::as_u64)
                .context("Sql::migrate ok result missing applied")?,
        }),
        Some("drift") => {
            let changes = result
                .get("changes")
                .and_then(Value::as_array)
                .context("Sql::migrate drift result missing changes")?
                .iter()
                .map(|change| {
                    Ok(MigrationDrift {
                        index: change
                            .get("index")
                            .and_then(Value::as_u64)
                            .context("drift change missing index")?
                            as usize,
                        stored: change
                            .get("stored")
                            .and_then(Value::as_str)
                            .context("drift change missing stored")?
                            .to_string(),
                        proposed: change
                            .get("proposed")
                            .and_then(Value::as_str)
                            .context("drift change missing proposed")?
                            .to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(MigrateResult::Drift { changes })
        }
        other => bail!("unexpected Sql::migrate status: {other:?}"),
    }
}
