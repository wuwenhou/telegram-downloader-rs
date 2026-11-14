//! Minimal, dependency-free SQLite binding.
//!
//! The build machine has no C toolchain, so linking `libsqlite3` (or compiling
//! the bundled amalgamation) is impossible. Windows 10/11 ship a real SQLite
//! engine as `winsqlite3.dll`; we load it at runtime and use the stable C API.
//! The database file is a normal SQLite 3 database and can be opened by any
//! SQLite tool.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use libloading::Library;

use crate::error::{Error, Result};

type RawDb = c_void;
type RawStmt = c_void;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;
const SQLITE_TRANSIENT: isize = -1;

type OpenV2 = unsafe extern "C" fn(
    filename: *const c_char,
    db: *mut *mut RawDb,
    flags: c_int,
    vfs: *const c_char,
) -> c_int;
type Close = unsafe extern "C" fn(db: *mut RawDb) -> c_int;
type Errmsg = unsafe extern "C" fn(db: *mut RawDb) -> *const c_char;
type Errcode = unsafe extern "C" fn(db: *mut RawDb) -> c_int;
type Prepare = unsafe extern "C" fn(
    db: *mut RawDb,
    sql: *const c_char,
    len: c_int,
    stmt: *mut *mut RawStmt,
    tail: *mut *const c_char,
) -> c_int;
type Finalize = unsafe extern "C" fn(stmt: *mut RawStmt) -> c_int;
type Step = unsafe extern "C" fn(stmt: *mut RawStmt) -> c_int;
type BindInt64 = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int, value: i64) -> c_int;
type BindDouble = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int, value: f64) -> c_int;
type BindText = unsafe extern "C" fn(
    stmt: *mut RawStmt,
    index: c_int,
    value: *const c_char,
    len: c_int,
    destructor: isize,
) -> c_int;
type BindBlob = unsafe extern "C" fn(
    stmt: *mut RawStmt,
    index: c_int,
    value: *const c_void,
    len: c_int,
    destructor: isize,
) -> c_int;
type BindNull = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> c_int;
type ColumnCount = unsafe extern "C" fn(stmt: *mut RawStmt) -> c_int;
type ColumnType = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> c_int;
type ColumnInt64 = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> i64;
type ColumnDouble = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> f64;
type ColumnText = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> *const c_char;
type ColumnBytes = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> c_int;
type ColumnName = unsafe extern "C" fn(stmt: *mut RawStmt, index: c_int) -> *const c_char;
type Exec = unsafe extern "C" fn(
    db: *mut RawDb,
    sql: *const c_char,
    callback: *const c_void,
    argument: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int;
type Free = unsafe extern "C" fn(pointer: *mut c_void);
type BusyTimeout = unsafe extern "C" fn(db: *mut RawDb, milliseconds: c_int) -> c_int;
type LastInsertRowid = unsafe extern "C" fn(db: *mut RawDb) -> i64;
type Changes = unsafe extern "C" fn(db: *mut RawDb) -> c_int;
type LibVersion = unsafe extern "C" fn() -> *const c_char;

struct Api {
    // Field order matters: the library must outlive the function pointers.
    _library: Library,
    open_v2: OpenV2,
    close: Close,
    errmsg: Errmsg,
    errcode: Errcode,
    prepare_v2: Prepare,
    finalize: Finalize,
    step: Step,
    bind_int64: BindInt64,
    bind_double: BindDouble,
    bind_text: BindText,
    bind_blob: BindBlob,
    bind_null: BindNull,
    column_count: ColumnCount,
    column_type: ColumnType,
    column_int64: ColumnInt64,
    column_double: ColumnDouble,
    column_text: ColumnText,
    column_bytes: ColumnBytes,
    column_name: ColumnName,
    exec: Exec,
    free: Free,
    busy_timeout: BusyTimeout,
    last_insert_rowid: LastInsertRowid,
    changes: Changes,
    lib_version: LibVersion,
}

impl Api {
    fn load() -> Result<Self> {
        // Prefer the Windows-provided engine, but accept a normal sqlite3.dll
        // when one is available on PATH.
        let library = ["winsqlite3.dll", "sqlite3.dll"]
            .iter()
            .find_map(|name| unsafe { Library::new(name).ok() })
            .ok_or_else(|| {
                Error::Database(
                    "cannot load winsqlite3.dll or sqlite3.dll; Windows 10/11 is required".into(),
                )
            })?;

        unsafe {
            macro_rules! symbol {
                ($name:literal, $ty:ty) => {{
                    let symbol: libloading::Symbol<'_, $ty> =
                        library.get($name).map_err(|error| {
                            Error::Database(format!(
                                "SQLite symbol {} is missing: {error}",
                                String::from_utf8_lossy(&$name[..$name.len() - 1])
                            ))
                        })?;
                    *symbol
                }};
            }

            Ok(Self {
                open_v2: symbol!(b"sqlite3_open_v2\0", OpenV2),
                close: symbol!(b"sqlite3_close\0", Close),
                errmsg: symbol!(b"sqlite3_errmsg\0", Errmsg),
                errcode: symbol!(b"sqlite3_errcode\0", Errcode),
                prepare_v2: symbol!(b"sqlite3_prepare_v2\0", Prepare),
                finalize: symbol!(b"sqlite3_finalize\0", Finalize),
                step: symbol!(b"sqlite3_step\0", Step),
                bind_int64: symbol!(b"sqlite3_bind_int64\0", BindInt64),
                bind_double: symbol!(b"sqlite3_bind_double\0", BindDouble),
                bind_text: symbol!(b"sqlite3_bind_text\0", BindText),
                bind_blob: symbol!(b"sqlite3_bind_blob\0", BindBlob),
                bind_null: symbol!(b"sqlite3_bind_null\0", BindNull),
                column_count: symbol!(b"sqlite3_column_count\0", ColumnCount),
                column_type: symbol!(b"sqlite3_column_type\0", ColumnType),
                column_int64: symbol!(b"sqlite3_column_int64\0", ColumnInt64),
                column_double: symbol!(b"sqlite3_column_double\0", ColumnDouble),
                column_text: symbol!(b"sqlite3_column_text\0", ColumnText),
                column_bytes: symbol!(b"sqlite3_column_bytes\0", ColumnBytes),
                column_name: symbol!(b"sqlite3_column_name\0", ColumnName),
                exec: symbol!(b"sqlite3_exec\0", Exec),
                free: symbol!(b"sqlite3_free\0", Free),
                busy_timeout: symbol!(b"sqlite3_busy_timeout\0", BusyTimeout),
                last_insert_rowid: symbol!(b"sqlite3_last_insert_rowid\0", LastInsertRowid),
                changes: symbol!(b"sqlite3_changes\0", Changes),
                lib_version: symbol!(b"sqlite3_libversion\0", LibVersion),
                _library: library,
            })
        }
    }
}

/// A SQLite value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::Real(value) => Some(*value as i64),
            Self::Text(value) => value.parse().ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|value| u64::try_from(value).ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Integer(value) => Some(*value as f64),
            Self::Real(value) => Some(*value),
            Self::Text(value) => value.parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// Conversion trait used by the [`params!`] macro.
pub trait IntoValue {
    fn into_value(self) -> Value;
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}
impl IntoValue for &str {
    fn into_value(self) -> Value {
        Value::Text(self.to_string())
    }
}
impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::Text(self)
    }
}
impl IntoValue for &String {
    fn into_value(self) -> Value {
        Value::Text(self.clone())
    }
}
impl IntoValue for i64 {
    fn into_value(self) -> Value {
        Value::Integer(self)
    }
}
impl IntoValue for i32 {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl IntoValue for u32 {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl IntoValue for u64 {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl IntoValue for usize {
    fn into_value(self) -> Value {
        Value::Integer(self as i64)
    }
}
impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Integer(i64::from(self))
    }
}
impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::Real(self)
    }
}
impl IntoValue for Vec<u8> {
    fn into_value(self) -> Value {
        Value::Blob(self)
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            Some(value) => value.into_value(),
            None => Value::Null,
        }
    }
}

