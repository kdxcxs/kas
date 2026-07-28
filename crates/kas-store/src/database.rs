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
use postgres::{
    types::{ToSql as PgToSql, Type},
    NoTls,
};
use r2d2::{Pool, PooledConnection};
use r2d2_postgres::PostgresConnectionManager;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::{ToSqlOutput, ValueRef};
use serde_json::Value as JsonValue;

type SqlitePool = Pool<SqliteConnectionManager>;
type SqliteConnection = PooledConnection<SqliteConnectionManager>;
type PostgresManager = PostgresConnectionManager<NoTls>;
type PostgresPool = Pool<PostgresManager>;
type PostgresConnection = PooledConnection<PostgresManager>;

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    Postgres(postgres::Error),
    Pool(String),
    Decode(String),
    NoRows,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::Postgres(error) => {
                if let Some(database) = error.as_db_error() {
                    write!(
                        formatter,
                        "{} (SQLSTATE {})",
                        database.message(),
                        database.code().code()
                    )?;
                    if let Some(detail) = database.detail() {
                        write!(formatter, ": {detail}")?;
                    }
                    Ok(())
                } else {
                    write!(formatter, "{error}")
                }
            }
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

impl From<postgres::Error> for Error {
    fn from(error: postgres::Error) -> Self {
        Self::Postgres(error)
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
pub(crate) struct SqliteDatabase {
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

pub(crate) struct SqliteWriterLease {
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
pub enum Connection {
    Sqlite(SqliteDatabase),
    Postgres(PostgresPool),
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
        Ok(Self::Sqlite(SqliteDatabase {
            pool,
            writer: Arc::new(SqliteWriterGate::default()),
        }))
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
        Ok(Self::Sqlite(SqliteDatabase {
            pool,
            writer: Arc::new(SqliteWriterGate::default()),
        }))
    }

    pub fn open_database(database: &str) -> Result<Self> {
        if database.starts_with("postgres://") || database.starts_with("postgresql://") {
            let config = database.parse().map_err(Error::Postgres)?;
            let manager = PostgresConnectionManager::new(config, NoTls);
            let pool = Pool::builder()
                .max_size(database_pool_size())
                .build(manager)
                .map_err(pool_error)?;
            Ok(Self::Postgres(pool))
        } else {
            Self::open(database)
        }
    }

    pub fn is_postgres(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    pub fn transaction(&self) -> Result<Transaction> {
        let connection = self.clone();
        run_database_blocking(move || match connection {
            Self::Sqlite(database) => {
                let writer = database.writer.acquire();
                let connection = database.pool.get().map_err(pool_error)?;
                connection.execute_batch("BEGIN IMMEDIATE")?;
                Ok(Transaction::Sqlite {
                    connection: Arc::new(Mutex::new(Some(connection))),
                    completed: Arc::new(AtomicBool::new(false)),
                    _writer: writer,
                })
            }
            Self::Postgres(pool) => {
                let mut connection = pool.get().map_err(pool_error)?;
                connection.batch_execute("BEGIN")?;
                Ok(Transaction::Postgres {
                    connection: Arc::new(Mutex::new(Some(connection))),
                    completed: Arc::new(AtomicBool::new(false)),
                })
            }
        })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let connection = self.clone();
        let sql = sql.to_owned();
        run_database_blocking(move || -> Result<()> {
            match connection {
                Self::Sqlite(database) => database
                    .pool
                    .get()
                    .map_err(pool_error)?
                    .execute_batch(&sql)?,
                Self::Postgres(pool) => pool.get().map_err(pool_error)?.batch_execute(&sql)?,
            }
            Ok(())
        })
    }

    pub fn prepare<'a>(&'a self, sql: &str) -> Result<Statement<'a>> {
        Ok(Statement {
            executor: Executor::Connection(self),
            sqlite_sql: sql.to_owned(),
            postgres_sql: sql.to_owned(),
        })
    }

    pub fn prepare_dialect<'a>(
        &'a self,
        sqlite_sql: &str,
        postgres_sql: &str,
    ) -> Result<Statement<'a>> {
        Ok(Statement {
            executor: Executor::Connection(self),
            sqlite_sql: sqlite_sql.to_owned(),
            postgres_sql: postgres_sql.to_owned(),
        })
    }

    pub fn query_row<T, F>(&self, sql: &str, params: Vec<Param>, mapper: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let rows = query_connection(self, sql, sql, params)?;
        let row = rows.first().ok_or(Error::NoRows)?;
        mapper(row)
    }

