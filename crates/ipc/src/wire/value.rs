//! # `wire::value` — `WireValue` and `WireRecordField`
//!
//! `WireValue` is the rkyv-archivable mirror of
//! [`mizu_core::core::types::Value`].  It encodes every runtime value that
//! can cross the process boundary.
//!
//! ## Design notes
//!
//! ### Recursive variants
//!
//! `Value::List` and `Value::Record` are self-referential via `Arc<[T]>`.
//! rkyv 0.8 handles recursive types by requiring the `attr(recursive)` hint
//! on the archived type definition, which avoids the bound-overflow caused by
//! the default `Portable` requirement propagating into the recursive cycle.
//!
//! ### `FileHandle` → capability token
//!
//! `Value::FileHandle` carries a filesystem path.  The sandboxed worker
//! must **never** receive a raw path — it cannot open files anyway, and
//! sending paths would be an injection channel.  Instead:
//!
//! * The main process (broker) replaces the path with an opaque `u64` token
//!   before serializing for the worker.
//! * Only the display `filename` (the last path component) is sent across
//!   the boundary, to populate `Content-Disposition: filename=` in
//!   multipart uploads.
//! * The broker keeps a `HashMap<u64, PathBuf>` capability table and reads
//!   the file at upload time, after verifying the token.

#![forbid(unsafe_code)]

use rkyv::{Archive, Deserialize, Serialize};

/// Wire-format mirror of `mizu_core::core::types::Value`.
///
/// `Arc<str>` → `String` (rkyv serializes string data inline in the archive).
/// `Arc<[T]>` → `Vec<T>` (rkyv flattens the `Arc` indirection).
///
/// The `attr(recursive)` annotation on the archived enum is required because
/// `WireValue` is self-referential via `WireValue::List(Vec<WireValue>)` and
/// `WireValue::Record(Vec<WireRecordField>)`.  Without it, the `Archive` derive
/// macro generates bounds that cycle and overflow the trait solver.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
#[rkyv(serialize_bounds(
    __S: rkyv::ser::Writer + rkyv::ser::Allocator,
    __S::Error: rkyv::rancor::Source,
))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(
    __C: rkyv::validation::ArchiveContext,
    __C::Error: rkyv::rancor::Source,
)))]
pub enum WireValue {
    /// `Value::Null`
    Null,
    /// `Value::Bool`
    Bool(bool),
    /// `Value::Int`
    Int(i64),
    /// `Value::Decimal` — fixed-point, multiplied by `DECIMAL_SCALE` (10^8).
    Decimal(i64),
    /// `Value::String`
    Str(String),
    /// `Value::List` — elements may themselves be any `WireValue`.
    List(#[rkyv(omit_bounds)] Vec<WireValue>),
    /// `Value::Record`
    Record(#[rkyv(omit_bounds)] Vec<WireRecordField>),
    /// Replaces `Value::FileHandle` across the IPC boundary.
    ///
    /// The broker issues an opaque `u64` token when it hands a file handle
    /// to the worker (via a form field update).  The token is sent back
    /// unchanged in `NetworkCall`/`Multipart` payloads so the broker can
    /// resolve it to a `PathBuf` without trusting path strings from the
    /// sandbox.
    FileHandleToken {
        /// Opaque token issued by the broker.
        token: u64,
        /// User-visible original filename for `Content-Disposition: filename=`.
        /// Never interpreted as a filesystem path on either side.
        filename: String,
    },
}

/// Wire-format mirror of `mizu_core::core::types::RecordField`.
///
/// Carries the precomputed FNV-1a hash so the worker does not re-hash keys
/// when rebuilding the in-memory `Record` from an archived value.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
#[rkyv(serialize_bounds(
    __S: rkyv::ser::Writer + rkyv::ser::Allocator,
    __S::Error: rkyv::rancor::Source,
))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(
    __C: rkyv::validation::ArchiveContext,
    __C::Error: rkyv::rancor::Source,
)))]
pub struct WireRecordField {
    /// Field name.
    pub key: String,
    /// Precomputed [`mizu_core::core::types::hash_field`] of `key`.
    /// Must match the value stored in `RecordField::hash` on the other side.
    pub hash: u32,
    /// Field value.
    #[rkyv(omit_bounds)]
    pub value: WireValue,
}

// ── Conversions ─────────────────────────────────────────────────────────────

impl From<&mizu_core::core::types::Value> for WireValue {
    fn from(v: &mizu_core::core::types::Value) -> Self {
        use mizu_core::core::types::Value;
        match v {
            Value::Null => WireValue::Null,
            Value::Bool(b) => WireValue::Bool(*b),
            Value::Int(n) => WireValue::Int(*n),
            Value::Decimal(n) => WireValue::Decimal(*n),
            Value::String(s) => WireValue::Str(s.to_string()),
            Value::List(items) => {
                WireValue::List(items.iter().map(WireValue::from).collect())
            }
            Value::Record(fields) => {
                WireValue::Record(fields.iter().map(WireRecordField::from).collect())
            }
            Value::FileHandle(fh) => WireValue::FileHandleToken {
                // Paths are NEVER sent. The broker must replace this with a
                // real token before serializing for the worker. This From
                // impl is intentionally conservative: it produces token=0
                // and only the safe filename, ensuring the path stays in the
                // broker even if the caller forgets to substitute the token.
                token: 0,
                filename: fh.filename.clone(),
            },
        }
    }
}

impl From<&mizu_core::core::types::RecordField> for WireRecordField {
    fn from(f: &mizu_core::core::types::RecordField) -> Self {
        WireRecordField {
            key: f.key.to_string(),
            hash: f.hash,
            value: WireValue::from(&f.value),
        }
    }
}

impl From<WireValue> for mizu_core::core::types::Value {
    fn from(w: WireValue) -> Self {
        use mizu_core::core::types::Value;
        use std::sync::Arc;
        match w {
            WireValue::Null => Value::Null,
            WireValue::Bool(b) => Value::Bool(b),
            WireValue::Int(n) => Value::Int(n),
            WireValue::Decimal(n) => Value::Decimal(n),
            WireValue::Str(s) => Value::String(Arc::from(s.as_str())),
            WireValue::List(items) => {
                let owned: Vec<Value> = items.into_iter().map(Value::from).collect();
                Value::List(Arc::new(owned))
            }
            WireValue::Record(fields) => {
                let owned: Vec<mizu_core::core::types::RecordField> =
                    fields.into_iter().map(Into::into).collect();
                Value::Record(Arc::from(owned.as_slice()))
            }
            // A token arriving from the worker side cannot be converted back
            // into a real FileHandle — the broker is responsible for that
            // substitution using its capability table.
            WireValue::FileHandleToken { filename, .. } => {
                // Produce a sentinel FileHandle with an empty path.
                // Callers that care about the file must consult the broker's
                // token table instead of calling this conversion directly.
                Value::FileHandle(std::sync::Arc::new(
                    mizu_core::core::types::FileHandleData {
                        path: std::path::PathBuf::new(),
                        filename,
                    },
                ))
            }
        }
    }
}

impl From<WireRecordField> for mizu_core::core::types::RecordField {
    fn from(w: WireRecordField) -> Self {
        mizu_core::core::types::RecordField {
            key: std::sync::Arc::from(w.key.as_str()),
            hash: w.hash,
            value: mizu_core::core::types::Value::from(w.value),
        }
    }
}