/// Build a `Vec<Value>` from mixed expressions.
#[macro_export]
macro_rules! params {
    () => { Vec::<$crate::sqlite::Value>::new() };
    ($($value:expr),+ $(,)?) => {
        [$($crate::sqlite::IntoValue::into_value($value)),+]
    };
}

/// A result row with column names.
#[derive(Clone, Debug)]
pub struct Row {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl Row {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.columns
            .iter()
            .position(|column| column == name)
            .and_then(|index| self.values.get(index))
    }

    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(Value::as_i64)
    }

    pub fn get_u64(&self, name: &str) -> Option<u64> {
        self.get(name).and_then(Value::as_u64)
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(Value::as_f64)
    }

    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        self.get_str(name).map(str::to_string)
    }

    pub fn require_str(&self, name: &str) -> Result<&str> {
        self.get_str(name)
            .ok_or_else(|| Error::Database(format!("column {name} is missing or not text")))
    }

    pub fn require_i64(&self, name: &str) -> Result<i64> {
        self.get_i64(name)
            .ok_or_else(|| Error::Database(format!("column {name} is missing or not an integer")))
    }

    pub fn require_u64(&self, name: &str) -> Result<u64> {
        self.get_u64(name)
            .ok_or_else(|| Error::Database(format!("column {name} is missing or not unsigned")))
    }
}

