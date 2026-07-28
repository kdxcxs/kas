use std::{cell::Cell, fmt, marker::PhantomData, path::Path, sync::mpsc};

use postgres::{
    types::{ToSql as PgToSql, Type},
    Client, NoTls,
};
use rusqlite::types::{ToSqlOutput, ValueRef};

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    Postgres(postgres::Error),
    Decode(String),
    NoRows,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::Postgres(error) => write!(formatter, "{error}"),
            Self::Decode(error) => formatter.write_str(error),
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
}

pub trait IntoParam {
    fn into_param(&self) -> Param;
}

impl IntoParam for str {
    fn into_param(&self) -> Param {
        Param::Text(Some(self.to_owned()))
    }
}

impl IntoParam for String {
    fn into_param(&self) -> Param {
        Param::Text(Some(self.clone()))
    }
}

impl<T: IntoParam + ?Sized> IntoParam for &T {
    fn into_param(&self) -> Param {
        (*self).into_param()
    }
}

impl IntoParam for Option<String> {
    fn into_param(&self) -> Param {
        Param::Text(self.clone())
    }
}

impl IntoParam for Option<&str> {
    fn into_param(&self) -> Param {
        Param::Text(self.map(str::to_owned))
    }
}

impl IntoParam for u64 {
    fn into_param(&self) -> Param {
        Param::Integer(*self as i64)
    }
}

impl IntoParam for usize {
    fn into_param(&self) -> Param {
        Param::Integer(*self as i64)
    }
}

impl IntoParam for i64 {
    fn into_param(&self) -> Param {
        Param::Integer(*self)
    }
}

impl rusqlite::types::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Text(Some(value)) => ToSqlOutput::Borrowed(ValueRef::Text(value.as_bytes())),
            Self::Text(None) => ToSqlOutput::Borrowed(ValueRef::Null),
            Self::Integer(value) => ToSqlOutput::Borrowed(ValueRef::Integer(*value)),
        })
    }
}

