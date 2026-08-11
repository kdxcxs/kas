use std::{
    fmt,
    marker::PhantomData,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
        Arc, Condvar, Mutex, OnceLock,
    },
};

use chrono::{DateTime, SecondsFormat, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::{ToSqlOutput, ValueRef};
use serde_json::Value as JsonValue;

type SqlitePool = Pool<SqliteConnectionManager>;
type SqliteConnection = PooledConnection<SqliteConnectionManager>;

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    Pool(String),
    Decode(String),
    NoRows,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::Pool(error) | Self::Decode(error) => formatter.write_str(error),
            Self::NoRows => formatter.write_str("query returned no rows"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
            Self::NoRows
        } else {
            Self::Sqlite(error)
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub enum Param {
    Text(Option<String>),
    Integer(i64),
    Json(JsonValue),
    Timestamp(DateTime<Utc>),
}

pub trait IntoParam {
    fn as_param(&self) -> Param;
}

impl IntoParam for str {
    fn as_param(&self) -> Param {
        Param::Text(Some(self.to_owned()))
    }
}

impl IntoParam for String {
    fn as_param(&self) -> Param {
        Param::Text(Some(self.clone()))
    }
}

impl<T: IntoParam + ?Sized> IntoParam for &T {
    fn as_param(&self) -> Param {
        (*self).as_param()
    }
}

impl IntoParam for Option<String> {
    fn as_param(&self) -> Param {
        Param::Text(self.clone())
    }
}

impl IntoParam for Option<&str> {
    fn as_param(&self) -> Param {
        Param::Text(self.map(str::to_owned))
    }
}

impl IntoParam for u64 {
    fn as_param(&self) -> Param {
        Param::Integer(*self as i64)
    }
}

impl IntoParam for usize {
    fn as_param(&self) -> Param {
        Param::Integer(*self as i64)
    }
}

impl IntoParam for i64 {
    fn as_param(&self) -> Param {
        Param::Integer(*self)
    }
}

impl IntoParam for JsonValue {
    fn as_param(&self) -> Param {
        Param::Json(self.clone())
    }
}

impl IntoParam for DateTime<Utc> {
    fn as_param(&self) -> Param {
        Param::Timestamp(*self)
    }
}

impl rusqlite::types::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Text(Some(value)) => ToSqlOutput::Borrowed(ValueRef::Text(value.as_bytes())),
            Self::Text(None) => ToSqlOutput::Borrowed(ValueRef::Null),
            Self::Integer(value) => ToSqlOutput::Borrowed(ValueRef::Integer(*value)),
            Self::Json(value) => {
                ToSqlOutput::Owned(rusqlite::types::Value::Text(value.to_string()))
            }
            Self::Timestamp(value) => ToSqlOutput::Owned(rusqlite::types::Value::Text(
                value.to_rfc3339_opts(SecondsFormat::Micros, true),
            )),
        })
    }
}

#[macro_export]
macro_rules! db_params {
    () => {
        Vec::<$crate::database::Param>::new()
    };
    ($($value:expr),+ $(,)?) => {
        vec![$($crate::database::IntoParam::as_param(&$value)),+]
    };
}

#[derive(Debug, Clone)]
pub(crate) enum CellValue {
    Null,
    Integer(i64),
    Text(String),
}

pub struct Row<'a> {
    values: Vec<CellValue>,
    _lifetime: PhantomData<&'a ()>,
}

pub trait RowIndex {
    fn index(self) -> usize;
}

impl RowIndex for usize {
    fn index(self) -> usize {
        self
    }
}

pub trait FromCell: Sized {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self>;
}

impl FromCell for String {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self> {
        match value {
            CellValue::Text(value) => Ok(value.clone()),
            CellValue::Integer(value) => Ok(value.to_string()),
            CellValue::Null => Err(Error::Decode(format!("column {index} is null"))),
        }
    }
}

impl FromCell for u64 {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self> {
        match value {
            CellValue::Integer(value) if *value >= 0 => Ok(*value as u64),
            CellValue::Text(value) => value
                .parse()
                .map_err(|error| Error::Decode(format!("column {index}: {error}"))),
            _ => Err(Error::Decode(format!(
                "column {index} is not a non-negative integer"
            ))),
        }
    }
}

impl FromCell for i64 {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self> {
        match value {
            CellValue::Integer(value) => Ok(*value),
            CellValue::Text(value) => value
                .parse()
                .map_err(|error| Error::Decode(format!("column {index}: {error}"))),
            CellValue::Null => Err(Error::Decode(format!("column {index} is null"))),
        }
    }
}

impl FromCell for u32 {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self> {
        u64::from_cell(value, index).and_then(|value| {
            value
                .try_into()
                .map_err(|error| Error::Decode(format!("column {index}: {error}")))
        })
    }
}