/// A single SQLite connection.
pub struct Connection {
    api: Api,
    handle: *mut RawDb,
    path: PathBuf,
}

// SQLite is opened with FULLMUTEX and every public method is guarded by the
// owning `Database` mutex. The handle itself is only used by one thread at a
// time.
unsafe impl Send for Connection {}

impl Connection {
    pub fn open(path: &Path) -> Result<Self> {
        let api = Api::load()?;
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| Error::Database("database path contains a NUL byte".into()))?;

        let mut handle: *mut RawDb = std::ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
        let code = unsafe { (api.open_v2)(c_path.as_ptr(), &mut handle, flags, std::ptr::null()) };
        if code != SQLITE_OK {
            let message = if handle.is_null() {
                format!("sqlite open failed with code {code}")
            } else {
                let message = unsafe {
                    let pointer = (api.errmsg)(handle);
                    if pointer.is_null() {
                        format!("sqlite open failed with code {code}")
                    } else {
                        CStr::from_ptr(pointer).to_string_lossy().into_owned()
                    }
                };
                unsafe { (api.close)(handle) };
                message
            };
            return Err(Error::Database(format!(
                "cannot open {}: {message}",
                path.display()
            )));
        }

        unsafe { (api.busy_timeout)(handle, 10_000) };

        let connection = Self {
            api,
            handle,
            path: path.to_path_buf(),
        };
        // WAL improves multi-process read concurrency. Some winsqlite3 builds
        // refuse it; that is not fatal.
        let _ = connection.execute_batch("PRAGMA journal_mode=WAL;");
        connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        connection.execute_batch("PRAGMA synchronous=NORMAL;")?;
        Ok(connection)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// SQLite library version reported by the loaded DLL.
    pub fn library_version(&self) -> String {
        unsafe {
            let pointer = (self.api.lib_version)();
            if pointer.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }
        }
    }

    fn error(&self, context: &str) -> Error {
        let message = unsafe {
            let pointer = (self.api.errmsg)(self.handle);
            if pointer.is_null() {
                "unknown SQLite error".to_string()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }
        };
        let code = unsafe { (self.api.errcode)(self.handle) };
        Error::Database(format!("{context}: {message} (code {code})"))
    }

    fn prepare(&self, sql: &str) -> Result<*mut RawStmt> {
        let c_sql = CString::new(sql)
            .map_err(|_| Error::Database("SQL statement contains a NUL byte".into()))?;
        let mut statement: *mut RawStmt = std::ptr::null_mut();
        let code = unsafe {
            (self.api.prepare_v2)(
                self.handle,
                c_sql.as_ptr(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        if code != SQLITE_OK {
            return Err(self.error(&format!("prepare failed for `{sql}`")));
        }
        Ok(statement)
    }

    fn bind(&self, statement: *mut RawStmt, values: &[Value]) -> Result<()> {
        for (index, value) in values.iter().enumerate() {
            let index = (index + 1) as c_int;
            let code = unsafe {
                match value {
                    Value::Null => (self.api.bind_null)(statement, index),
                    Value::Integer(number) => (self.api.bind_int64)(statement, index, *number),
                    Value::Real(number) => (self.api.bind_double)(statement, index, *number),
                    Value::Text(text) => (self.api.bind_text)(
                        statement,
                        index,
                        text.as_ptr() as *const c_char,
                        text.len() as c_int,
                        SQLITE_TRANSIENT,
                    ),
                    Value::Blob(bytes) => (self.api.bind_blob)(
                        statement,
                        index,
                        bytes.as_ptr() as *const c_void,
                        bytes.len() as c_int,
                        SQLITE_TRANSIENT,
                    ),
                }
            };
            if code != SQLITE_OK {
                return Err(self.error("bind failed"));
            }
        }
        Ok(())
    }

    /// Execute a statement and return the number of changed rows.
    pub fn execute(&self, sql: &str, values: &[Value]) -> Result<usize> {
        let statement = self.prepare(sql)?;
        let guard = StatementGuard {
            api: &self.api,
            statement,
        };

        self.bind(statement, values)?;
        loop {
            let code = unsafe { (self.api.step)(statement) };
            match code {
                SQLITE_DONE => break,
                SQLITE_ROW => continue,
                _ => return Err(self.error(&format!("execution failed for `{sql}`"))),
            }
        }
        let changed = unsafe { (self.api.changes)(self.handle) };
        drop(guard);
        Ok(changed.max(0) as usize)
    }

    /// Run a query and materialise every row.
    pub fn query(&self, sql: &str, values: &[Value]) -> Result<Vec<Row>> {
        let statement = self.prepare(sql)?;
        let guard = StatementGuard {
            api: &self.api,
            statement,
        };
        self.bind(statement, values)?;

        let column_count = unsafe { (self.api.column_count)(statement) };
        let mut columns = Vec::with_capacity(column_count.max(0) as usize);
        for index in 0..column_count {
            let name = unsafe {
                let pointer = (self.api.column_name)(statement, index);
                if pointer.is_null() {
                    format!("column_{index}")
                } else {
                    CStr::from_ptr(pointer).to_string_lossy().into_owned()
                }
            };
            columns.push(name);
        }

        let mut rows = Vec::new();
        loop {
            let code = unsafe { (self.api.step)(statement) };
            match code {
                SQLITE_ROW => {
                    let mut values = Vec::with_capacity(column_count.max(0) as usize);
                    for index in 0..column_count {
                        values.push(unsafe { self.column_value(statement, index) });
                    }
                    rows.push(Row {
                        columns: columns.clone(),
                        values,
                    });
                }
                SQLITE_DONE => break,
                _ => return Err(self.error(&format!("query failed for `{sql}`"))),
            }
        }
        drop(guard);
        Ok(rows)
    }

    unsafe fn column_value(&self, statement: *mut RawStmt, index: c_int) -> Value {
        match (self.api.column_type)(statement, index) {
            1 => Value::Integer((self.api.column_int64)(statement, index)),
            2 => Value::Real((self.api.column_double)(statement, index)),
            3 => {
                let pointer = (self.api.column_text)(statement, index);
                let length = (self.api.column_bytes)(statement, index).max(0) as usize;
                if pointer.is_null() {
                    Value::Text(String::new())
                } else {
                    let bytes = std::slice::from_raw_parts(pointer as *const u8, length);
                    Value::Text(String::from_utf8_lossy(bytes).into_owned())
                }
            }
            4 => {
                let pointer = (self.api.column_text)(statement, index);
                let length = (self.api.column_bytes)(statement, index).max(0) as usize;
                if pointer.is_null() {
                    Value::Blob(Vec::new())
                } else {
                    Value::Blob(std::slice::from_raw_parts(pointer as *const u8, length).to_vec())
                }
            }
            _ => Value::Null,
        }
    }

    /// Execute several statements separated by semicolons.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql)
            .map_err(|_| Error::Database("SQL batch contains a NUL byte".into()))?;
        let mut error_pointer: *mut c_char = std::ptr::null_mut();
        let code = unsafe {
            (self.api.exec)(
                self.handle,
                c_sql.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut error_pointer,
            )
        };
        if code != SQLITE_OK {
            let message = if error_pointer.is_null() {
                "unknown SQLite error".to_string()
            } else {
                let message =
                    unsafe { CStr::from_ptr(error_pointer).to_string_lossy().into_owned() };
                unsafe { (self.api.free)(error_pointer as *mut c_void) };
                message
            };
            return Err(Error::Database(format!("SQL batch failed: {message}")));
        }
        Ok(())
    }

    pub fn last_insert_rowid(&self) -> i64 {
        unsafe { (self.api.last_insert_rowid)(self.handle) }
    }
}

struct StatementGuard<'a> {
    api: &'a Api,
    statement: *mut RawStmt,
}