    pub fn query_row_dialect<T, F>(
        &self,
        sqlite_sql: &str,
        postgres_sql: &str,
        params: Vec<Param>,
        mapper: F,
    ) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let rows = query_connection(self, sqlite_sql, postgres_sql, params)?;
        let row = rows.first().ok_or(Error::NoRows)?;
        mapper(row)
    }

    pub fn pragma_update<T: rusqlite::types::ToSql>(
        &self,
        schema: Option<rusqlite::DatabaseName<'_>>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        if let Self::Sqlite(database) = self {
            database
                .pool
                .get()
                .map_err(pool_error)?
                .pragma_update(schema, pragma, value)?;
        }
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
        match self {
            Self::Sqlite(database) => {
                let connection = database.pool.get().map_err(pool_error)?;
                let value = connection.pragma_query_value(schema, pragma, |row| {
                    let row = sqlite_row(row)?;
                    mapper(&row).map_err(to_sqlite_error)
                })?;
                Ok(value)
            }
            Self::Postgres(_) => Err(Error::Decode(
                "PostgreSQL does not support SQLite pragmas".into(),
            )),
        }
    }
}

pub enum Transaction {
    Sqlite {
        connection: Arc<Mutex<Option<SqliteConnection>>>,
        completed: Arc<AtomicBool>,
        _writer: SqliteWriterLease,
    },
    Postgres {
        connection: Arc<Mutex<Option<PostgresConnection>>>,
        completed: Arc<AtomicBool>,
    },
}

enum TransactionConnection {
    Sqlite(Arc<Mutex<Option<SqliteConnection>>>),
    Postgres(Arc<Mutex<Option<PostgresConnection>>>),
}

impl Transaction {
    pub fn execute(&self, sql: &str, params: Vec<Param>) -> Result<usize> {
        self.execute_dialect(sql, sql, params)
    }

    pub fn execute_dialect(
        &self,
        sqlite_sql: &str,
        postgres_sql: &str,
        params: Vec<Param>,
    ) -> Result<usize> {
        let connection = match self {
            Self::Sqlite { connection, .. } => TransactionConnection::Sqlite(connection.clone()),
            Self::Postgres { connection, .. } => {
                TransactionConnection::Postgres(connection.clone())
            }
        };
        let sqlite_sql = sqlite_sql.to_owned();
        let postgres_sql = postgres_sql.to_owned();
        run_database_blocking(move || match connection {
            TransactionConnection::Sqlite(connection) => Ok(connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction")
                .execute(&sqlite_sql, rusqlite::params_from_iter(params.iter()))?),
            TransactionConnection::Postgres(connection) => {
                let values = postgres_values(&params);
                let references = postgres_references(&values);
                let postgres_sql = postgres_parameters(&postgres_sql);
                Ok(connection
                    .lock()
                    .expect("PostgreSQL transaction lock poisoned")
                    .as_mut()
                    .expect("active PostgreSQL transaction")
                    .execute(&postgres_sql, &references)? as usize)
            }
        })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let connection = match self {
            Self::Sqlite { connection, .. } => TransactionConnection::Sqlite(connection.clone()),
            Self::Postgres { connection, .. } => {
                TransactionConnection::Postgres(connection.clone())
            }
        };
        let sql = sql.to_owned();
        run_database_blocking(move || -> Result<()> {
            match connection {
                TransactionConnection::Sqlite(connection) => connection
                    .lock()
                    .expect("SQLite transaction lock poisoned")
                    .as_ref()
                    .expect("active SQLite transaction")
                    .execute_batch(&sql)?,
                TransactionConnection::Postgres(connection) => connection
                    .lock()
                    .expect("PostgreSQL transaction lock poisoned")
                    .as_mut()
                    .expect("active PostgreSQL transaction")
                    .batch_execute(&sql)?,
            }
            Ok(())
        })
    }

