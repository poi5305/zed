//! WASM-compatible wrapper around `async-tar`.
//!
//! On native targets this crate re-exports the git rev the desktop workspace
//! pins. On `wasm32` it exposes a stub whose I/O operations fail.

#[cfg(not(target_family = "wasm"))]
pub use async_tar_real::*;

#[cfg(target_family = "wasm")]
pub use stub::*;

#[cfg(target_family = "wasm")]
mod stub {
    use std::io;
    use std::marker::PhantomData;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use futures_core::Stream;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "async-tar is not supported on wasm32",
        ))
    }

    /// A top-level representation of an archive file.
    #[derive(Debug, Clone)]
    pub struct Archive<R>(PhantomData<R>);

    /// Configure the archive.
    pub struct ArchiveBuilder<R>(PhantomData<R>);

    /// A streaming iterator over the entries in an archive.
    pub struct Entries<R>(PhantomData<R>);

    /// A top-level representation of an archive builder.
    pub struct Builder<W>(PhantomData<W>);

    /// A read-only view of an entry in an archive.
    pub struct Entry<R> {
        _reader: PhantomData<R>,
        header: Header,
    }

    /// A file extracted from an archive.
    pub struct Unpacked;

    /// The type of a tar entry.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EntryType {
        Regular,
        Link,
        Symlink,
        Char,
        Block,
        Directory,
        Fifo,
        Continuous,
        GNULongName,
        GNULongLink,
        GNUSparse,
        XGlobalHeader,
        XHeader,
        Other(u8),
    }

    /// Representation of a header of a tar entry.
    pub struct Header;

    /// Mode indicating how to generate a header.
    #[derive(Debug, Clone, Copy)]
    pub enum HeaderMode {
        Complete,
        Deterministic,
    }

    pub struct GnuHeader;
    pub struct GnuSparseHeader;
    pub struct GnuExtSparseHeader;
    pub struct OldHeader;
    pub struct UstarHeader;

    pub struct PaxExtension<'a>(PhantomData<&'a ()>);
    pub struct PaxExtensions<'a>(PhantomData<&'a ()>);

    impl<R> Archive<R> {
        pub fn new(_obj: R) -> Archive<R> {
            Archive(PhantomData)
        }

        pub fn into_inner(self) -> Result<R, Self> {
            Err(self)
        }

        pub fn entries(self) -> io::Result<Entries<R>> {
            unsupported()
        }

        pub fn entries_raw(self) -> io::Result<Entries<R>> {
            unsupported()
        }

        pub async fn unpack<P: AsRef<Path>>(self, _dst: P) -> io::Result<()> {
            unsupported()
        }

        pub fn set_ignore_zeros(&mut self, _ignore: bool) {}
        pub fn set_unpack_xattrs(&mut self, _unpack: bool) {}
        pub fn set_preserve_permissions(&mut self, _preserve: bool) {}
        pub fn set_preserve_mtime(&mut self, _preserve: bool) {}
    }

    impl<R> ArchiveBuilder<R> {
        pub fn new(_obj: R) -> ArchiveBuilder<R> {
            ArchiveBuilder(PhantomData)
        }

        pub fn set_unpack_xattrs(self, _unpack_xattrs: bool) -> Self {
            self
        }

        pub fn set_preserve_permissions(self, _preserve: bool) -> Self {
            self
        }

        pub fn set_preserve_mtime(self, _preserve: bool) -> Self {
            self
        }

        pub fn set_ignore_zeros(self, _ignore_zeros: bool) -> Self {
            self
        }

        pub fn build(self) -> Archive<R> {
            Archive(PhantomData)
        }
    }

    impl<R> Stream for Entries<R> {
        type Item = io::Result<Entry<R>>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(Some(unsupported()))
        }
    }

    impl<R> Entry<R> {
        pub fn path(&self) -> io::Result<&Path> {
            unsupported()
        }

        pub fn path_bytes(&self) -> Vec<u8> {
            Vec::new()
        }

        pub fn header(&self) -> &Header {
            &self.header
        }

        pub async fn unpack<P: AsRef<Path>>(&mut self, _dst: P) -> io::Result<Unpacked> {
            unsupported()
        }

        pub async fn unpack_in<P: AsRef<Path>>(&mut self, _dst: P) -> io::Result<bool> {
            unsupported()
        }
    }

    impl Header {
        pub fn new_gnu() -> Self {
            Header
        }

        pub fn new_ustar() -> Self {
            Header
        }

        pub fn set_path<P: AsRef<Path>>(&mut self, _path: P) -> io::Result<()> {
            unsupported()
        }

        pub fn set_size(&mut self, _size: u64) {}

        pub fn set_entry_type(&mut self, _ty: EntryType) {}

        pub fn set_cksum(&mut self) {}

        pub fn set_mode(&mut self, _mode: u32) {}

        pub fn set_username(&mut self, _name: &str) -> io::Result<()> {
            unsupported()
        }

        pub fn set_groupname(&mut self, _name: &str) -> io::Result<()> {
            unsupported()
        }

        pub fn entry_type(&self) -> EntryType {
            EntryType::Regular
        }

        pub fn size(&self) -> io::Result<u64> {
            unsupported()
        }

        pub fn path(&self) -> io::Result<&Path> {
            unsupported()
        }
    }

    impl<W> Builder<W> {
        pub fn new(_obj: W) -> Builder<W> {
            Builder(PhantomData)
        }

        pub async fn append_data<P, R>(
            &mut self,
            _header: &mut Header,
            _path: P,
            _data: R,
        ) -> io::Result<()> {
            unsupported()
        }

        pub async fn append_path<P: AsRef<Path>>(&mut self, _path: P) -> io::Result<()> {
            unsupported()
        }

        pub async fn append_dir<P, Q>(&mut self, _path: P, _src: Q) -> io::Result<()> {
            unsupported()
        }

        pub async fn append_file<P: AsRef<Path>>(
            &mut self,
            _path: P,
            _file: &mut std::fs::File,
        ) -> io::Result<()> {
            unsupported()
        }

        pub async fn finish(&mut self) -> io::Result<()> {
            unsupported()
        }

        pub async fn into_inner(self) -> io::Result<W> {
            unsupported()
        }

        pub fn mode(&mut self, _mode: HeaderMode) -> &mut Self {
            self
        }

        pub fn follow_symlinks(&mut self, _follow: bool) -> &mut Self {
            self
        }
    }
}
