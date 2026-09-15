use std::cell::RefCell;
use std::marker::PhantomData;

use anyhow::{Context as _, Result, bail};

use crate::bindable::{Bind, Column};
use crate::connection::Connection;

#[derive(Clone, Debug)]
pub(crate) enum RemoteSqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

pub struct Statement<'a> {
    _connection: PhantomData<&'a Connection>,
    bindings: RefCell<Vec<RemoteSqlValue>>,
    row: Vec<RemoteSqlValue>,
    blob_scratch: Vec<u8>,
    text_scratch: String,
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

    pub(crate) fn unbound() -> Statement<'static> {
        Statement {
            _connection: PhantomData,
            bindings: RefCell::new(Vec::new()),
            row: Vec::new(),
            blob_scratch: Vec::new(),
            text_scratch: String::new(),
        }
    }

    pub(crate) fn from_remote_row(row: Vec<RemoteSqlValue>) -> Statement<'static> {
        Statement {
            _connection: PhantomData,
            bindings: RefCell::new(Vec::new()),
            row,
            blob_scratch: Vec::new(),
            text_scratch: String::new(),
        }
    }

    pub(crate) fn into_remote_bindings(self) -> Vec<RemoteSqlValue> {
        self.bindings.into_inner()
    }

    fn set_binding(&self, index: i32, value: RemoteSqlValue) -> Result<()> {
        if index < 1 {
            bail!("bind index must be >= 1");
        }
        let slot = (index as usize) - 1;
        let mut bindings = self.bindings.borrow_mut();
        if bindings.len() <= slot {
            bindings.resize(slot + 1, RemoteSqlValue::Null);
        }
        bindings[slot] = value;
        Ok(())
    }

    fn cell(&self, index: i32) -> Result<RemoteSqlValue> {
        let index = usize::try_from(index).context("negative column index")?;
        self.row
            .get(index)
            .cloned()
            .with_context(|| format!("column index {index} out of bounds"))
    }

    pub fn reset(&mut self) {}

    pub fn parameter_count(&self) -> i32 {
        self.bindings.borrow().len() as i32
    }

    pub fn bind_blob(&self, index: i32, blob: &[u8]) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Blob(blob.to_vec()))
    }

    pub fn column_blob(&mut self, index: i32) -> Result<&[u8]> {
        match self.cell(index)? {
            RemoteSqlValue::Blob(bytes) => {
                self.blob_scratch = bytes;
                Ok(&self.blob_scratch)
            }
            RemoteSqlValue::Text(text) => {
                self.blob_scratch = text.into_bytes();
                Ok(&self.blob_scratch)
            }
            RemoteSqlValue::Null => {
                self.blob_scratch.clear();
                Ok(&self.blob_scratch)
            }
            other => bail!("column {index} is not a blob ({other:?})"),
        }
    }

    pub fn bind_double(&self, index: i32, double: f64) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Real(double))
    }

    pub fn column_double(&self, index: i32) -> Result<f64> {
        match self.cell(index)? {
            RemoteSqlValue::Real(value) => Ok(value),
            RemoteSqlValue::Integer(value) => Ok(value as f64),
            other => bail!("column {index} is not a float ({other:?})"),
        }
    }

    pub fn bind_int(&self, index: i32, int: i32) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Integer(i64::from(int)))
    }

    pub fn column_int(&self, index: i32) -> Result<i32> {
        Ok(self.column_int64(index)? as i32)
    }

    pub fn bind_int64(&self, index: i32, int: i64) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Integer(int))
    }

    pub fn column_int64(&self, index: i32) -> Result<i64> {
        match self.cell(index)? {
            RemoteSqlValue::Integer(value) => Ok(value),
            RemoteSqlValue::Real(value) => Ok(value as i64),
            other => bail!("column {index} is not an integer ({other:?})"),
        }
    }

    pub fn bind_null(&self, index: i32) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Null)
    }

    pub fn bind_text(&self, index: i32, text: &str) -> Result<()> {
        self.set_binding(index, RemoteSqlValue::Text(text.to_string()))
    }

    pub fn column_text(&mut self, index: i32) -> Result<&str> {
        match self.cell(index)? {
            RemoteSqlValue::Text(text) => {
                self.text_scratch = text;
                Ok(&self.text_scratch)
            }
            RemoteSqlValue::Integer(value) => {
                self.text_scratch = value.to_string();
                Ok(&self.text_scratch)
            }
            RemoteSqlValue::Real(value) => {
                self.text_scratch = value.to_string();
                Ok(&self.text_scratch)
            }
            RemoteSqlValue::Null => Ok(""),
            other => bail!("column {index} is not text ({other:?})"),
        }
    }

    pub fn bind<T: Bind>(&self, value: &T, index: i32) -> Result<i32> {
        value.bind(self, index)
    }

    pub fn column<T: Column>(&mut self) -> Result<T> {
        Ok(T::column(self, 0)?.0)
    }

    pub fn column_type(&mut self, index: i32) -> Result<SqlType> {
        Ok(match self.cell(index)? {
            RemoteSqlValue::Null => SqlType::Null,
            RemoteSqlValue::Integer(_) => SqlType::Integer,
            RemoteSqlValue::Real(_) => SqlType::Float,
            RemoteSqlValue::Text(_) => SqlType::Text,
            RemoteSqlValue::Blob(_) => SqlType::Blob,
        })
    }

    pub fn with_bindings(&mut self, bindings: &impl Bind) -> Result<&mut Self> {
        self.bind(bindings, 1)?;
        Ok(self)
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