#[macro_export]
macro_rules! db_params {
    () => {
        Vec::<$crate::database::Param>::new()
    };
    ($($value:expr),+ $(,)?) => {
        vec![$($crate::database::IntoParam::into_param(&$value)),+]
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

pub enum Connection {
    Sqlite(rusqlite::Connection),
    Postgres(PostgresWorker),
}

impl Connection {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::Sqlite(rusqlite::Connection::open(path)?))
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self::Sqlite(rusqlite::Connection::open_in_memory()?))
    }

    pub fn open_database(database: &str) -> Result<Self> {
        if database.starts_with("postgres://") || database.starts_with("postgresql://") {
            Ok(Self::Postgres(PostgresWorker::connect(database)?))
        } else {
            Self::open(database)
        }
    }

    pub fn is_postgres(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        match self {
            Self::Sqlite(connection) => Ok(Transaction::Sqlite(Some(connection.transaction()?))),
            Self::Postgres(connection) => {
                connection.batch("BEGIN")?;
                Ok(Transaction::Postgres {
                    connection,
                    completed: Cell::new(false),
                })
            }
        }
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match self {
            Self::Sqlite(connection) => {
                connection.execute_batch(sql)?;
            }
            Self::Postgres(connection) => {
                connection.batch(sql)?;
            }
        }
        Ok(())
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
        if let Self::Sqlite(connection) = self {
            connection.pragma_update(schema, pragma, value)?;
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
            Self::Sqlite(connection) => {
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

pub enum Transaction<'a> {
    Sqlite(Option<rusqlite::Transaction<'a>>),
    Postgres {
        connection: &'a PostgresWorker,
        completed: Cell<bool>,
    },
}

impl<'a> Transaction<'a> {
    pub fn execute(&self, sql: &str, params: Vec<Param>) -> Result<usize> {
        match self {
            Self::Sqlite(transaction) => Ok(transaction
                .as_ref()
                .expect("active SQLite transaction")
                .execute(sql, rusqlite::params_from_iter(params.iter()))?),
            Self::Postgres { connection, .. } => {
                let sql = postgres_sql(sql);
                connection.execute(sql, params)
            }
        }
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match self {
            Self::Sqlite(transaction) => transaction
                .as_ref()
                .expect("active SQLite transaction")
                .execute_batch(sql)?,
            Self::Postgres { connection, .. } => connection.batch(sql)?,
        }
        Ok(())
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
        if let Self::Sqlite(transaction) = self {
            transaction
                .as_ref()
                .expect("active SQLite transaction")
                .pragma_update(schema, pragma, value)?;
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        match &mut self {
            Self::Sqlite(transaction) => transaction
                .take()
                .expect("active SQLite transaction")
                .commit()?,
            Self::Postgres {
                connection,
                completed,
            } => {
                connection.batch("COMMIT")?;
                completed.set(true);
            }
        }
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if let Self::Postgres {
            connection,
            completed,
        } = self
        {
            if !completed.get() {
                let _ = connection.batch("ROLLBACK");
                completed.set(true);
            }
        }
    }
}

enum Executor<'a> {
    Connection(&'a Connection),
    Transaction(&'a Transaction<'a>),
}

pub struct Statement<'a> {
    executor: Executor<'a>,
    sql: String,
}

impl Statement<'_> {
    pub fn query_map<T, F>(
        &mut self,
        params: Vec<Param>,
        mut mapper: F,
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
        Ok(rows
            .iter()
            .map(|row| mapper(row))
            .collect::<Vec<_>>()
            .into_iter())
    }
}

fn query_connection(
    connection: &Connection,
    sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    match connection {
        Connection::Sqlite(connection) => query_sqlite(connection, sql, &params),
        Connection::Postgres(connection) => connection.query(postgres_sql(sql), params),
    }
}

fn query_transaction(
    transaction: &Transaction<'_>,
    sql: &str,
    params: Vec<Param>,
) -> Result<Vec<Row<'static>>> {
    match transaction {
        Transaction::Sqlite(transaction) => query_sqlite(
            transaction.as_ref().expect("active SQLite transaction"),
            sql,
            &params,
        ),
        Transaction::Postgres { connection, .. } => connection.query(postgres_sql(sql), params),
    }
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

fn query_postgres(client: &mut Client, sql: &str, params: &[Param]) -> Result<Vec<Row<'static>>> {
    let values = postgres_values(params);
    let references = postgres_references(&values);
    let rows = client.query(sql, &references)?;
    rows.into_iter().map(postgres_row).collect()
}

enum PostgresRequest {
    Batch {
        sql: String,
        response: mpsc::SyncSender<Result<()>>,
    },
    Execute {
        sql: String,
        params: Vec<Param>,
        response: mpsc::SyncSender<Result<usize>>,
    },
    Query {
        sql: String,
        params: Vec<Param>,
        response: mpsc::SyncSender<Result<Vec<Row<'static>>>>,
    },
}

pub struct PostgresWorker {
    sender: mpsc::Sender<PostgresRequest>,
}

impl PostgresWorker {
    fn connect(database: &str) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let database = database.to_owned();
        std::thread::Builder::new()
            .name("kas-postgres".into())
            .spawn(move || {
                let mut client = match Client::connect(&database, NoTls) {
                    Ok(client) => {
                        let _ = ready_sender.send(Ok(()));
                        client
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(Error::Postgres(error)));
                        return;
                    }
                };
                while let Ok(request) = receiver.recv() {
                    match request {
                        PostgresRequest::Batch { sql, response } => {
                            let _ =
                                response.send(client.batch_execute(&sql).map_err(Error::Postgres));
                        }
                        PostgresRequest::Execute {
                            sql,
                            params,
                            response,
                        } => {
                            let values = postgres_values(&params);
                            let references = postgres_references(&values);
                            let result = client
                                .execute(&sql, &references)
                                .map(|changed| changed as usize)
                                .map_err(Error::Postgres);
                            let _ = response.send(result);
                        }
                        PostgresRequest::Query {
                            sql,
                            params,
                            response,
                        } => {
                            let _ = response.send(query_postgres(&mut client, &sql, &params));
                        }
                    }
                }
            })
            .map_err(|error| Error::Decode(format!("start PostgreSQL worker: {error}")))?;
        ready_receiver
            .recv()
            .map_err(|error| Error::Decode(format!("start PostgreSQL worker: {error}")))??;
        Ok(Self { sender })
    }

    fn batch(&self, sql: impl Into<String>) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(PostgresRequest::Batch {
                sql: sql.into(),
                response: sender,
            })
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?;
        receiver
            .recv()
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?
    }

    fn execute(&self, sql: String, params: Vec<Param>) -> Result<usize> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(PostgresRequest::Execute {
                sql,
                params,
                response: sender,
            })
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?;
        receiver
            .recv()
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?
    }

    fn query(&self, sql: String, params: Vec<Param>) -> Result<Vec<Row<'static>>> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(PostgresRequest::Query {
                sql,
                params,
                response: sender,
            })
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?;
        receiver
            .recv()
            .map_err(|error| Error::Decode(format!("PostgreSQL worker stopped: {error}")))?
    }
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
        })
        .collect()
}

fn postgres_references(values: &[PgValue]) -> Vec<&(dyn PgToSql + Sync)> {
    values
        .iter()
        .map(|value| value.as_ref() as &(dyn PgToSql + Sync))
        .collect()
}

fn postgres_sql(sql: &str) -> String {
    let mut sql = sql
        .replace(
            "json_extract(metadata,'$.manifest')",
            "(metadata::jsonb->>'manifest')",
        )
        .replace(
            "json_extract(metadata,'$.\"[kas]\".created_at')",
            "(metadata::jsonb#>>'{\"[kas]\",created_at}')",
        )
        .replace("json_extract(spec,'$.driver')", "(spec::jsonb->>'driver')")
        .replace(
            "json_extract(spec,'$.relation')",
            "(spec::jsonb->>'relation')",
        )
        .replace("json_extract(spec,'$.source')", "(spec::jsonb->>'source')")
        .replace("json_extract(spec,'$.target')", "(spec::jsonb->>'target')")
        .replace(
            "json_extract(spec,'$.token_hash')",
            "(spec::jsonb->>'token_hash')",
        )
        .replace(
            "json_extract(status,'$.metadata.state')",
            "(status::jsonb#>>'{metadata,state}')",
        );
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
    sql.clear();
    converted
}

fn to_sqlite_error(error: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