impl<T: FromCell> FromCell for Option<T> {
    fn from_cell(value: &CellValue, index: usize) -> Result<Self> {
        if matches!(value, CellValue::Null) {
            Ok(None)
        } else {
            T::from_cell(value, index).map(Some)
        }
    }
}

impl<'a> Row<'a> {
    pub fn get<I: RowIndex, T: FromCell>(&self, index: I) -> Result<T> {
        let index = index.index();
        let value = self
            .values
            .get(index)
            .ok_or_else(|| Error::Decode(format!("column {index} does not exist")))?;
        T::from_cell(value, index)
    }
}

pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::NoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone)]
struct SqliteDatabase {
    pool: SqlitePool,
    writer: Arc<SqliteWriterGate>,
}

#[derive(Default)]
struct SqliteWriterGate {
    active: Mutex<bool>,
    available: Condvar,
}

impl SqliteWriterGate {
    fn acquire(self: &Arc<Self>) -> SqliteWriterLease {
        let mut active = self.active.lock().expect("SQLite writer gate poisoned");
        while *active {
            active = self
                .available
                .wait(active)
                .expect("SQLite writer gate poisoned");
        }
        *active = true;
        SqliteWriterLease { gate: self.clone() }
    }
}

struct SqliteWriterLease {
    gate: Arc<SqliteWriterGate>,
}

impl Drop for SqliteWriterLease {
    fn drop(&mut self) {
        *self
            .gate
            .active
            .lock()
            .expect("SQLite writer gate poisoned") = false;
        self.gate.available.notify_one();
    }
}

#[derive(Clone)]
pub struct Connection {
    database: SqliteDatabase,
}

impl Connection {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path).with_init(|connection| {
            connection.execute_batch(
                "PRAGMA foreign_keys=ON;
                 PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;",
            )
        });
        let pool = Pool::builder()
            .max_size(database_pool_size())
            .build(manager)
            .map_err(pool_error)?;
        Ok(Self {
            database: SqliteDatabase {
                pool,
                writer: Arc::new(SqliteWriterGate::default()),
            },
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let manager = SqliteConnectionManager::memory().with_init(|connection| {
            connection.execute_batch(
                "PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )
        });
        let pool = Pool::builder()
            .max_size(1)
            .build(manager)
            .map_err(pool_error)?;
        Ok(Self {
            database: SqliteDatabase {
                pool,
                writer: Arc::new(SqliteWriterGate::default()),
            },
        })
    }

    pub fn transaction(&self) -> Result<Transaction> {
        let database = self.database.clone();
        run_database_blocking(move || {
            let writer = database.writer.acquire();
            let connection = database.pool.get().map_err(pool_error)?;
            connection.execute_batch("BEGIN IMMEDIATE")?;
            Ok(Transaction {
                connection: Arc::new(Mutex::new(Some(connection))),
                completed: Arc::new(AtomicBool::new(false)),
                _writer: writer,
            })
        })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let database = self.database.clone();
        let sql = sql.to_owned();
        run_database_blocking(move || -> Result<()> {
            database
                .pool
                .get()
                .map_err(pool_error)?
                .execute_batch(&sql)?;
            Ok(())
        })
    }

    pub fn prepare<'a>(&'a self, sql: &str) -> Result<Statement<'a>> {
        Ok(Statement {
            executor: Executor::Connection(self),
            sql: sql.to_owned(),
        })
    }

    pub fn query_row<T, F>(&self, sql: &str, params: Vec<Param>, mapper: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let rows = query_connection(self, sql, params)?;
        let row = rows.first().ok_or(Error::NoRows)?;
        mapper(row)
    }

    pub fn pragma_update<T: rusqlite::types::ToSql>(
        &self,
        schema: Option<rusqlite::DatabaseName<'_>>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        self.database
            .pool
            .get()
            .map_err(pool_error)?
            .pragma_update(schema, pragma, value)?;
        Ok(())
    }

    pub fn pragma_query_value<T, F>(
        &self,
        schema: Option<rusqlite::DatabaseName<'_>>,
        pragma: &str,
        mapper: F,
    ) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let connection = self.database.pool.get().map_err(pool_error)?;
        let value = connection.pragma_query_value(schema, pragma, |row| {
            let row = sqlite_row(row)?;
            mapper(&row).map_err(to_sqlite_error)
        })?;
        Ok(value)
    }
}

pub struct Transaction {
    connection: Arc<Mutex<Option<SqliteConnection>>>,
    completed: Arc<AtomicBool>,
    _writer: SqliteWriterLease,
}

impl Transaction {
    pub fn execute(&self, sql: &str, params: Vec<Param>) -> Result<usize> {
        let connection = self.connection.clone();
        let sql = sql.to_owned();
        run_database_blocking(move || {
            Ok(connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction")
                .execute(&sql, rusqlite::params_from_iter(params.iter()))?)
        })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let connection = self.connection.clone();
        let sql = sql.to_owned();
        run_database_blocking(move || -> Result<()> {
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction")
                .execute_batch(&sql)?;
            Ok(())
        })
    }