impl Drop for StatementGuard<'_> {
    fn drop(&mut self) {
        if !self.statement.is_null() {
            unsafe { (self.api.finalize)(self.statement) };
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.api.close)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// Thread-safe handle to a SQLite database.
pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn with<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard: MutexGuard<'_, Connection> = self
            .connection
            .lock()
            .map_err(|_| Error::Database("database mutex is poisoned".into()))?;
        operation(&guard)
    }

    /// Run `operation` inside an immediate transaction.
    pub fn transaction<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with(|connection| {
            connection.execute_batch("BEGIN IMMEDIATE;")?;
            match operation(connection) {
                Ok(value) => {
                    connection.execute_batch("COMMIT;")?;
                    Ok(value)
                }
                Err(error) => {
                    let _ = connection.execute_batch("ROLLBACK;");
                    Err(error)
                }
            }
        })
    }

    pub fn library_version(&self) -> String {
        self.with(|connection| Ok(connection.library_version()))
            .unwrap_or_else(|_| "unknown".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_round_trips_values() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(&directory.path().join("test.db")).unwrap();
        database
            .with(|connection| {
                connection.execute_batch(
                    "CREATE TABLE sample(id INTEGER PRIMARY KEY, name TEXT, size INTEGER);",
                )?;
                connection.execute(
                    "INSERT INTO sample(name, size) VALUES (?1, ?2)",
                    &params!["alpha", 42i64],
                )?;
                let rows = connection.query("SELECT id, name, size FROM sample", &[])?;
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].require_str("name").unwrap(), "alpha");
                assert_eq!(rows[0].require_i64("size").unwrap(), 42);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn transaction_rolls_back() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(&directory.path().join("test.db")).unwrap();
        database
            .with(|connection| {
                connection.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY);")
            })
            .unwrap();

        let result: Result<()> = database.transaction(|connection| {
            connection.execute("INSERT INTO sample DEFAULT VALUES", &[])?;
            Err(Error::Other("boom".into()))
        });
        assert!(result.is_err());
        database
            .with(|connection| {
                let rows = connection.query("SELECT COUNT(*) AS total FROM sample", &[])?;
                assert_eq!(rows[0].require_i64("total").unwrap(), 0);
                Ok(())
            })
            .unwrap();
    }
}
