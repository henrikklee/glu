//! Streaming SHA-256 helpers shared by every artifact verification point so
//! they can't drift from each other.
//!
//! There are two intentional trust boundaries, both fail-closed:
//!   - ingest: `download/http.rs` hashes bytes as they stream off the network,
//!     deciding whether to accept a download into the cache;
//!   - consume: `bottle/prepare.rs` hashes bytes as the extract reads them back
//!     from the cache, deciding whether to install/commit.
//!
//! Both use `ring`'s hardware-accelerated SHA-256 and this module's lowercase
//! hex output, so a stream hashed at either point produces an identical,
//! comparable value.

use ring::digest;
use std::{
    fmt,
    io::{self, Read},
};

/// Lowercase hex of `bytes`.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// SHA-256 digest of `bytes`, returned in the canonical lowercase hex format.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(digest::digest(&digest::SHA256, bytes).as_ref())
}

/// A verified byte stream or file had a digest different from the manifest's
/// declared SHA-256. This is intentionally typed so cache self-healing keys off
/// the verification result, not a human-facing error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sha256Mismatch {
    pub(crate) subject: String,
    pub(crate) expected: String,
    pub(crate) actual: String,
}

impl Sha256Mismatch {
    pub(crate) fn new(
        subject: impl Into<String>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}

impl fmt::Display for Sha256Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sha256 mismatch for {}: expected {}, got {}",
            self.subject, self.expected, self.actual
        )
    }
}

impl std::error::Error for Sha256Mismatch {}

/// A `Read` wrapper that feeds every byte read through a SHA-256 context.
/// This lets verification be *fused* into a mandatory read (e.g. extraction)
/// — one pass, hardware-accelerated — instead of a separate full-file re-hash.
pub(crate) struct HashingReader<R: Read> {
    inner: R,
    context: digest::Context,
}

impl<R: Read> HashingReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            context: digest::Context::new(&digest::SHA256),
        }
    }

    /// Finalize and return the lowercase-hex SHA-256 of everything read so far.
    pub(crate) fn finish(self) -> String {
        hex_lower(self.context.finish().as_ref())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.context.update(&buf[..n]);
        Ok(n)
    }
}