    pub fn prepare<'b>(&'b self, sql: &str) -> Result<Statement<'b>> {
        Ok(Statement {
            executor: Executor::Transaction(self),
            sql: sql.to_owned(),
        })
    }

    pub fn query_row<T, F>(&self, sql: &str, params: Vec<Param>, mapper: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let rows = query_transaction(self, sql, params)?;
        let row = rows.first().ok_or(Error::NoRows)?;
        mapper(row)
    }

    pub fn pragma_update<T: rusqlite::types::ToSql>(
        &self,
        schema: Option<rusqlite::DatabaseName<'_>>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        self.connection
            .lock()
            .expect("SQLite transaction lock poisoned")
            .as_ref()
            .expect("active SQLite transaction")
            .pragma_update(schema, pragma, value)?;
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        let connection = self.connection.clone();
        let completed = self.completed.clone();
        run_database_blocking(move || -> Result<()> {
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction")
                .execute_batch("COMMIT")?;
            completed.store(true, Ordering::Release);
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .take();
            Ok(())
        })
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.completed.swap(true, Ordering::AcqRel) {
            return;
        }
        let connection = self.connection.clone();
        let _ = run_database_blocking(move || -> Result<()> {
            if let Some(connection) = connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .take()
            {
                connection.execute_batch("ROLLBACK")?;
            }
            Ok(())
        });
    }
}

enum Executor<'a> {
    Connection(&'a Connection),
    Transaction(&'a Transaction),
}

pub struct Statement<'a> {
    executor: Executor<'a>,
    sql: String,
}

impl Statement<'_> {
    pub fn query_map<T, F>(
        &mut self,
        params: Vec<Param>,
        mapper: F,
    ) -> Result<std::vec::IntoIter<Result<T>>>
    where
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        let rows = match self.executor {
            Executor::Connection(connection) => query_connection(connection, &self.sql, params)?,
            Executor::Transaction(transaction) => {
                query_transaction(transaction, &self.sql, params)?
            }
        };
        Ok(rows.iter().map(mapper).collect::<Vec<_>>().into_iter())
    }
}

fn query_connection(
    connection: &Connection,
    sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    let database = connection.database.clone();
    let sql = sql.to_owned();
    run_database_blocking(move || {
        let connection = database.pool.get().map_err(pool_error)?;
        query_sqlite(&connection, &sql, &params)
    })
}

fn query_transaction(
    transaction: &Transaction,
    sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    let connection = transaction.connection.clone();
    let sql = sql.to_owned();
    run_database_blocking(move || {
        query_sqlite(
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction"),
            &sql,
            &params,
        )
    })
}

fn query_sqlite(
    connection: &rusqlite::Connection,
    sql: &str,
    params: &[Param],
) -> Result<Vec<Row<'static>>> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(sqlite_row(row)?);
    }
    Ok(result)
}

fn sqlite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row<'static>> {
    let mut values = Vec::with_capacity(row.as_ref().column_count());
    for index in 0..row.as_ref().column_count() {
        values.push(match row.get_ref(index)? {
            ValueRef::Null => CellValue::Null,
            ValueRef::Integer(value) => CellValue::Integer(value),
            ValueRef::Real(value) => CellValue::Text(value.to_string()),
            ValueRef::Text(value) => CellValue::Text(String::from_utf8_lossy(value).into_owned()),
            ValueRef::Blob(value) => CellValue::Text(String::from_utf8_lossy(value).into_owned()),
        });
    }
    Ok(Row {
        values,
        _lifetime: PhantomData,
    })
}

fn database_pool_size() -> u32 {
    std::env::var("KAS_DATABASE_POOL_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|size| *size > 0)
        .unwrap_or(16)
}

fn run_database_blocking<T: Send + 'static>(operation: impl FnOnce() -> T + Send + 'static) -> T {
    if tokio::runtime::Handle::try_current().is_err() || rayon::current_thread_index().is_some() {
        return operation();
    }
    static DATABASE_WORKERS: OnceLock<rayon::ThreadPool> = OnceLock::new();
    let workers = DATABASE_WORKERS.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(database_pool_size() as usize)
            .thread_name(|index| format!("kas-database-{index}"))
            .build()
            .expect("KAS database worker pool must start")
    });
    let (sender, receiver) = sync_channel::<std::thread::Result<T>>(0);
    workers.spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
        let _ = sender.send(result);
    });
    match receiver
        .recv()
        .expect("database worker must return a result")
    {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn pool_error(error: r2d2::Error) -> Error {
    Error::Pool(error.to_string())
}

fn to_sqlite_error(error: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn database_work_leaves_the_tokio_runtime() {
        let has_runtime = run_database_blocking(|| tokio::runtime::Handle::try_current().is_ok());
        assert!(!has_runtime);
    }
}