    pub fn prepare<'b>(&'b self, sql: &str) -> Result<Statement<'b>> {
        Ok(Statement {
            executor: Executor::Transaction(self),
            sqlite_sql: sql.to_owned(),
            postgres_sql: sql.to_owned(),
        })
    }

    pub fn prepare_dialect<'b>(
        &'b self,
        sqlite_sql: &str,
        postgres_sql: &str,
    ) -> Result<Statement<'b>> {
        Ok(Statement {
            executor: Executor::Transaction(self),
            sqlite_sql: sqlite_sql.to_owned(),
            postgres_sql: postgres_sql.to_owned(),
        })
    }

    pub fn query_row<T, F>(&self, sql: &str, params: Vec<Param>, mapper: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.query_row_dialect(sql, sql, params, mapper)
    }

    pub fn query_row_dialect<T, F>(
        &self,
        sqlite_sql: &str,
        postgres_sql: &str,
        params: Vec<Param>,
        mapper: F,
    ) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let rows = query_transaction(self, sqlite_sql, postgres_sql, params)?;
        let row = rows.first().ok_or(Error::NoRows)?;
        mapper(row)
    }

    pub fn pragma_update<T: rusqlite::types::ToSql>(
        &self,
        schema: Option<rusqlite::DatabaseName<'_>>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        if let Self::Sqlite { connection, .. } = self {
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction")
                .pragma_update(schema, pragma, value)?;
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        let (connection, completed) = match &self {
            Self::Sqlite {
                connection,
                completed,
                ..
            } => (
                TransactionConnection::Sqlite(connection.clone()),
                completed.clone(),
            ),
            Self::Postgres {
                connection,
                completed,
            } => (
                TransactionConnection::Postgres(connection.clone()),
                completed.clone(),
            ),
        };
        run_database_blocking(move || -> Result<()> {
            match connection {
                TransactionConnection::Sqlite(connection) => {
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
                }
                TransactionConnection::Postgres(connection) => {
                    connection
                        .lock()
                        .expect("PostgreSQL transaction lock poisoned")
                        .as_mut()
                        .expect("active PostgreSQL transaction")
                        .batch_execute("COMMIT")?;
                    completed.store(true, Ordering::Release);
                    connection
                        .lock()
                        .expect("PostgreSQL transaction lock poisoned")
                        .take();
                }
            }
            Ok(())
        })
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let rollback = match self {
            Self::Sqlite {
                connection,
                completed,
                ..
            } if !completed.load(Ordering::Acquire) => {
                completed.store(true, Ordering::Release);
                Some(TransactionConnection::Sqlite(connection.clone()))
            }
            Self::Postgres {
                connection,
                completed,
            } if !completed.load(Ordering::Acquire) => {
                completed.store(true, Ordering::Release);
                Some(TransactionConnection::Postgres(connection.clone()))
            }
            _ => None,
        };
        if let Some(connection) = rollback {
            let _ = run_database_blocking(move || -> Result<()> {
                match connection {
                    TransactionConnection::Sqlite(connection) => {
                        if let Some(connection) = connection
                            .lock()
                            .expect("SQLite transaction lock poisoned")
                            .take()
                        {
                            connection.execute_batch("ROLLBACK")?;
                        }
                    }
                    TransactionConnection::Postgres(connection) => {
                        if let Some(mut connection) = connection
                            .lock()
                            .expect("PostgreSQL transaction lock poisoned")
                            .take()
                        {
                            connection.batch_execute("ROLLBACK")?;
                        }
                    }
                }
                Ok(())
            });
        }
    }
}

enum Executor<'a> {
    Connection(&'a Connection),
    Transaction(&'a Transaction),
}

pub struct Statement<'a> {
    executor: Executor<'a>,
    sqlite_sql: String,
    postgres_sql: String,
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
            Executor::Connection(connection) => {
                query_connection(connection, &self.sqlite_sql, &self.postgres_sql, params)?
            }
            Executor::Transaction(transaction) => {
                query_transaction(transaction, &self.sqlite_sql, &self.postgres_sql, params)?
            }
        };
        Ok(rows.iter().map(mapper).collect::<Vec<_>>().into_iter())
    }
}

fn query_connection(
    connection: &Connection,
    sqlite_sql: &str,
    postgres_sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    let connection = connection.clone();
    let sqlite_sql = sqlite_sql.to_owned();
    let postgres_sql = postgres_sql.to_owned();
    run_database_blocking(move || match connection {
        Connection::Sqlite(database) => {
            let connection = database.pool.get().map_err(pool_error)?;
            query_sqlite(&connection, &sqlite_sql, &params)
        }
        Connection::Postgres(pool) => {
            let mut connection = pool.get().map_err(pool_error)?;
            query_postgres(
                &mut connection,
                &postgres_parameters(&postgres_sql),
                &params,
            )
        }
    })
}

