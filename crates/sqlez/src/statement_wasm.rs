use anyhow::{Result, bail};

use crate::bindable::{Bind, Column};
use crate::connection::Connection;

pub struct Statement<'a> {
    _connection: &'a Connection,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepResult {
    Row,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SqlType {
    Text,
    Integer,
    Blob,
    Float,
    Null,
}

fn sqlite_unsupported<T>() -> Result<T> {
    bail!("SQLite is not supported on wasm")
}

impl<'a> Statement<'a> {
    pub fn prepare<T: AsRef<str>>(_connection: &'a Connection, _query: T) -> Result<Self> {
        sqlite_unsupported()
    }

    pub fn reset(&mut self) {}

    pub fn parameter_count(&self) -> i32 {
        0
    }

    pub fn bind_blob(&self, _index: i32, _blob: &[u8]) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn column_blob(&mut self, _index: i32) -> Result<&[u8]> {
        sqlite_unsupported()
    }

    pub fn bind_double(&self, _index: i32, _double: f64) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn column_double(&self, _index: i32) -> Result<f64> {
        sqlite_unsupported()
    }

    pub fn bind_int(&self, _index: i32, _int: i32) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn column_int(&self, _index: i32) -> Result<i32> {
        sqlite_unsupported()
    }

    pub fn bind_int64(&self, _index: i32, _int: i64) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn column_int64(&self, _index: i32) -> Result<i64> {
        sqlite_unsupported()
    }

    pub fn bind_null(&self, _index: i32) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn bind_text(&self, _index: i32, _text: &str) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn column_text(&mut self, _index: i32) -> Result<&str> {
        sqlite_unsupported()
    }

    pub fn bind<T: Bind>(&self, _value: &T, _index: i32) -> Result<i32> {
        sqlite_unsupported()
    }

    pub fn column<T: Column>(&mut self) -> Result<T> {
        sqlite_unsupported()
    }

    pub fn column_type(&mut self, _index: i32) -> Result<SqlType> {
        sqlite_unsupported()
    }

    pub fn with_bindings(&mut self, _bindings: &impl Bind) -> Result<&mut Self> {
        sqlite_unsupported()
    }

    pub fn exec(&mut self) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn map<R>(&mut self, _callback: impl FnMut(&mut Statement) -> Result<R>) -> Result<Vec<R>> {
        sqlite_unsupported()
    }

    pub fn rows<R: Column>(&mut self) -> Result<Vec<R>> {
        sqlite_unsupported()
    }

    pub fn single<R>(&mut self, _callback: impl FnOnce(&mut Statement) -> Result<R>) -> Result<R> {
        sqlite_unsupported()
    }

    pub fn row<R: Column>(&mut self) -> Result<R> {
        sqlite_unsupported()
    }

    pub fn maybe<R>(
        &mut self,
        _callback: impl FnOnce(&mut Statement) -> Result<R>,
    ) -> Result<Option<R>> {
        sqlite_unsupported()
    }

    pub fn maybe_row<R: Column>(&mut self) -> Result<Option<R>> {
        sqlite_unsupported()
    }
}