fn query_transaction(
    transaction: &Transaction,
    sqlite_sql: &str,
    postgres_sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    let connection = match transaction {
        Transaction::Sqlite { connection, .. } => TransactionConnection::Sqlite(connection.clone()),
        Transaction::Postgres { connection, .. } => {
            TransactionConnection::Postgres(connection.clone())
        }
    };
    let sqlite_sql = sqlite_sql.to_owned();
    let postgres_sql = postgres_sql.to_owned();
    run_database_blocking(move || match connection {
        TransactionConnection::Sqlite(connection) => query_sqlite(
            connection
                .lock()
                .expect("SQLite transaction lock poisoned")
                .as_ref()
                .expect("active SQLite transaction"),
            &sqlite_sql,
            &params,
        ),
        TransactionConnection::Postgres(connection) => {
            let mut connection = connection
                .lock()
                .expect("PostgreSQL transaction lock poisoned");
            query_postgres(
                connection.as_mut().expect("active PostgreSQL transaction"),
                &postgres_parameters(&postgres_sql),
                &params,
            )
        }
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

fn query_postgres(
    client: &mut postgres::Client,
    sql: &str,
    params: &[Param],
) -> Result<Vec<Row<'static>>> {
    let values = postgres_values(params);
    let references = postgres_references(&values);
    client
        .query(sql, &references)?
        .into_iter()
        .map(postgres_row)
        .collect()
}

fn postgres_row(row: postgres::Row) -> Result<Row<'static>> {
    let mut values = Vec::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let kind = column.type_();
        let value = if *kind == Type::INT8 {
            row.try_get::<_, Option<i64>>(index)?
                .map(CellValue::Integer)
                .unwrap_or(CellValue::Null)
        } else if *kind == Type::INT4 {
            row.try_get::<_, Option<i32>>(index)?
                .map(|value| CellValue::Integer(value as i64))
                .unwrap_or(CellValue::Null)
        } else if *kind == Type::BOOL {
            row.try_get::<_, Option<bool>>(index)?
                .map(|value| CellValue::Integer(i64::from(value)))
                .unwrap_or(CellValue::Null)
        } else if *kind == Type::JSON || *kind == Type::JSONB {
            row.try_get::<_, Option<JsonValue>>(index)?
                .map(|value| CellValue::Text(value.to_string()))
                .unwrap_or(CellValue::Null)
        } else if *kind == Type::TIMESTAMPTZ {
            row.try_get::<_, Option<DateTime<Utc>>>(index)?
                .map(|value| CellValue::Text(value.to_rfc3339_opts(SecondsFormat::Micros, true)))
                .unwrap_or(CellValue::Null)
        } else {
            row.try_get::<_, Option<String>>(index)?
                .map(CellValue::Text)
                .unwrap_or(CellValue::Null)
        };
        values.push(value);
    }
    Ok(Row {
        values,
        _lifetime: PhantomData,
    })
}

type PgValue = Box<dyn PgToSql + Sync>;

fn postgres_values(params: &[Param]) -> Vec<PgValue> {
    params
        .iter()
        .map(|value| match value {
            Param::Text(value) => Box::new(value.clone()) as PgValue,
            Param::Integer(value) => Box::new(*value) as PgValue,
            Param::Json(value) => Box::new(value.clone()) as PgValue,
            Param::Timestamp(value) => Box::new(*value) as PgValue,
        })
        .collect()
}

fn postgres_references(values: &[PgValue]) -> Vec<&(dyn PgToSql + Sync)> {
    values
        .iter()
        .map(|value| value.as_ref() as &(dyn PgToSql + Sync))
        .collect()
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

fn postgres_parameters(sql: &str) -> String {
    let mut converted = String::with_capacity(sql.len() + 16);
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut anonymous = 1usize;
    while index < bytes.len() {
        if bytes[index] != b'?' {
            converted.push(bytes[index] as char);
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let number = if start == index {
            let number = anonymous;
            anonymous += 1;
            number
        } else {
            let number = sql[start..index].parse::<usize>().unwrap_or(anonymous);
            anonymous = anonymous.max(number + 1);
            number
        };
        converted.push('$');
        converted.push_str(&number.to_string());
    }
    converted
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

    #[test]
    fn postgres_placeholders_preserve_explicit_and_anonymous_positions() {
        assert_eq!(
            postgres_parameters("SELECT ?1, ?, ?3, ?"),
            "SELECT $1, $2, $3, $4"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_work_leaves_the_tokio_runtime() {
        let has_runtime = run_database_blocking(|| tokio::runtime::Handle::try_current().is_ok());
        assert!(!has_runtime);
    }
}
