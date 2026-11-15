use log::{error, warn};
use std::borrow::Cow;
use std::cmp;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::TryInto;
#[cfg(not(feature = "async"))]
use std::fs::File;
#[cfg(not(feature = "async"))]
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(feature = "rayon")]
use rayon::prelude::*;
#[cfg(feature = "async")]
use tokio::fs::File;
#[cfg(feature = "async")]
use tokio::io::{AsyncRead, AsyncReadExt};
#[cfg(feature = "async")]
use tokio::pin;

use crate::common_data_structures;
use crate::encryption::{self, EncryptionState, PasswordAlgorithm};
use crate::error::{ParseError, XrefError};
use crate::load_options::{FilterFunc, LoadOptions};
use crate::object_stream::ObjectStream;
use crate::parser;
use crate::xref::{Xref, XrefEntry, XrefType};
use crate::{Dictionary, Document, Error, IncrementalDocument, Object, ObjectId, Result};

/// Open `path` and report its size, so the caller can pre-size the buffer it
/// reads the file into.
#[cfg(not(feature = "async"))]
fn open_file<P: AsRef<Path>>(path: P) -> Result<(File, usize)> {
    let file = File::open(path)?;
    let capacity = file.metadata()?.len() as usize;
    Ok((file, capacity))
}

#[cfg(not(feature = "async"))]
impl Document {
    /// Load a PDF document from a specified file path.
    #[inline]
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::default())
    }

    /// Load a PDF document from a specified file path with the given options.
    #[inline]
    pub fn load_with_options<P: AsRef<Path>>(path: P, options: LoadOptions) -> Result<Document> {
        let (file, capacity) = open_file(path)?;
        Self::load_internal(file, Some(capacity), options)
    }

    /// Load a PDF document from a specified file path with a password for encrypted PDFs.
    #[inline]
    pub fn load_with_password<P: AsRef<Path>>(path: P, password: &str) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::with_password(password))
    }

    #[deprecated(since = "0.41.0", note = "Use load_with_options instead")]
    #[inline]
    pub fn load_filtered<P: AsRef<Path>>(path: P, filter_func: FilterFunc) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::with_filter(filter_func))
    }

    /// Load a PDF document from an arbitrary source.
    #[inline]
    pub fn load_from<R: Read>(source: R) -> Result<Document> {
        Self::load_from_with_options(source, LoadOptions::default())
    }

    /// Load a PDF document from an arbitrary source with the given options.
    #[inline]
    pub fn load_from_with_options<R: Read>(source: R, options: LoadOptions) -> Result<Document> {
        Self::load_internal(source, None, options)
    }

    /// Load a PDF document from an arbitrary source with a password for encrypted PDFs.
    #[deprecated(since = "0.41.0", note = "Use load_from_with_options instead")]
    #[inline]
    pub fn load_from_with_password<R: Read>(source: R, password: &str) -> Result<Document> {
        Self::load_from_with_options(source, LoadOptions::with_password(password))
    }

    fn load_internal<R: Read>(
        mut source: R,
        capacity: Option<usize>,
        options: LoadOptions,
    ) -> Result<Document> {
        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer)?;

        let filter = options.filter;
        Reader::with_options(&buffer, options).read(filter)
    }

    /// Load a PDF document from a memory slice.
    pub fn load_mem(buffer: &[u8]) -> Result<Document> {
        Self::load_mem_with_options(buffer, LoadOptions::default())
    }

    /// Load a PDF document from a memory slice with the given options.
    pub fn load_mem_with_options(buffer: &[u8], options: LoadOptions) -> Result<Document> {
        let filter = options.filter;
        Reader::with_options(buffer, options).read(filter)
    }

    /// Load a PDF document from a memory slice with a password for encrypted PDFs.
    #[deprecated(since = "0.41.0", note = "Use load_mem_with_options instead")]
    pub fn load_mem_with_password(buffer: &[u8], password: &str) -> Result<Document> {
        Self::load_mem_with_options(buffer, LoadOptions::with_password(password))
    }

    /// Load PDF metadata (title and page count) without loading the entire document.
    /// This is much faster for large PDFs when you only need basic information.
    #[inline]
    pub fn load_metadata<P: AsRef<Path>>(path: P) -> Result<PdfMetadata> {
        let (file, capacity) = open_file(path)?;
        Self::load_metadata_internal(file, Some(capacity), None)
    }

    /// Load PDF metadata from a file path with a password for encrypted PDFs.
    #[inline]
    pub fn load_metadata_with_password<P: AsRef<Path>>(
        path: P,
        password: &str,
    ) -> Result<PdfMetadata> {
        let (file, capacity) = open_file(path)?;
        Self::load_metadata_internal(file, Some(capacity), Some(password.to_string()))
    }

    /// Load PDF metadata from an arbitrary source without loading the entire document.
    #[inline]
    pub fn load_metadata_from<R: Read>(source: R) -> Result<PdfMetadata> {
        Self::load_metadata_internal(source, None, None)
    }

    /// Load PDF metadata from an arbitrary source with a password for encrypted PDFs.
    #[inline]
    pub fn load_metadata_from_with_password<R: Read>(
        source: R,
        password: &str,
    ) -> Result<PdfMetadata> {
        Self::load_metadata_internal(source, None, Some(password.to_string()))
    }

    /// Load PDF metadata from a memory slice without loading the entire document.
    #[inline]
    pub fn load_metadata_mem(buffer: &[u8]) -> Result<PdfMetadata> {
        Reader::for_metadata(buffer, None).read_metadata()
    }

    /// Load PDF metadata from a memory slice with a password for encrypted PDFs.
    #[inline]
    pub fn load_metadata_mem_with_password(buffer: &[u8], password: &str) -> Result<PdfMetadata> {
        Reader::for_metadata(buffer, Some(password.to_string())).read_metadata()
    }

    fn load_metadata_internal<R: Read>(
        mut source: R,
        capacity: Option<usize>,
        password: Option<String>,
    ) -> Result<PdfMetadata> {
        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer)?;

        Reader::for_metadata(&buffer, password).read_metadata()
    }
}

/// Open `path` and report its size, so the caller can pre-size the buffer it
/// reads the file into.
#[cfg(feature = "async")]
async fn open_file<P: AsRef<Path>>(path: P) -> Result<(File, usize)> {
    let file = File::open(path).await?;
    let capacity = file.metadata().await?.len() as usize;
    Ok((file, capacity))
}

#[cfg(feature = "async")]
impl Document {
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::default()).await
    }

    /// Load a PDF document from a specified file path with the given options.
    pub async fn load_with_options<P: AsRef<Path>>(
        path: P,
        options: LoadOptions,
    ) -> Result<Document> {
        let (file, capacity) = open_file(path).await?;
        Self::load_internal(file, Some(capacity), options).await
    }

    /// Load a PDF document from a specified file path with a password for encrypted PDFs.
    pub async fn load_with_password<P: AsRef<Path>>(path: P, password: &str) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::with_password(password)).await
    }

    #[deprecated(since = "0.41.0", note = "Use load_with_options instead")]
    pub async fn load_filtered<P: AsRef<Path>>(
        path: P,
        filter_func: FilterFunc,
    ) -> Result<Document> {
        Self::load_with_options(path, LoadOptions::with_filter(filter_func)).await
    }

    async fn load_internal<R: AsyncRead>(
        source: R,
        capacity: Option<usize>,
        options: LoadOptions,
    ) -> Result<Document> {
        pin!(source);

        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer).await?;

        let filter = options.filter;
        Reader::with_options(&buffer, options).read(filter)
    }

    /// Load a PDF document from a memory slice.
    pub fn load_mem(buffer: &[u8]) -> Result<Document> {
        Self::load_mem_with_options(buffer, LoadOptions::default())
    }

    /// Load a PDF document from a memory slice with the given options.
    pub fn load_mem_with_options(buffer: &[u8], options: LoadOptions) -> Result<Document> {
        let filter = options.filter;
        Reader::with_options(buffer, options).read(filter)
    }

    /// Load PDF metadata (title and page count) without loading the entire document.
    /// This is much faster for large PDFs when you only need basic information.
    #[inline]
    pub async fn load_metadata<P: AsRef<Path>>(path: P) -> Result<PdfMetadata> {
        let (file, capacity) = open_file(path).await?;
        Self::load_metadata_internal(file, Some(capacity), None).await
    }

    /// Load PDF metadata from a file path with a password for encrypted PDFs.
    #[inline]
    pub async fn load_metadata_with_password<P: AsRef<Path>>(
        path: P,
        password: &str,
    ) -> Result<PdfMetadata> {
        let (file, capacity) = open_file(path).await?;
        Self::load_metadata_internal(file, Some(capacity), Some(password.to_string())).await
    }

    /// Load PDF metadata from an arbitrary source without loading the entire document.
    #[inline]
    pub async fn load_metadata_from<R: AsyncRead>(source: R) -> Result<PdfMetadata> {
        Self::load_metadata_internal(source, None, None).await
    }

    /// Load PDF metadata from an arbitrary source with a password for encrypted PDFs.
    #[inline]
    pub async fn load_metadata_from_with_password<R: AsyncRead>(
        source: R,
        password: &str,
    ) -> Result<PdfMetadata> {
        Self::load_metadata_internal(source, None, Some(password.to_string())).await
    }

    /// Load PDF metadata from a memory slice without loading the entire document.
    #[inline]
    pub fn load_metadata_mem(buffer: &[u8]) -> Result<PdfMetadata> {
        Reader::for_metadata(buffer, None).read_metadata()
    }

    /// Load PDF metadata from a memory slice with a password for encrypted PDFs.
    #[inline]
    pub fn load_metadata_mem_with_password(buffer: &[u8], password: &str) -> Result<PdfMetadata> {
        Reader::for_metadata(buffer, Some(password.to_string())).read_metadata()
    }

    async fn load_metadata_internal<R: AsyncRead>(
        source: R,
        capacity: Option<usize>,
        password: Option<String>,
    ) -> Result<PdfMetadata> {
        pin!(source);

        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer).await?;

        Reader::for_metadata(&buffer, password).read_metadata()
    }
}

impl TryInto<Document> for &[u8] {
    type Error = Error;

    fn try_into(self) -> Result<Document> {
        Reader::new(self).read(None)
    }
}

#[cfg(not(feature = "async"))]
impl IncrementalDocument {
    /// Load a PDF document from a specified file path.
    #[inline]
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let (file, capacity) = open_file(path)?;
        Self::load_internal(file, Some(capacity))
    }

    /// Load a PDF document from an arbitrary source.
    #[inline]
    pub fn load_from<R: Read>(source: R) -> Result<Self> {
        Self::load_internal(source, None)
    }

    fn load_internal<R: Read>(mut source: R, capacity: Option<usize>) -> Result<Self> {
        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer)?;

        let document = Reader::new(&buffer).read(None)?;

        Ok(IncrementalDocument::create_from(buffer, document))
    }

    /// Load a PDF document from a memory slice.
    pub fn load_mem(buffer: &[u8]) -> Result<Document> {
        buffer.try_into()
    }
}

#[cfg(feature = "async")]
impl IncrementalDocument {
    /// Load a PDF document from a specified file path.
    #[inline]
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let (file, capacity) = open_file(path).await?;
        Self::load_internal(file, Some(capacity)).await
    }

    /// Load a PDF document from an arbitrary source.
    #[inline]
    pub async fn load_from<R: AsyncRead>(source: R) -> Result<Self> {
        Self::load_internal(source, None).await
    }

    async fn load_internal<R: AsyncRead>(source: R, capacity: Option<usize>) -> Result<Self> {
        pin!(source);

        let mut buffer = capacity.map(Vec::with_capacity).unwrap_or_default();
        source.read_to_end(&mut buffer).await?;

        let document = Reader::new(&buffer).read(None)?;

        Ok(IncrementalDocument::create_from(buffer, document))
    }

    /// Load a PDF document from a memory slice.
    pub fn load_mem(buffer: &[u8]) -> Result<Document> {
        buffer.try_into()
    }
}

impl TryInto<IncrementalDocument> for &[u8] {
    type Error = Error;

    fn try_into(self) -> Result<IncrementalDocument> {
        let document = Reader::new(self).read(None)?;

        Ok(IncrementalDocument::create_from(self.to_vec(), document))
    }
}

/// The thread-safety a range source must provide.  Native loading parses
/// objects in parallel (rayon) and therefore needs `Send + Sync`; the wasm
/// core is single-threaded and must accept a browser callback, which is
/// neither.
#[cfg(not(target_arch = "wasm32"))]
pub trait RangeSourceBounds: std::fmt::Debug + Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: std::fmt::Debug + Send + Sync> RangeSourceBounds for T {}

#[cfg(target_arch = "wasm32")]
pub trait RangeSourceBounds: std::fmt::Debug {}
#[cfg(target_arch = "wasm32")]
impl<T: std::fmt::Debug> RangeSourceBounds for T {}

/// Synchronous random-access source for on-demand loading.  A browser
/// implements this over HTTP Range requests (a worker may use sync XHR); the
/// reader asks only for the header, xref, page tree and the objects a page
/// needs, in buffer-relative offsets plus a fixed zero-offset driver.
pub trait RangeSource: RangeSourceBounds {
    /// Total file length in bytes.
    fn len(&self) -> usize;
    /// Reads exactly `len` bytes at `offset`.
    fn read(&self, offset: usize, len: usize) -> std::result::Result<Vec<u8>, String>;
}

/// Bounded LRU chunk store for a range-backed reader.  The reader never
/// materialises the whole file: bytes fetched for the shell, an object or a
/// cross-reference section are kept here until the budget evicts them, and a
/// parse loads the window it needs into `Reader::buffer`.
#[derive(Debug)]
struct SparseStore {
    len: usize,
    /// Fetched, non-overlapping chunks keyed by buffer-relative start.
    chunks: BTreeMap<usize, Vec<u8>>,
    /// Insertion order for LRU eviction.
    age: VecDeque<usize>,
    bytes: usize,
    budget: usize,
}

/// Working-set limit for fetched range bytes.  This is deliberately small:
/// re-fetching an evicted object is cheap compared with holding a whole file,
/// and the parsed objects (display lists) are what a viewer actually reuses.
const SPARSE_STORE_BUDGET: usize = 16 * 1024 * 1024;

impl SparseStore {
    fn new(len: usize) -> Self {
        Self {
            len,
            chunks: BTreeMap::new(),
            age: VecDeque::new(),
            bytes: 0,
            budget: SPARSE_STORE_BUDGET,
        }
    }

    fn has(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return true;
        }
        let mut cursor = start;
        for (&chunk_start, bytes) in self.chunks.range(..end) {
            let chunk_end = chunk_start + bytes.len();
            if chunk_end <= cursor {
                continue;
            }
            if chunk_start > cursor {
                return false;
            }
            cursor = cursor.max(chunk_end);
            if cursor >= end {
                return true;
            }
        }
        false
    }

    fn missing(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut missing = Vec::new();
        let mut cursor = start;
        for (&chunk_start, bytes) in self.chunks.range(..end) {
            let chunk_end = chunk_start + bytes.len();
            if chunk_end <= cursor {
                continue;
            }
            if chunk_start >= end {
                break;
            }
            if chunk_start > cursor {
                missing.push((cursor, chunk_start.min(end)));
            }
            cursor = cursor.max(chunk_end);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            missing.push((cursor, end));
        }
        missing
    }

    fn insert(&mut self, start: usize, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.bytes += bytes.len();
        self.age.push_back(start);
        self.chunks.insert(start, bytes);
    }

    fn copy_range(&self, start: usize, end: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; end.checked_sub(start)?];
        let mut cursor = start;
        for (&chunk_start, bytes) in self.chunks.range(..end) {
            let chunk_end = chunk_start + bytes.len();
            if chunk_end <= cursor {
                continue;
            }
            if chunk_start > cursor {
                return None;
            }
            let from = cursor.max(chunk_start);
            let to = chunk_end.min(end);
            let src = from - chunk_start;
            out[from - start..to - start].copy_from_slice(&bytes[src..src + (to - from)]);
            cursor = to;
            if cursor >= end {
                return Some(out);
            }
        }
        None
    }

    fn trim(&mut self) {
        while self.bytes > self.budget {
            let Some(start) = self.age.pop_front() else {
                break;
            };
            if let Some(bytes) = self.chunks.remove(&start) {
                self.bytes = self.bytes.saturating_sub(bytes.len());
            }
        }
    }

    fn shift(&mut self, offset: usize) {
        let old = std::mem::take(&mut self.chunks);
        self.age.clear();
        self.bytes = 0;
        for (start, mut bytes) in old {
            if start + bytes.len() <= offset {
                continue;
            }
            let new_start = start.saturating_sub(offset);
            if start < offset {
                bytes.drain(..offset - start);
            }
            self.bytes += bytes.len();
            self.age.push_back(new_start);
            self.chunks.insert(new_start, bytes);
        }
        self.len = self.len.saturating_sub(offset);
        self.trim();
    }
}

/// Dictionaries smaller than this are cheaper to keep than to fingerprint;
/// the repeated per-form `/Resources` maps that motivate interning are much
/// larger.
const INTERN_MIN_ENTRIES: usize = 16;
/// Bound the intern table itself.  A miss after the cap still produces a
/// correct private dictionary.
const INTERN_MAX_DICTIONARIES: usize = 4096;

/// Shares structurally identical large dictionaries parsed during a load.
///
/// `Dictionary` is copy-on-write, so interning changes memory ownership, not
/// value semantics: a later `set`/`get_mut` on one object clones the shared
/// map before writing.  The table holds `Dictionary` handles (one `Arc`
/// each), not copies of the maps.
#[derive(Default)]
struct DictionaryInterner {
    entries: HashMap<u64, Vec<Dictionary>>,
    stored: usize,
}

impl DictionaryInterner {
    fn intern_object(&mut self, object: &mut Object) {
        self.intern_object_at(object, 0);
    }

    fn intern_object_at(&mut self, object: &mut Object, depth: u8) {
        if depth >= 16 {
            return;
        }
        match object {
            Object::Array(values) => {
                for value in values {
                    self.intern_object_at(value, depth + 1);
                }
            }
            Object::Dictionary(dict) => self.intern_dictionary_at(dict, depth + 1),
            Object::Stream(stream) => self.intern_dictionary_at(&mut stream.dict, depth + 1),
            _ => {}
        }
    }

    fn intern_dictionary_at(&mut self, dict: &mut Dictionary, depth: u8) {
        // Recurse first so nested repeated maps (an `/ExtGState` map inside
        // `/Resources`) are replaced before the parent is fingerprinted.
        for (_, value) in dict.iter_mut() {
            self.intern_object_at(value, depth + 1);
        }
        if dict.len() < INTERN_MIN_ENTRIES || self.stored >= INTERN_MAX_DICTIONARIES {
            return;
        }
        let fingerprint = dictionary_fingerprint(dict);
        let cached = self
            .entries
            .get(&fingerprint)
            .and_then(|candidates| candidates.iter().find(|known| *known == dict))
            .cloned();
        if let Some(cached) = cached {
            *dict = cached;
            return;
        }
        self.entries
            .entry(fingerprint)
            .or_default()
            .push(dict.clone());
        self.stored += 1;
    }
}

fn dictionary_fingerprint(dict: &Dictionary) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_dictionary(dict, &mut hasher, 0);
    hasher.finish()
}

fn hash_dictionary<H: std::hash::Hasher>(dict: &Dictionary, hasher: &mut H, depth: u8) {
    use std::hash::Hash;
    dict.len().hash(hasher);
    for (key, value) in dict {
        key.hash(hasher);
        hash_object(value, hasher, depth);
    }
}

fn hash_object<H: std::hash::Hasher>(object: &Object, hasher: &mut H, depth: u8) {
    use std::hash::Hash;
    if depth >= 16 {
        0xFFu8.hash(hasher);
        return;
    }
    match object {
        Object::Null => 0u8.hash(hasher),
        Object::Boolean(value) => {
            1u8.hash(hasher);
            value.hash(hasher);
        }
        Object::Integer(value) => {
            2u8.hash(hasher);
            value.hash(hasher);
        }
        Object::Real(value) => {
            3u8.hash(hasher);
            value.to_bits().hash(hasher);
        }
        Object::Name(name) => {
            4u8.hash(hasher);
            name.as_slice().hash(hasher);
        }
        Object::String(name, _) => {
            4u8.hash(hasher);
            name.as_slice().hash(hasher);
        }
        Object::Array(values) => {
            5u8.hash(hasher);
            values.len().hash(hasher);
            for value in values {
                hash_object(value, hasher, depth + 1);
            }
        }
        Object::Dictionary(dict) => {
            6u8.hash(hasher);
            hash_dictionary(dict, hasher, depth + 1);
        }
        Object::Stream(stream) => {
            7u8.hash(hasher);
            hash_dictionary(&stream.dict, hasher, depth + 1);
            stream.content.len().hash(hasher);
        }
        Object::Reference((id, generation)) => {
            8u8.hash(hasher);
            id.hash(hasher);
            generation.hash(hasher);
        }
    }
}

pub struct Reader<'a> {
    /// The PDF bytes.  `Borrowed` is the normal loader; `Owned` lets a
    /// long-lived reader keep the file without a self-referential borrow
    /// (shell loading, on-demand objects).  With a range source this is the
    /// current parse window, not the file: `sparse` holds fetched chunks.
    pub buffer: Cow<'a, [u8]>,
    pub document: Document,
    pub encryption_state: Option<EncryptionState>,
    /// Byte ranges `(id, start, end)` of encrypted direct objects.  The bytes
    /// stay in `buffer`; copying each object out would duplicate the whole
    /// file and allocate once per object before decryption even starts.
    pub raw_objects: Vec<(ObjectId, usize, usize)>,
    pub password: Option<String>,                 // Password for encrypted PDFs
    pub strict: bool,                             // Reject non-conforming PDFs when true
    /// Maximum decompressed size, in bytes, allowed for any single stream
    /// (object streams and cross-reference streams) decoded during loading.
    /// `None` (the default) applies no limit; set it to guard against
    /// decompression bombs. See [`crate::LoadOptions`].
    pub max_decompressed_size: Option<usize>,
    /// Intern large structurally identical dictionaries while loading.
    intern_dictionaries: bool,
    /// Persistent intern table for lazily fetched objects; the same table
    /// serves every `get_object` call on a shell reader.
    dictionary_interner: Mutex<DictionaryInterner>,
    /// Sorted unique byte offsets of every `XrefEntry::Normal` entry in the
    /// final cross-reference table. Built once when the table is complete so
    /// object-boundary lookups can binary-search the successor offset instead
    /// of scanning all xref entries on each call.
    normal_offsets: Vec<usize>,
    /// Object streams decoded while resolving compressed objects, keyed by
    /// container number; `None` records one that failed to decode. Without it
    /// every indirect `/Length` stored in an object stream re-reads and
    /// re-decodes the whole container, which is quadratic in the number of
    /// such streams.
    object_streams: Mutex<HashMap<u32, Option<Arc<ObjectStream>>>>,
    /// On-demand byte source (browser Range reader); `None` means the buffer
    /// is complete.
    range_source: Option<Box<dyn RangeSource>>,
    /// Buffer-relative intervals that `range_source` has supplied in *full*
    /// mode (after `ensure_all`); sparse mode uses `sparse` instead.
    filled: Vec<(usize, usize)>,
    /// File offset of buffer-relative offset 0 (junk before `%PDF-`).
    buffer_base: usize,
    /// Chunked backing for a range reader; `None` means `buffer` is the file.
    sparse: Option<SparseStore>,
    /// Buffer-relative offset of `buffer[0]` in sparse mode.
    window_base: usize,
    /// Small LRU of objects parsed only to resolve an indirect `/Length`
    /// while another object is being parsed.  Bounded so it cannot grow with
    /// the document.
    range_objects: Mutex<VecDeque<(ObjectId, Object)>>,
}

/// How many indirect `/Length` objects stay resolved for the parser's
/// immutable lookups.  256 integers is negligible; the bound keeps a hostile
/// file from using this cache as unbounded allocation.
const RANGE_OBJECT_CACHE: usize = 256;

/// Maximum allowed embedding of literal strings.
pub const MAX_BRACKET: usize = 100;

pub const MAX_NESTING_DEPTH: usize = 100;

/// Cap on reconstructed cross-reference entries, bounding memory on hostile inputs.
const MAX_RECONSTRUCTED_OBJECTS: usize = 1_000_000;

/// Cap on how many trailing `trailer` dictionaries are inspected for a `/Root`.
const MAX_TRAILER_CANDIDATES: usize = 16;

/// PDF metadata extracted without loading the entire document.
/// This is useful for quickly getting basic information about large PDFs.
#[derive(Debug, Clone)]
pub struct PdfMetadata {
    /// Document title from Info dictionary
    pub title: Option<String>,
    /// Document author from Info dictionary
    pub author: Option<String>,
    /// Document subject from Info dictionary
    pub subject: Option<String>,
    /// Document keywords from Info dictionary
    pub keywords: Option<String>,
    /// Application that created the document
    pub creator: Option<String>,
    /// Application that produced the document
    pub producer: Option<String>,
    /// Document creation date (PDF date format: D:YYYYMMDDHHmmSSOHH'mm')
    pub creation_date: Option<String>,
    /// Document modification date (PDF date format: D:YYYYMMDDHHmmSSOHH'mm')
    pub modification_date: Option<String>,
    /// Custom Info dictionary entries that are not part of the standard set above.
    ///
    /// Producers commonly use the Info dictionary to attach application-specific
    /// metadata (for example, Microsoft Information Protection labels stored as
    /// `MSIP_Label_{GUID}_{Property}` keys). Such entries used to be discarded
    /// by the metadata-only loader; they are now preserved here as raw `Object`
    /// values so callers can inspect or decode them as needed.
    pub custom: HashMap<Vec<u8>, Object>,
    /// Number of pages in the document
    pub page_count: u32,
    /// PDF version
    pub version: String,
    /// Whether the document declares encryption (an `/Encrypt` entry in the
    /// trailer). When `true`, string and stream contents are encrypted; the
    /// standard Info fields above are populated only if the document could be
    /// decrypted — e.g. an empty user password, or a password supplied via a
    /// `*_with_password` loader.
    pub encrypted: bool,
}

struct InfoMetadata {
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    keywords: Option<String>,
    creator: Option<String>,
    producer: Option<String>,
    creation_date: Option<String>,
    modification_date: Option<String>,
    custom: HashMap<Vec<u8>, Object>,
}

impl InfoMetadata {
    fn empty() -> Self {
        Self {
            title: None,
            author: None,
            subject: None,
            keywords: None,
            creator: None,
            producer: None,
            creation_date: None,
            modification_date: None,
            custom: HashMap::new(),
        }
    }
}

/// Standard Info dictionary keys defined in PDF 1.7 §14.3.3 ("Document
/// Information Dictionary"). Any other key is preserved on
/// `PdfMetadata::custom` rather than dropped.
const STANDARD_INFO_KEYS: &[&[u8]] = &[
    b"Title",
    b"Author",
    b"Subject",
    b"Keywords",
    b"Creator",
    b"Producer",
    b"CreationDate",
    b"ModDate",
    b"Trapped",
];

impl<'a> Reader<'a> {
    /// A reader with the default load options: no password, lenient, unlimited.
    ///
    /// This deliberately defers to `LoadOptions::default()`, so a field added
    /// there later is picked up here rather than silently reset to a stale
    /// value the way an explicit struct literal would.
    fn new(buffer: &'a [u8]) -> Self {
        Self::with_options(buffer, LoadOptions::default())
    }

    /// The metadata-only loaders accept a password but never the strictness,
    /// filter or decompression-limit knobs, so they use their own defaults.
    fn for_metadata(buffer: &'a [u8], password: Option<String>) -> Self {
        Self {
            password,
            ..Self::new(buffer)
        }
    }

    fn with_options(buffer: &'a [u8], options: LoadOptions) -> Self {
        let mut reader = Self {
            buffer: Cow::Borrowed(buffer),
            document: Document::new(),
            encryption_state: None,
            raw_objects: Vec::new(),
            password: options.password,
            strict: options.strict,
            max_decompressed_size: options.max_decompressed_size,
            intern_dictionaries: options.intern_dictionaries,
            dictionary_interner: Mutex::default(),
            normal_offsets: Vec::new(),
            object_streams: Mutex::default(),
            range_source: None,
            filled: Vec::new(),
            buffer_base: 0,
            sparse: None,
            window_base: 0,
            range_objects: Mutex::new(VecDeque::new()),
        };
        reader.filled.push((0, reader.buffer.len()));
        reader
    }

    /// Drops `offset` bytes from the front of the buffer (junk before
    /// `%PDF-`).  Borrowed slices stay zero-copy; owned buffers drain in place.
    fn strip_prefix_bytes(&mut self, offset: usize) {
        if offset == 0 {
            return;
        }
        if let Some(store) = &mut self.sparse {
            store.shift(offset);
            self.buffer.to_mut().clear();
            self.window_base = 0;
            self.buffer_base += offset;
            return;
        }
        let buffer = std::mem::replace(&mut self.buffer, Cow::Borrowed(&[]));
        self.buffer = match buffer {
            Cow::Borrowed(bytes) => Cow::Borrowed(&bytes[offset..]),
            Cow::Owned(mut bytes) => {
                bytes.drain(..offset);
                Cow::Owned(bytes)
            }
        };
        self.buffer_base += offset;
        self.filled = self
            .filled
            .iter()
            .filter_map(|(start, end)| {
                let start = start.saturating_sub(offset);
                let end = end.saturating_sub(offset);
                (start < end).then_some((start, end))
            })
            .collect();
    }

    /// A reader that fetches bytes on demand.  It never allocates a
    /// file-sized buffer: `sparse` keeps a bounded chunk cache and each parse
    /// loads only the window it needs.
    pub fn with_range_source(
        source: Box<dyn RangeSource>,
        options: LoadOptions,
    ) -> Reader<'static> {
        let length = source.len();
        let mut reader = Reader::with_options(&[], options);
        reader.buffer = Cow::Owned(Vec::new());
        reader.filled.clear();
        reader.sparse = Some(SparseStore::new(length));
        reader.range_source = Some(source);
        reader
    }

    /// Logical file length after junk-prefix stripping (the sparse window is
    /// usually much shorter than this).
    fn logical_len(&self) -> usize {
        self.sparse
            .as_ref()
            .map_or(self.buffer.len(), |store| store.len)
    }

    /// Fetches the missing parts of `[start, end)` from the range source.
    fn ensure(&mut self, start: usize, end: usize) -> Result<()> {
        let end = end.min(self.logical_len());
        if self.range_source.is_none() || start >= end {
            return Ok(());
        }
        if self.sparse.is_some() {
            let missing = self
                .sparse
                .as_ref()
                .expect("checked above")
                .missing(start, end);
            for (start, end) in missing {
                let bytes = self
                    .range_source
                    .as_ref()
                    .expect("checked above")
                    .read(self.buffer_base + start, end - start)
                    .map_err(Error::RangeSource)?;
                if bytes.len() != end - start {
                    return Err(Error::RangeSource(format!(
                        "source returned {} bytes for {}..{}",
                        bytes.len(),
                        start,
                        end
                    )));
                }
                self.sparse
                    .as_mut()
                    .expect("checked above")
                    .insert(start, bytes);
            }
            return Ok(());
        }
        let mut missing: Vec<(usize, usize)> = Vec::new();
        let mut cursor = start;
        for (filled_start, filled_end) in &self.filled {
            if *filled_end <= cursor {
                continue;
            }
            if *filled_start >= end {
                break;
            }
            if *filled_start > cursor {
                missing.push((cursor, (*filled_start).min(end)));
            }
            cursor = cursor.max(*filled_end);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            missing.push((cursor, end));
        }
        for (start, end) in missing {
            let bytes = self
                .range_source
                .as_ref()
                .expect("checked above")
                .read(self.buffer_base + start, end - start)
                .map_err(Error::RangeSource)?;
            if bytes.len() != end - start {
                return Err(Error::RangeSource(format!(
                    "source returned {} bytes for {}..{}",
                    bytes.len(),
                    start,
                    end
                )));
            }
            self.buffer.to_mut()[start..end].copy_from_slice(&bytes);
            self.filled.push((start, end));
            self.filled.sort_unstable();
            self.filled =
                self.filled
                    .iter()
                    .copied()
                    .fold(Vec::new(), |mut merged, (start, end)| {
                        match merged.last_mut() {
                            Some((_, last_end)) if start <= *last_end => {
                                *last_end = (*last_end).max(end);
                            }
                            _ => merged.push((start, end)),
                        }
                        merged
                    });
        }
        Ok(())
    }

    /// Whether `[start, end)` has already been supplied by the range source
    /// (or the whole buffer is present because there is no source).
    fn is_filled(&self, start: usize, end: usize) -> bool {
        if let Some(store) = &self.sparse {
            return store.has(start, end);
        }
        self.range_source.is_none()
            || self
                .filled
                .iter()
                .any(|&(filled_start, filled_end)| filled_start <= start && end <= filled_end)
    }

    /// Fetches the whole file.  Used by eager loaders, metadata extraction and
    /// by recovery paths that need to scan every byte.  A sparse reader
    /// switches to the ordinary full-buffer mode here; range shell loading
    /// never calls it except for cross-reference reconstruction.
    fn ensure_all(&mut self) -> Result<()> {
        if let Some(store) = &self.sparse {
            let len = store.len;
            self.ensure(0, len)?;
            let store = self.sparse.take().expect("checked above");
            let mut buffer = vec![0u8; len];
            for (start, bytes) in &store.chunks {
                let end = start + bytes.len();
                if end <= len {
                    buffer[*start..end].copy_from_slice(bytes);
                }
            }
            self.buffer = Cow::Owned(buffer);
            self.window_base = 0;
            self.filled.clear();
            self.filled.push((0, len));
            return Ok(());
        }
        let len = self.buffer.len();
        self.ensure(0, len)
    }

    /// Loads `[start, end)` into the contiguous parse window.  In full mode
    /// this only ensures the bytes exist; in sparse mode it copies them out of
    /// the chunk cache and then trims that cache back to its budget.
    fn load_window(&mut self, start: usize, end: usize) -> Result<()> {
        let end = end.min(self.logical_len());
        let start = start.min(end);
        if self.sparse.is_none() {
            return self.ensure(start, end);
        }
        self.ensure(start, end)?;
        let bytes = self
            .sparse
            .as_ref()
            .expect("checked above")
            .copy_range(start, end)
            .ok_or_else(|| {
                Error::RangeSource(format!("range window {start}..{end} is incomplete"))
            })?;
        if let Some(store) = &mut self.sparse {
            store.trim();
        }
        self.buffer = Cow::Owned(bytes);
        self.window_base = start;
        Ok(())
    }

    /// Makes sure the bytes of one indirect object are present before a
    /// resolution path parses it.  The extent runs to the next cross-reference
    /// offset (or EOF) plus the recovery window, so header recovery and
    /// unterminated streams see the same neighbour bytes an eager load does.
    fn ensure_object(&mut self, id: ObjectId) -> Result<()> {
        self.ensure_object_inner(id, 0)
    }

    fn ensure_object_inner(&mut self, id: ObjectId, depth: usize) -> Result<()> {
        // Sparse readers fetch and parse a single object window in
        // `get_object_sparse`; there is no full buffer to fill.
        if self.range_source.is_none() || self.sparse.is_some() || depth > 8 {
            return Ok(());
        }
        let Some(entry) = self.document.reference_table.get(id.0).cloned() else {
            return Ok(());
        };
        match entry {
            XrefEntry::Normal { offset, .. } => {
                let offset = offset as usize;
                if offset >= self.logical_len() {
                    // Let the parser report the bad offset; there is no
                    // object extent to fetch.
                    return Ok(());
                }
                let next = self
                    .normal_offsets
                    .get(self.normal_offsets.partition_point(|&next| next <= offset))
                    .copied();
                let object_end = self.object_end(offset, next);
                let start = offset.saturating_sub(4096);
                let end = object_end.saturating_add(4096).min(self.logical_len());
                self.ensure(start, end)?;
                // The parser may need an indirect `/Length` while decoding the
                // stream dictionary.  It is a reference written inside the
                // extent just fetched; resolving it here (rather than inside
                // the parser, which has no `&mut self`) keeps streams intact.
                let refs = {
                    let bytes = &self.buffer[offset..object_end.min(self.buffer.len())];
                    length_references(bytes)
                };
                for (number, generation) in refs {
                    self.ensure_object_inner((number, generation), depth + 1)?;
                }
            }
            XrefEntry::Compressed { container, .. } => {
                // The member's bytes live in the container object.
                self.ensure_object_inner((container, 0), depth + 1)?;
            }
            XrefEntry::Free { .. } | XrefEntry::UnusableFree => {}
        }
        Ok(())
    }

    /// Ensures an entire cross-reference section (and the bytes around it that
    /// offset correction needs) is present.  A section that does not parse in
    /// the initial window is retried with a larger window, up to EOF.
    fn ensure_xref_at(&mut self, offset: usize) -> Result<()> {
        if self.range_source.is_none() {
            return Ok(());
        }
        let len = self.logical_len();
        let mut window = 1usize << 20;
        loop {
            let start = offset.saturating_sub(64);
            let end = offset.saturating_add(window).min(len);
            if self.sparse.is_some() {
                self.load_window(start, end)?;
            } else {
                self.ensure(start, end)?;
                // Cross-reference streams may name an indirect `/Length`; the
                // parser cannot fetch it itself.
                let refs = {
                    let bytes = &self.buffer[start..end];
                    length_references(bytes)
                };
                for (number, generation) in refs {
                    self.ensure_object_inner((number, generation), 0)?;
                }
            }
            match self.xref_and_trailer_at(offset) {
                Ok(_) => return Ok(()),
                Err(_) if end < len => window = window.saturating_mul(4),
                Err(err) => return Err(err),
            }
        }
    }

    /// Whether the buffer holds only fetched chunks rather than the whole
    /// file.  Such a reader is read-only: serialising its window would write
    /// holes.
    pub fn is_sparse(&self) -> bool {
        self.sparse.is_some()
    }

    /// `(logical file bytes, fetched bytes still cached, parse-window bytes)`.
    /// The fetched value is the sparse chunk cache's current working set, not
    /// a lifetime total (evicted chunks are refetched on demand).
    pub fn range_stats(&self) -> (usize, usize, usize) {
        let fetched = self
            .sparse
            .as_ref()
            .map_or(self.buffer.len(), |store| store.bytes);
        (self.logical_len(), fetched, self.buffer.len())
    }

    /// A reader that owns its bytes (`Reader<'static>`), for shell loading
    /// and on-demand object resolution.
    pub fn owned_with_options(buffer: Vec<u8>, options: LoadOptions) -> Reader<'static> {
        let mut reader = Reader::with_options(&[], options);
        reader.buffer = Cow::Owned(buffer);
        reader
    }

    /// Resolves the header, cross-reference table and trailer, then loads only
    /// the catalog, page-tree nodes and page dictionaries.  Other objects stay
    /// on demand through [`Reader::get_object`].
    ///
    /// This is the shell a browser uses to know the page list without pulling
    /// every content stream and image into memory.  Encrypted documents are
    /// rejected here; callers fall back to the eager loader.
    pub fn read_shell(mut self) -> Result<Self> {
        let head = self.logical_len().min(1024);
        self.load_window(0, head)?;
        let offset = self
            .buffer
            .windows(5)
            .position(|window| window == b"%PDF-")
            .unwrap_or(0);
        self.strip_prefix_bytes(offset);
        let head = self.logical_len().min(1024);
        self.load_window(0, head)?;
        let version =
            parser::header(&self.buffer, self.strict).ok_or(ParseError::InvalidFileHeader)?;
        let (xref, trailer) = match self.resolve_xref_and_trailer() {
            Ok(resolved) => resolved,
            Err(err) => {
                // Reconstruction scans every byte; only fall back after the
                // whole file is present.
                self.ensure_all()?;
                match self.reconstruct_xref_and_trailer() {
                    Some(reconstructed) => reconstructed,
                    None => return Err(err),
                }
            }
        };
        let encrypted = trailer.get(b"Encrypt").is_ok();
        self.document.version = version;
        self.document.max_id = xref.size.saturating_sub(1);
        self.document.trailer = trailer;
        self.set_reference_table(xref);

        if encrypted {
            // Authenticate before loading the catalog/page tree; every later
            // `get_object` then decrypts direct objects and object-stream
            // containers on demand.
            self.parse_encryption_dictionary()?;
            if self.authenticate_and_setup_encryption(false)?.is_none() {
                return Err(Error::PasswordRequired);
            }
            let encrypt_ref = self
                .document
                .trailer
                .get(b"Encrypt")
                .and_then(Object::as_reference)
                .ok();
            if let Some(mut state) = self.encryption_state.clone() {
                state.encrypt_object_id = encrypt_ref;
                self.document.encryption_state = Some(state);
            }
        }

        let root = self
            .document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| Error::Parse(ParseError::InvalidXref))?;
        let catalog = self.get_object(root, &mut HashSet::new())?;
        let pages = catalog
            .as_dict()
            .ok()
            .and_then(|dict| dict.get(b"Pages").ok())
            .and_then(|pages| pages.as_reference().ok());
        self.document.objects.insert(root, catalog);
        let Some(pages) = pages else {
            return Err(Error::Parse(ParseError::InvalidXref));
        };
        self.load_page_tree_shell(pages)?;

        // `/Info` is small and every metadata call needs it.
        if let Ok(info) = self
            .document
            .trailer
            .get(b"Info")
            .and_then(Object::as_reference)
        {
            if !self.document.objects.contains_key(&info)
                && let Ok(object) = self.get_object(info, &mut HashSet::new())
            {
                self.document.objects.insert(info, object);
            }
        }
        Ok(self)
    }

    /// Loads the page-tree node and, recursively, its page dictionaries.
    fn load_page_tree_shell(&mut self, root: ObjectId) -> Result<()> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if self.document.objects.contains_key(&id) {
                continue;
            }
            let object = self.get_object(id, &mut HashSet::new())?;
            let mut kids = Vec::new();
            if let Object::Dictionary(dict) = &object
                && let Ok(array) = dict
                    .get_deref(b"Kids", &self.document)
                    .and_then(Object::as_array)
            {
                for kid in array {
                    if let Object::Reference(kid) = kid {
                        kids.push(*kid);
                    }
                }
            }
            self.document.objects.insert(id, object);
            stack.extend(kids);
        }
        Ok(())
    }

    /// Read metadata (title and page count) without loading the entire document.
    /// This is much faster for large PDFs when you only need basic information.
    ///
    /// For encrypted PDFs, use `Document::load_metadata_with_password()` instead.
    pub fn read_metadata(mut self) -> Result<PdfMetadata> {
        // Metadata is a whole-document operation: Info, the catalog and the
        // page tree may sit anywhere, so a range source is fetched completely
        // before the existing eager path runs.
        self.ensure_all()?;
        let offset = self
            .buffer
            .windows(5)
            .position(|w| w == b"%PDF-")
            .unwrap_or(0);
        self.strip_prefix_bytes(offset);

        let version =
            parser::header(&self.buffer, self.strict).ok_or(ParseError::InvalidFileHeader)?;

        let (xref, trailer) = match self.resolve_xref_and_trailer() {
            Ok(resolved) => resolved,
            Err(err) => match self.reconstruct_xref_and_trailer() {
                Some(reconstructed) => {
                    warn!(
                        "cross-reference resolution failed ({err}); recovered by scanning for indirect objects"
                    );
                    reconstructed
                }
                None => return Err(err),
            },
        };

        self.set_reference_table(xref);
        self.document.trailer = trailer.clone();
        let encrypted = self.document.trailer.get(b"Encrypt").is_ok();
        // For encrypted PDFs, set up decryption so the Info dictionary can be read. Without a
        // usable password, report what we have rather than failing the whole call; callers
        // needing the Info fields should use a `*_with_password` loader.
        let needs_password = if encrypted {
            match self.setup_encryption_for_metadata() {
                Ok(()) => false,
                // No usable password: report `encrypted` without the Info fields.
                Err(Error::Unimplemented(_)) => true,
                Err(e) => return Err(e),
            }
        } else {
            false
        };

        let info_metadata = if needs_password {
            InfoMetadata::empty()
        } else {
            self.extract_info_metadata()?
        };
        let page_count = if needs_password {
            0
        } else {
            self.extract_page_count()?
        };

        Ok(PdfMetadata {
            title: info_metadata.title,
            author: info_metadata.author,
            subject: info_metadata.subject,
            keywords: info_metadata.keywords,
            creator: info_metadata.creator,
            producer: info_metadata.producer,
            creation_date: info_metadata.creation_date,
            modification_date: info_metadata.modification_date,
            custom: info_metadata.custom,
            page_count,
            version,
            encrypted,
        })
    }

    fn extract_info_metadata(&self) -> Result<InfoMetadata> {
        let info_ref = match self.document.trailer.get(b"Info") {
            Ok(obj) => obj.as_reference().ok(),
            Err(_) => return Ok(InfoMetadata::empty()),
        };

        let info_id = match info_ref {
            Some(id) => id,
            None => return Ok(InfoMetadata::empty()),
        };

        let mut already_seen = HashSet::new();
        let info_obj = match self.get_object_cached(info_id, &mut already_seen) {
            Ok(obj) => obj,
            Err(_) => return Ok(InfoMetadata::empty()),
        };

        let info_dict = match info_obj.as_dict() {
            Ok(dict) => dict,
            Err(_) => return Ok(InfoMetadata::empty()),
        };

        let mut custom = HashMap::new();
        for (key, value) in info_dict.iter() {
            if STANDARD_INFO_KEYS.contains(&key.as_slice()) {
                continue;
            }
            custom.insert(key.to_vec(), value.clone());
        }

        Ok(InfoMetadata {
            title: Self::extract_string_field(info_dict, b"Title"),
            author: Self::extract_string_field(info_dict, b"Author"),
            subject: Self::extract_string_field(info_dict, b"Subject"),
            keywords: Self::extract_string_field(info_dict, b"Keywords"),
            creator: Self::extract_string_field(info_dict, b"Creator"),
            producer: Self::extract_string_field(info_dict, b"Producer"),
            creation_date: Self::extract_string_field(info_dict, b"CreationDate"),
            modification_date: Self::extract_string_field(info_dict, b"ModDate"),
            custom,
        })
    }

    fn extract_string_field(dict: &Dictionary, key: &[u8]) -> Option<String> {
        match dict.get(key) {
            Ok(obj) => match obj {
                Object::String(_bytes, _) => common_data_structures::decode_text_string(obj).ok(),
                _ => None,
            },
            Err(_) => None,
        }
    }

    fn extract_page_count(&self) -> Result<u32> {
        let root_ref = match self
            .document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
        {
            Ok(id) => id,
            Err(_) => return Ok(0),
        };

        let mut already_seen = HashSet::new();
        let catalog_obj = match self.get_object_cached(root_ref, &mut already_seen) {
            Ok(obj) => obj,
            Err(_) => return Ok(0),
        };

        let catalog_dict = match catalog_obj.as_dict() {
            Ok(dict) => dict,
            Err(_) => return Ok(0),
        };

        let pages_ref = match catalog_dict.get(b"Pages").and_then(Object::as_reference) {
            Ok(id) => id,
            Err(_) => return Ok(0),
        };

        self.get_pages_tree_count(pages_ref, &mut HashSet::new(), 0)
            .or(Ok(0))
    }

    fn get_pages_tree_count(
        &self,
        pages_id: ObjectId,
        seen: &mut HashSet<ObjectId>,
        depth: usize,
    ) -> Result<u32> {
        // `seen` catches cycles but not a long non-cyclic /Pages chain with no /Count,
        // which recurses once per level.
        if depth >= MAX_NESTING_DEPTH {
            return Ok(0);
        }
        if seen.contains(&pages_id) {
            return Err(Error::ReferenceCycle(pages_id));
        }
        seen.insert(pages_id);

        let mut already_seen = HashSet::new();
        let pages_obj = match self.get_object_cached(pages_id, &mut already_seen) {
            Ok(obj) => obj,
            Err(_) => return Ok(0),
        };

        let pages_dict = match pages_obj.as_dict() {
            Ok(dict) => dict,
            Err(_) => return Ok(0),
        };

        match pages_dict.get_type() {
            Ok(type_name) if type_name == b"Page" => Ok(1),
            Ok(type_name) if type_name == b"Pages" => {
                if let Ok(count_obj) = pages_dict.get(b"Count")
                    && let Ok(count) = count_obj.as_i64()
                    && count >= 0
                {
                    return Ok(count as u32);
                }

                let kids = match pages_dict.get(b"Kids").and_then(Object::as_array) {
                    Ok(arr) => arr,
                    Err(_) => return Ok(0),
                };

                let mut total = 0u32;
                for kid in kids.iter() {
                    if let Ok(kid_ref) = kid.as_reference()
                        && let Ok(count) = self.get_pages_tree_count(kid_ref, seen, depth + 1)
                    {
                        total += count;
                    }
                }
                Ok(total)
            }
            _ => Ok(1),
        }
    }

    /// Read whole document.
    pub fn read(mut self, filter_func: Option<FilterFunc>) -> Result<Document> {
        // Eager loading needs every byte eventually; fetch it up front so the
        // bulk loader can keep using continuous slices.
        self.ensure_all()?;
        let offset = self
            .buffer
            .windows(5)
            .position(|w| w == b"%PDF-")
            .unwrap_or(0);
        self.strip_prefix_bytes(offset);

        // The document structure can be expressed in PEG as:
        //   document <- header indirect_object* xref trailer xref_start
        let version =
            parser::header(&self.buffer, self.strict).ok_or(ParseError::InvalidFileHeader)?;

        //The binary_mark is in line 2 after the pdf version. If at other line number, then will be declared as invalid
        // pdf.
        if let Some(pos) = self.buffer.iter().position(|&byte| byte == b'\n')
            && let Some(binary_mark) = parser::binary_mark(&self.buffer[pos + 1..])
            && binary_mark.iter().all(|&byte| byte >= 128)
        {
            self.document.binary_mark = binary_mark;
        }

        // Fall back to reconstruction only after all standard resolution fails.
        let (xref, trailer) = match self.resolve_xref_and_trailer() {
            Ok(resolved) => resolved,
            Err(err) => match self.reconstruct_xref_and_trailer() {
                Some(reconstructed) => {
                    warn!(
                        "cross-reference resolution failed ({err}); recovered by scanning for indirect objects"
                    );
                    reconstructed
                }
                None => return Err(err),
            },
        };

        self.document.version = version;
        self.document.max_id = xref.size - 1;
        self.document.trailer = trailer;
        self.set_reference_table(xref);

        // Check if encrypted
        let is_encrypted = self.document.trailer.get(b"Encrypt").is_ok();

        if is_encrypted {
            // For encrypted PDFs, use a special loading strategy
            self.load_encrypted_document(filter_func)?;
        } else {
            // For non-encrypted PDFs, use the normal loading
            self.load_objects_raw(filter_func)?;
        }

        // Inline page dictionaries in /Kids become indirect objects so every
        // leaf has an id.  Skip encrypted files: their dictionaries are
        // decrypted in place and new objects must not bypass the writer's
        // encryption path.
        if !is_encrypted {
            if let Err(err) = self.document.materialize_inline_pages() {
                warn!("could not materialise inline page dictionaries: {err}");
            }
        }

        // Object-stream members join `objects` after `max_id` was derived from the xref size,
        // so raise the ceiling to the highest loaded object.
        if let Some(&max_loaded_id) = self.document.objects.keys().next_back() {
            self.document.max_id = self.document.max_id.max(max_loaded_id.0);
        }

        Ok(self.document)
    }

    fn load_encrypted_document(&mut self, _filter_func: Option<FilterFunc>) -> Result<()> {
        // First, extract all raw object bytes without parsing
        let entries: Vec<_> = self
            .document
            .reference_table
            .entries
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        let mut object_streams = Vec::new();

        for (obj_num, entry) in entries {
            match entry {
                XrefEntry::Normal { offset, .. } => {
                    if let Ok((obj_id, start, end)) = self.extract_raw_object(offset as usize) {
                        self.raw_objects.push((obj_id, start, end));
                    }
                }
                XrefEntry::Compressed { container, index } => {
                    // Store compressed object info for later processing
                    object_streams.push((obj_num, container, index));
                }
                XrefEntry::Free { .. } | XrefEntry::UnusableFree => {
                    // Skip free entries
                }
            }
        }

        self.parse_encryption_dictionary()?;

        if self.authenticate_and_setup_encryption(false)?.is_none() {
            return Ok(());
        }

        if let Some(ref state) = self.encryption_state {
            let encrypt_ref = self
                .document
                .trailer
                .get(b"Encrypt")
                .ok()
                .and_then(|o| o.as_reference().ok());

            for (obj_id, start, end) in std::mem::take(&mut self.raw_objects) {
                if let Some(enc_ref) = encrypt_ref
                    && obj_id == enc_ref
                {
                    continue;
                }
                let Some(raw_bytes) = self.buffer.get(start..end) else {
                    continue;
                };

                if let Ok((id, mut obj)) = self.parse_raw_object(raw_bytes) {
                    let _ = encryption::decrypt_object(state, obj_id, &mut obj);
                    self.document.objects.insert(id, obj);
                }
            }

            let mut streams_to_process: std::collections::HashMap<u32, Vec<(u32, u16)>> =
                std::collections::HashMap::new();
            for (obj_num, container_id, index) in object_streams {
                streams_to_process
                    .entry(container_id)
                    .or_default()
                    .push((obj_num, index));
            }

            for (container_id, objects_in_stream) in streams_to_process {
                if let Some(container_obj) = self.document.objects.get(&(container_id, 0))
                    && let Ok(stream) = container_obj.as_stream()
                {
                    match ObjectStream::new_with_limit(stream, self.max_decompressed_size) {
                        Ok(object_stream) => {
                            for (obj_num, index) in objects_in_stream {
                                let obj_id = (obj_num, 0);
                                if let Some((member_id, object)) =
                                    object_stream.member_at(usize::from(index))
                                    && member_id == obj_id
                                {
                                    self.document.objects.insert(obj_id, object.clone());
                                }
                            }
                        }
                        Err(_e) => {}
                    }
                }
            }

            let mut stored_state = state.clone();
            // Record the /Encrypt id so a later incremental save can restore it in the trailer.
            stored_state.encrypt_object_id = encrypt_ref;
            self.document.encryption_state = Some(stored_state);

            if let Some(enc_ref) = encrypt_ref {
                self.document.objects.remove(&enc_ref);
            }
            self.document.trailer.remove(b"Encrypt");
        }

        Ok(())
    }

    fn parse_raw_object(&self, raw_bytes: &[u8]) -> Result<(ObjectId, Object)> {
        // Parse the raw bytes as an indirect object
        parser::indirect_object(raw_bytes, 0, None, self, &mut HashSet::new(), None)
    }

    /// Install a fully merged cross-reference table and derive its sorted
    /// offset index. All later object-boundary lookups go through that index,
    /// so it must be rebuilt whenever the table is replaced.
    fn set_reference_table(&mut self, xref: Xref) {
        let mut normal_offsets: Vec<usize> = xref
            .entries
            .values()
            .filter_map(|entry| match entry {
                XrefEntry::Normal { offset, .. } => Some(*offset as usize),
                _ => None,
            })
            .collect();
        normal_offsets.sort_unstable();
        normal_offsets.dedup();
        self.normal_offsets = normal_offsets;
        self.document.reference_table = xref;
    }

    /// Whether the merged cross-reference table selects `member` at `index`
    /// inside `container`.  A type-2 entry's index is the member position, and
    /// the member stored there must also carry the requested object number;
    /// pdfium rejects a mismatch (`CPDF_ObjectStream::ParseObject`,
    /// cpdf_object_stream.cpp:82-101).
    fn xref_selects_member(
        reference_table: &Xref,
        member: ObjectId,
        container: u32,
        index: usize,
    ) -> bool {
        matches!(
            reference_table.get(member.0),
            Some(XrefEntry::Compressed {
                container: entry_container,
                index: entry_index,
            }) if *entry_container == container && usize::from(*entry_index) == index
        )
    }

    fn load_objects_raw(&mut self, filter_func: Option<FilterFunc>) -> Result<()> {
        let is_encrypted = self.document.trailer.get(b"Encrypt").is_ok();
        let zero_length_streams = Mutex::new(vec![]);
        let object_streams = Mutex::new(vec![]);
        let interner = self
            .intern_dictionaries
            .then(|| Mutex::new(DictionaryInterner::default()));

        let entries_filter_map =
            |(&id_number, entry): (&u32, &XrefEntry)| -> Result<Option<(ObjectId, Object)>> {
                if let XrefEntry::Normal { offset, generation } = *entry {
                    // The xref entry identifies the object, so a slightly wrong
                    // offset can be recovered instead of loading a neighbouring
                    // object or silently dropping this one.
                    let expected_id = (id_number, generation);
                    let result =
                        self.read_object(offset as usize, Some(expected_id), &mut HashSet::new());
                    let (object_id, mut object) = match result {
                        Ok(obj) => obj,
                        Err(e) => {
                            if is_encrypted {
                                // Non-fatal even in strict mode: an encrypted object does not
                                // indicate a malformed file.
                                warn!("Skipping encrypted object at offset {}: {:?}", offset, e);
                                return Ok(None);
                            }
                            // Lenient loading logs and skips malformed objects; strict loading fails.
                            error!("Object load error at offset {}: {e:?}", offset);
                            return if self.strict { Err(e) } else { Ok(None) };
                        }
                    };
                    if let Some(filter_func) = filter_func
                        && filter_func(object_id, &mut object).is_none()
                    {
                        return Ok(None);
                    }

                    if let Ok(stream) = object.as_stream() {
                        if stream.dict.has_type(b"ObjStm") && !is_encrypted {
                            let obj_stream = match ObjectStream::new_with_limit(
                                stream,
                                self.max_decompressed_size,
                            ) {
                                Ok(obj_stream) => obj_stream,
                                // Not an encryption-related loss, so it obeys the same
                                // strict-mode policy as any other load error.
                                Err(e) => {
                                    error!("Object stream load error for {object_id:?}: {e:?}");
                                    return if self.strict { Err(e) } else { Ok(None) };
                                }
                            };
                            let container_id = object_id.0;
                            let mut members = obj_stream.into_members();
                            if let Some(interner) = &interner {
                                let mut interner = interner.lock().unwrap();
                                for member in &mut members {
                                    if let Some((_, object)) = member {
                                        interner.intern_object(object);
                                    }
                                }
                            }
                            let mut object_streams = object_streams.lock().unwrap();
                            // The declared index is part of the identity: a
                            // stale ObjStm copy from another container (e.g. a
                            // linearization first-page section) or a member at
                            // the wrong position must not answer the entry
                            // (pdfium CPDF_ObjectStream::ParseObject,
                            // cpdf_object_stream.cpp:82-101).
                            let selected =
                                |index: usize,
                                 member: Option<(ObjectId, Object)>|
                                 -> Option<(ObjectId, Object)> {
                                    let (object_id, object) = member?;
                                    Self::xref_selects_member(
                                        &self.document.reference_table,
                                        object_id,
                                        container_id,
                                        index,
                                    )
                                    .then_some((object_id, object))
                                };
                            if let Some(filter_func) = filter_func {
                                let objects: BTreeMap<(u32, u16), Object> = members
                                    .into_iter()
                                    .enumerate()
                                    .filter_map(|(index, member)| {
                                        let (object_id, mut object) = selected(index, member)?;
                                        filter_func(object_id, &mut object)
                                    })
                                    .collect();
                                object_streams.extend(objects);
                            } else {
                                object_streams.extend(
                                    members
                                        .into_iter()
                                        .enumerate()
                                        .filter_map(|(index, member)| selected(index, member)),
                                );
                            }
                        } else if stream.content.is_empty() {
                            let mut zero_length_streams = zero_length_streams.lock().unwrap();
                            zero_length_streams.push(object_id);
                        }
                    }

                    if let Some(interner) = &interner {
                        interner.lock().unwrap().intern_object(&mut object);
                    }
                    Ok(Some((object_id, object)))
                } else {
                    Ok(None)
                }
            };

        #[cfg(feature = "rayon")]
        {
            self.document.objects = self
                .document
                .reference_table
                .entries
                .par_iter()
                .map(entries_filter_map)
                .filter_map(Result::transpose)
                .collect::<Result<crate::ObjectStore>>()?;
        }
        #[cfg(not(feature = "rayon"))]
        {
            self.document.objects = self
                .document
                .reference_table
                .entries
                .iter()
                .map(entries_filter_map)
                .filter_map(Result::transpose)
                .collect::<Result<crate::ObjectStore>>()?;
        }

        // Only add entries, but never replace entries
        for (id, entry) in object_streams.into_inner().unwrap() {
            if !self.document.objects.contains_key(&id) { self.document.objects.insert(id, entry); }
        }

        for object_id in zero_length_streams.into_inner().unwrap() {
            let _ = self.read_stream_content(object_id);
        }

        Ok(())
    }

    fn read_stream_content(&mut self, object_id: ObjectId) -> Result<()> {
        let length = self.get_stream_length(object_id)?;
        let stream = self
            .document
            .get_object_mut(object_id)
            .and_then(Object::as_stream_mut)?;
        let start = stream
            .start_position
            .ok_or(Error::InvalidStream("missing start position".to_string()))?;

        if length < 0 {
            return Err(Error::InvalidStream("negative stream length.".to_string()));
        }

        let length = usize::try_from(length).map_err(|e| Error::NumericCast(e.to_string()))?;
        let end = start + length;

        if end > self.buffer.len() {
            return Err(Error::InvalidStream(
                "stream extends after document end.".to_string(),
            ));
        }

        stream.set_content(self.buffer[start..end].to_vec());
        Ok(())
    }

    fn get_stream_length(&self, object_id: ObjectId) -> Result<i64> {
        let object = self.document.get_object(object_id)?;
        let stream = object.as_stream()?;
        stream
            .dict
            .get(b"Length")
            .and_then(|value| self.document.dereference(value))
            .and_then(|(_id, obj)| match obj.as_i64() {
                Ok(length) => Ok(length),
                // s7.3.8.2 requires an integer /Length, but some producers write a real ("42.");
                // accept one whose value is integral and in range.
                Err(err) => match obj.as_f32() {
                    Ok(value)
                        if value.fract() == 0.0
                            && value >= -(2f32.powi(63))
                            && value < 2f32.powi(63) =>
                    {
                        Ok(value as i64)
                    }
                    _ => Err(err),
                },
            })
            .inspect_err(|_err| {
                error!(
                    "stream dictionary of '{} {} R' is missing the Length entry",
                    object_id.0, object_id.1
                );
            })
    }

    /// Get object offset by object ID.
    fn get_offset(&self, id: ObjectId) -> Result<u32> {
        let entry = self
            .document
            .reference_table
            .get(id.0)
            .ok_or(Error::MissingXrefEntry)?;
        match *entry {
            XrefEntry::Normal { offset, generation } if generation == id.1 => Ok(offset),
            _ => Err(Error::MissingXrefEntry),
        }
    }

    /// Load a compressed object from an object stream (for lightweight metadata extraction)
    ///
    /// `already_seen` is the resolution path of the caller. Reading the
    /// container may resolve its own indirect `/Length`, and if that length
    /// lives in the same container, a fresh set would recurse until the stack
    /// overflows.
    fn get_compressed_object(
        &self,
        id: ObjectId,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<Object> {
        let entry = self
            .document
            .reference_table
            .get(id.0)
            .ok_or(Error::MissingXrefEntry)?;

        let (container_id, index) = match entry {
            XrefEntry::Compressed { container, index } => (*container, usize::from(*index)),
            _ => return Err(Error::MissingXrefEntry),
        };

        let object_stream = self.object_stream(container_id, already_seen)?;
        // The declared index selects a position; that member must also carry
        // the requested object number.  A corrupt index never falls back to a
        // same-numbered member elsewhere in the stream (pdfium
        // CPDF_ObjectStream::ParseObject, cpdf_object_stream.cpp:82-101).
        match object_stream.member_at(index) {
            Some((member_id, object)) if member_id == id => Ok(object.clone()),
            _ => Err(Error::MissingXrefEntry),
        }
    }

    /// Decode object stream `container` once per load and reuse it afterwards.
    ///
    /// The result depends only on the buffer, the cross-reference table and
    /// the decompression limit, all fixed while the reader lives, so caching it
    /// changes nothing but the cost. The lock is not held while decoding: two
    /// threads may decode the same container, and both get the same result.
    ///
    /// A reference cycle is the one failure that is not cached: it depends on
    /// the caller's resolution path in `already_seen`, not on the container,
    /// and a caller coming by another path may read the same container fine.
    fn object_stream(
        &self,
        container: u32,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<Arc<ObjectStream>> {
        if let Some(cached) = self.object_streams.lock().unwrap().get(&container) {
            return cached.clone().ok_or_else(|| {
                Error::InvalidObjectStream(format!(
                    "object stream {container} 0 R failed to decode"
                ))
            });
        }
        let decoded = self
            .get_object_cached((container, 0), already_seen)
            .and_then(|object| {
                let stream = object.as_stream()?;
                // Only a real /Type /ObjStm stream is an object table
                // (pdfium IsObjectStream, cpdf_object_stream.cpp:23-57).
                if !stream.dict.has_type(b"ObjStm") {
                    return Err(Error::InvalidObjectStream(
                        "compressed object container is not /Type /ObjStm".to_string(),
                    ));
                }
                if !matches!(
                    stream.dict.get(b"N").and_then(Object::as_i64),
                    Ok(n) if n >= 0
                ) || !matches!(
                    stream.dict.get(b"First").and_then(Object::as_i64),
                    Ok(first) if first >= 0
                ) {
                    return Err(Error::InvalidObjectStream(
                        "object stream is missing a non-negative integer /N or /First".to_string(),
                    ));
                }
                ObjectStream::new_with_limit(stream, self.max_decompressed_size)
            })
            .map(Arc::new);
        if !matches!(decoded, Err(Error::ReferenceCycle(_))) {
            self.object_streams
                .lock()
                .unwrap()
                .insert(container, decoded.as_ref().ok().cloned());
        }
        decoded
    }

    /// Resolves an object, fetching its bytes first when a range source is
    /// active.  Shell loading and lazy page resolution call this; parser
    /// internals that only follow `/Length` use [`Reader::get_object_cached`]
    /// because they hold an immutable borrow.
    pub fn get_object(
        &mut self,
        id: ObjectId,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<Object> {
        let sparse = self.sparse.is_some();
        let mut object = if sparse {
            self.get_object_sparse(id, already_seen, 0)?
        } else {
            self.ensure_object(id)?;
            self.get_object_cached(id, already_seen)?
        };
        // The non-sparse resolver decrypts inside `get_object_cached`; the
        // sparse path reads a window directly and needs the same treatment.
        // Compressed members are already plaintext because their container
        // stream was decrypted as a whole.
        if sparse
            && let Some(state) = self.encryption_state.as_ref()
        {
            let encrypt_ref = self
                .document
                .trailer
                .get(b"Encrypt")
                .ok()
                .and_then(|object| object.as_reference().ok());
            let compressed = matches!(
                self.document.reference_table.get(id.0),
                Some(XrefEntry::Compressed { .. })
            );
            if !compressed && encrypt_ref != Some(id) {
                encryption::decrypt_object(state, id, &mut object).map_err(Error::Decryption)?;
            }
        }
        if self.intern_dictionaries {
            self.dictionary_interner
                .lock()
                .unwrap()
                .intern_object(&mut object);
        }
        Ok(object)
    }

    fn range_cache_get(&self, id: ObjectId) -> Option<Object> {
        self.range_objects
            .lock()
            .unwrap()
            .iter()
            .find(|(cached, _)| *cached == id)
            .map(|(_, object)| object.clone())
    }

    fn range_cache_put(&self, id: ObjectId, object: Object) {
        let mut cache = self.range_objects.lock().unwrap();
        if cache.iter().any(|(cached, _)| *cached == id) {
            return;
        }
        if cache.len() == RANGE_OBJECT_CACHE {
            cache.pop_front();
        }
        cache.push_back((id, object));
    }

    /// `(window_start, object_end, window_end)` for the object at `offset`.
    /// The window includes the recovery margin so header search and
    /// unterminated-stream recovery see the same neighbour bytes as eager
    /// parsing.
    fn object_window(&self, offset: usize) -> (usize, usize, usize) {
        let next = self
            .normal_offsets
            .get(self.normal_offsets.partition_point(|&next| next <= offset))
            .copied();
        let object_end = self.object_end(offset, next);
        let start = offset.saturating_sub(4096);
        let end = object_end.saturating_add(4096).min(self.logical_len());
        (start, object_end, end)
    }

    /// Fetches and parses one object through the sparse store.
    fn get_object_sparse(
        &mut self,
        id: ObjectId,
        already_seen: &mut HashSet<ObjectId>,
        depth: usize,
    ) -> Result<Object> {
        if let Some(object) = self.range_cache_get(id) {
            return Ok(object);
        }
        if depth > 8 {
            return Err(Error::ReferenceCycle(id));
        }
        if let Some(object) = self.document.objects.get(&id) {
            return Ok(object.clone());
        }
        if already_seen.contains(&id) {
            warn!(
                "reference cycle detected resolving object {} {}",
                id.0, id.1
            );
            return Err(Error::ReferenceCycle(id));
        }
        already_seen.insert(id);

        let entry = self
            .document
            .reference_table
            .get(id.0)
            .cloned()
            .ok_or(Error::MissingXrefEntry)?;
        match entry {
            XrefEntry::Normal { offset, generation } if generation == id.1 => {
                let offset = offset as usize;
                if offset >= self.logical_len() {
                    return Err(Error::InvalidOffset(offset));
                }
                let (start, object_end, end) = self.object_window(offset);
                self.load_window(start, end)?;
                // Stream dictionaries may name an indirect `/Length`; the
                // parser only holds `&self`, so resolve those first and keep
                // them in the bounded range cache.
                let refs = {
                    let from = offset - start;
                    let to = object_end.min(self.window_base + self.buffer.len()) - start;
                    length_references(&self.buffer[from..to])
                };
                for (number, generation) in refs {
                    let length_id = (number, generation);
                    if self.range_cache_get(length_id).is_none()
                        && !self.document.objects.contains_key(&length_id)
                        && let Ok(object) =
                            self.get_object_sparse(length_id, &mut HashSet::new(), depth + 1)
                    {
                        self.range_cache_put(length_id, object);
                    }
                }
                // Resolving lengths moved the window; reload this object's
                // extent (the store keeps it unless the budget evicted it).
                self.load_window(start, end)?;
                let (_, object) = self.read_object_windowed(offset, Some(id), already_seen)?;
                Ok(object)
            }
            XrefEntry::Compressed { container, .. } => {
                // The member bytes live in the container object stream.
                let Some(XrefEntry::Normal { offset, .. }) =
                    self.document.reference_table.get(container).cloned()
                else {
                    return Err(Error::MissingXrefEntry);
                };
                let offset = offset as usize;
                if offset >= self.logical_len() {
                    return Err(Error::InvalidOffset(offset));
                }
                let (start, object_end, end) = self.object_window(offset);
                self.load_window(start, end)?;
                let refs = {
                    let from = offset - start;
                    let to = object_end.min(self.window_base + self.buffer.len()) - start;
                    length_references(&self.buffer[from..to])
                };
                for (number, generation) in refs {
                    let length_id = (number, generation);
                    if self.range_cache_get(length_id).is_none()
                        && !self.document.objects.contains_key(&length_id)
                        && let Ok(object) =
                            self.get_object_sparse(length_id, &mut HashSet::new(), depth + 1)
                    {
                        self.range_cache_put(length_id, object);
                    }
                }
                self.load_window(start, end)?;
                self.get_compressed_object(id, already_seen)
            }
            XrefEntry::Free { .. } | XrefEntry::UnusableFree => Err(Error::MissingXrefEntry),
            XrefEntry::Normal { .. } => Err(Error::MissingXrefEntry),
        }
    }

    /// Resolves an object from bytes already present in the buffer.  With a
    /// range source the caller is responsible for calling [`Reader::get_object`]
    /// (or ensuring the extent itself).
    pub fn get_object_cached(
        &self,
        id: ObjectId,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<Object> {
        if self.sparse.is_some()
            && let Some(object) = self.range_cache_get(id)
        {
            return Ok(object);
        }
        if already_seen.contains(&id) {
            warn!(
                "reference cycle detected resolving object {} {}",
                id.0, id.1
            );
            return Err(Error::ReferenceCycle(id));
        }
        already_seen.insert(id);

        if let Some(entry) = self.document.reference_table.get(id.0)
            && matches!(entry, XrefEntry::Compressed { .. })
        {
            return self.get_compressed_object(id, already_seen);
        }

        let offset = self.get_offset(id)?;
        let (_, mut obj) = self.read_object(offset as usize, Some(id), already_seen)?;

        if let Some(ref state) = self.encryption_state {
            let encrypt_ref = self
                .document
                .trailer
                .get(b"Encrypt")
                .ok()
                .and_then(|o| o.as_reference().ok());
            if let Some(enc_ref) = encrypt_ref
                && id != enc_ref
            {
                encryption::decrypt_object(state, id, &mut obj).map_err(Error::Decryption)?;
            }
        }

        Ok(obj)
    }

    fn parse_encryption_dictionary(&mut self) -> Result<()> {
        if let Ok(encrypt_ref) = self
            .document
            .trailer
            .get(b"Encrypt")
            .and_then(|o| o.as_reference())
        {
            if self.raw_objects.is_empty() {
                // Shell loading: `read_object` cannot fetch a range-backed
                // or compressed Encrypt object itself, so go through the
                // window-aware resolver.
                let encrypt_obj = self.get_object(encrypt_ref, &mut HashSet::new())?;
                self.document.objects.insert(encrypt_ref, encrypt_obj);
            } else if let Some(&(_, start, end)) =
                self.raw_objects.iter().find(|(id, _, _)| *id == encrypt_ref)
                && let Some(raw_bytes) = self.buffer.get(start..end)
                && let Ok((_, obj)) = self.parse_raw_object(raw_bytes)
            {
                self.document.objects.insert(encrypt_ref, obj);
            }
        }
        Ok(())
    }

    fn authenticate_and_setup_encryption(
        &mut self,
        require_password: bool,
    ) -> Result<Option<String>> {
        // pdfium returns success without a password check when the effective
        // cipher is Identity (cpdf_security_handler.cpp, OnInit): /StmF and
        // /StrF Identity mean page content and strings are plaintext, and
        // only /EFF embedded files are encrypted.
        if self.document.has_identity_crypt_filters() {
            let state = encryption::EncryptionState::decode_identity(&self.document)?;
            self.encryption_state = Some(state);
            if let Ok(cache) = self.object_streams.get_mut() {
                cache.clear();
            }
            if let Ok(mut cache) = self.range_objects.lock() {
                cache.clear();
            }
            return Ok(Some(String::new()));
        }

        let password_to_use: Option<String> = if self.document.authenticate_password("").is_ok() {
            Some(String::new())
        } else if let Some(ref pwd) = self.password {
            if self.document.authenticate_password(pwd).is_ok() {
                Some(pwd.clone())
            } else if require_password {
                return Err(Error::InvalidPassword);
            } else {
                warn!("Invalid password provided for encrypted PDF");
                return Err(Error::InvalidPassword);
            }
        } else if require_password {
            return Err(Error::Unimplemented(
                "PDF is encrypted and requires a password. Use Document::load_metadata_with_password() instead.",
            ));
        } else {
            warn!("PDF is encrypted and requires a password");
            return Ok(None);
        };

        if let Some(ref password) = password_to_use {
            // Derive the key from the same sanitized bytes `authenticate_password` checked.
            let password =
                PasswordAlgorithm::try_from(&self.document)?.sanitize_password(password)?;
            let state = EncryptionState::decode(&self.document, &password)?;
            self.encryption_state = Some(state);
            // Containers decoded before decryption was set up hold ciphertext.
            if let Ok(cache) = self.object_streams.get_mut() {
                cache.clear();
            }
            if let Ok(mut cache) = self.range_objects.lock() {
                cache.clear();
            }
        }

        Ok(password_to_use)
    }

    fn setup_encryption_for_metadata(&mut self) -> Result<()> {
        self.parse_encryption_dictionary()?;
        self.authenticate_and_setup_encryption(true)?;
        Ok(())
    }

    fn extract_raw_object(&mut self, offset: usize) -> Result<(ObjectId, usize, usize)> {
        if offset > self.buffer.len() {
            return Err(Error::InvalidOffset(offset));
        }

        // Find object header (e.g., "19 0 obj")
        let slice = &self.buffer[offset..];

        let pos = skip_whitespace(slice, 0);

        // Get object number
        let (obj_num, pos) = scan_number::<u32>(slice, pos)?;
        let pos = skip_whitespace(slice, pos);

        // Get generation number
        let (obj_gen, pos) = scan_number::<u16>(slice, pos)?;

        // Skip to "obj"
        let pos = skip_whitespace(slice, pos);
        if pos + 3 > slice.len() || &slice[pos..pos + 3] != b"obj" {
            return Err(Error::Parse(ParseError::InvalidXref));
        }
        let body_start = pos + 3;

        // Find "endobj"
        let end_pos =
            find_from(slice, body_start, b"endobj").map_or(0, |end| end + b"endobj".len());

        // A truncated file may lack the keyword. Returning the rest of the buffer
        // would claim the object runs to the end of the file, so reject it and let
        // the caller decide how much of the damage to tolerate.
        if end_pos == 0 {
            return Err(Error::Parse(ParseError::InvalidXref));
        }

        // Keep the byte range in the reader's buffer instead of copying the
        // object; the parser can borrow it again after encryption is set up.
        Ok(((obj_num, obj_gen), offset, offset + end_pos))
    }

    fn read_object(
        &self,
        offset: usize,
        expected_id: Option<ObjectId>,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<(ObjectId, Object)> {
        if self.sparse.is_some() {
            return self.read_object_windowed(offset, expected_id, already_seen);
        }
        let next_index = self.normal_offsets.partition_point(|&next| next <= offset);
        let end = self.object_end(offset, self.normal_offsets.get(next_index).copied());
        let first = self.read_object_to(offset, end, expected_id, already_seen);
        if first.is_ok() || self.strict {
            return first;
        }
        let Some(expected_id) = expected_id else {
            return first;
        };
        let Some(recovered) = self.find_nearby_object_header(offset, expected_id) else {
            return first;
        };
        if recovered == offset {
            return first;
        }
        let next_index = self
            .normal_offsets
            .partition_point(|&next| next <= recovered);
        let end = self.object_end(recovered, self.normal_offsets.get(next_index).copied());
        // The failed attempt may have marked referenced /Length objects as
        // seen; retry with a fresh cycle-detection set.
        let mut retry_seen = HashSet::new();
        match self.read_object_to(recovered, end, Some(expected_id), &mut retry_seen) {
            Ok(object) => {
                warn!(
                    "xref entry for '{} {} R' points at {offset}, but the object header is at {recovered}",
                    expected_id.0, expected_id.1
                );
                Ok(object)
            }
            Err(_) => first,
        }
    }

    /// Parses one object from the current sparse window.  `offset` is still
    /// buffer-relative (file coordinates); only the parser sees window
    /// coordinates.
    fn read_object_windowed(
        &self,
        offset: usize,
        expected_id: Option<ObjectId>,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<(ObjectId, Object)> {
        let relative = offset
            .checked_sub(self.window_base)
            .ok_or(Error::InvalidOffset(offset))?;
        if relative > self.buffer.len() {
            return Err(Error::InvalidOffset(offset));
        }
        let next_index = self.normal_offsets.partition_point(|&next| next <= offset);
        let end = self.object_end(offset, self.normal_offsets.get(next_index).copied());
        let end = end.saturating_sub(self.window_base).min(self.buffer.len());
        if relative > end {
            return Err(Error::InvalidOffset(offset));
        }
        let first = parser::indirect_object(
            &self.buffer,
            relative,
            expected_id,
            self,
            already_seen,
            Some(end),
        );
        if first.is_ok() || self.strict {
            return first;
        }
        let Some(expected_id) = expected_id else {
            return first;
        };
        let Some(recovered) = self.find_nearby_object_header_window(relative, expected_id) else {
            return first;
        };
        if recovered == relative {
            return first;
        }
        let recovered_absolute = self.window_base + recovered;
        let next_index = self
            .normal_offsets
            .partition_point(|&next| next <= recovered_absolute);
        let end = self.object_end(
            recovered_absolute,
            self.normal_offsets.get(next_index).copied(),
        );
        let end = end.saturating_sub(self.window_base).min(self.buffer.len());
        let mut retry_seen = HashSet::new();
        match parser::indirect_object(
            &self.buffer,
            recovered,
            Some(expected_id),
            self,
            &mut retry_seen,
            Some(end),
        ) {
            Ok(object) => {
                warn!(
                    "xref entry for '{} {} R' points at {offset}, but the object header is at {recovered_absolute}",
                    expected_id.0, expected_id.1
                );
                Ok(object)
            }
            Err(_) => first,
        }
    }

    /// Sparse-window variant of [`Reader::find_nearby_object_header`].
    fn find_nearby_object_header_window(
        &self,
        relative: usize,
        expected_id: ObjectId,
    ) -> Option<usize> {
        const WINDOW: usize = 4096;
        let start = relative.saturating_sub(WINDOW);
        let end = (relative + WINDOW).min(self.buffer.len());
        let window = self.buffer.get(start..end)?;
        let mut best: Option<(usize, usize)> = None;
        for position in 0..window.len() {
            if Self::parse_object_header(&window[position..]) != Some(expected_id) {
                continue;
            }
            let absolute = start + position;
            let distance = absolute.abs_diff(relative);
            if best.is_none_or(|(best_distance, _)| distance < best_distance) {
                best = Some((distance, absolute));
            }
        }
        best.map(|(_, position)| position)
    }

    /// Find the nearest `N G obj` header within a small window around an xref
    /// offset.  Damaged producers often point a few bytes before or after the
    /// real header (for example at the preceding `endobj`).
    fn find_nearby_object_header(&self, offset: usize, expected_id: ObjectId) -> Option<usize> {
        const WINDOW: usize = 4096;
        let start = offset.saturating_sub(WINDOW);
        let end = (offset + WINDOW).min(self.buffer.len());
        let window = self.buffer.get(start..end)?;
        let mut best: Option<(usize, usize)> = None;
        for position in 0..window.len() {
            if Self::parse_object_header(&window[position..]) != Some(expected_id) {
                continue;
            }
            let absolute = start + position;
            let distance = absolute.abs_diff(offset);
            if best.is_none_or(|(best_distance, _)| distance < best_distance) {
                best = Some((distance, absolute));
            }
        }
        best.map(|(_, position)| position)
    }

    fn object_end(&self, offset: usize, next_object: Option<usize>) -> usize {
        let len = self.logical_len();
        let xref_start = (self.document.xref_start > offset).then_some(self.document.xref_start);
        next_object
            .into_iter()
            .chain(xref_start)
            .min()
            .unwrap_or(len)
            .min(len)
    }

    fn read_object_to(
        &self,
        offset: usize,
        end: usize,
        expected_id: Option<ObjectId>,
        already_seen: &mut HashSet<ObjectId>,
    ) -> Result<(ObjectId, Object)> {
        if offset > end || end > self.buffer.len() {
            return Err(Error::InvalidOffset(offset));
        }

        // Parse against the full buffer so a wrong xref offset cannot truncate a well-formed
        // object; `end` only bounds stream length recovery.
        parser::indirect_object(
            &self.buffer,
            offset,
            expected_id,
            self,
            already_seen,
            Some(end),
        )
    }

    /// Parse the cross-reference section recorded at `offset`, and return it with
    /// the offset it was read from. In lenient mode a slightly miswritten offset
    /// is corrected first, but only when the section found there locates its own
    /// objects: after bytes were inserted or removed before it, a nearby section
    /// points at shifted objects, and failing lets reconstruction find them.
    fn xref_and_trailer_at(&self, offset: usize) -> Result<(usize, Xref, Dictionary)> {
        if self.sparse.is_some() {
            let relative = offset
                .checked_sub(self.window_base)
                .ok_or(Error::InvalidOffset(offset))?;
            if relative > self.buffer.len() {
                return Err(Error::InvalidOffset(offset));
            }
            let corrected = self.correct_xref_offset_window(relative);
            if corrected != relative {
                if let Ok((xref, trailer)) =
                    parser::xref_and_trailer(&self.buffer[corrected..], self)
                {
                    let absolute = self.window_base + corrected;
                    warn!(
                        "Cross-reference offset {} does not point at an xref section; using nearby offset {} instead.",
                        offset, absolute
                    );
                    return Ok((absolute, xref, trailer));
                }
            }
            let (xref, trailer) = parser::xref_and_trailer(&self.buffer[relative..], self)?;
            return Ok((offset, xref, trailer));
        }
        let corrected = self.correct_xref_offset(offset);
        if corrected != offset
            && let Ok((xref, trailer)) = parser::xref_and_trailer(&self.buffer[corrected..], self)
            && self.locates_objects(&xref)
        {
            warn!(
                "Cross-reference offset {} does not point at an xref section; using nearby offset {} instead.",
                offset, corrected
            );
            return Ok((corrected, xref, trailer));
        }
        let (xref, trailer) = parser::xref_and_trailer(&self.buffer[offset..], self)?;
        Ok((offset, xref, trailer))
    }

    /// Whether most normal entries of a cross-reference section point at the
    /// header of their own object, as they do in a section describing this file.
    fn locates_objects(&self, xref: &Xref) -> bool {
        let (mut normal, mut located) = (0usize, 0usize);
        for (&number, entry) in &xref.entries {
            if let XrefEntry::Normal { offset, generation } = *entry {
                // A lazy reader may not have the object bodies yet; an
                // unfetched header is no evidence either way, so it is not
                // counted (and must not make a corrected offset look wrong).
                if !self.is_filled(offset as usize, offset as usize + 1) {
                    continue;
                }
                normal += 1;
                let rest = self.buffer.get(offset as usize..).unwrap_or_default();
                if Self::parse_object_header(&rest[skip_whitespace(rest, 0)..])
                    == Some((number, generation))
                {
                    located += 1;
                }
            }
        }
        normal == 0 || located * 2 > normal
    }

    /// Sparse variant of [`Reader::locates_objects`]: fetch a sample of the
    /// object headers a section claims and require most of them to match.
    /// Verification moves the parse window, so the caller reloads its xref
    /// window afterwards.
    fn locates_objects_sparse(&mut self, xref: &Xref) -> Result<bool> {
        let normals: Vec<(u32, usize, u16)> = xref
            .entries
            .iter()
            .filter_map(|(&number, entry)| match *entry {
                XrefEntry::Normal { offset, generation } => {
                    Some((number, offset as usize, generation))
                }
                _ => None,
            })
            .collect();
        if normals.is_empty() {
            return Ok(true);
        }
        let step = (normals.len() / 16).max(1);
        let samples: Vec<(u32, usize, u16)> =
            normals.iter().step_by(step).take(16).copied().collect();
        let mut located = 0usize;
        for (number, offset, generation) in &samples {
            let (number, offset, generation) = (*number, *offset, *generation);
            if offset >= self.logical_len() {
                continue;
            }
            let start = offset.saturating_sub(64);
            let end = offset.saturating_add(128).min(self.logical_len());
            self.load_window(start, end)?;
            let relative = offset - start;
            let rest = &self.buffer[relative..];
            if Self::parse_object_header(&rest[skip_whitespace(rest, 0)..])
                == Some((number, generation))
            {
                located += 1;
            }
        }
        Ok(located * 2 > samples.len())
    }

    /// Resolve the cross-reference table/stream and trailer, including the
    /// `/Prev` chain, and record the resolved start offset on the document.
    fn resolve_xref_and_trailer(&mut self) -> Result<(Xref, Dictionary)> {
        let len = self.logical_len();
        let tail = len.saturating_sub(65536);
        if self.sparse.is_some() {
            self.load_window(tail, len)?;
        } else {
            self.ensure(tail, len)?;
        }
        let xref_start = Self::get_xref_start(&self.buffer)?;
        if xref_start > len {
            return Err(Error::Xref(XrefError::Start));
        }
        self.ensure_xref_at(xref_start)?;
        let (xref_start, mut xref, mut trailer) = self.xref_and_trailer_at(xref_start)?;
        self.document.xref_start = xref_start;
        self.merge_hybrid_xref_stream(&mut xref, trailer.remove(b"XRefStm"))?;

        // Read previous Xrefs of linearized or incremental updated document.
        let mut already_seen = HashSet::new();
        let mut prev_xref_start = trailer.remove(b"Prev");
        while let Some(prev) = prev_xref_start.and_then(|offset| offset.as_i64().ok()) {
            if already_seen.contains(&prev) {
                break;
            }
            already_seen.insert(prev);
            if prev < 0 || prev as usize > self.logical_len() {
                return Err(Error::Xref(XrefError::PrevStart));
            }

            self.ensure_xref_at(prev as usize)?;
            let (_, prev_xref, mut prev_trailer) = self.xref_and_trailer_at(prev as usize)?;
            xref.merge(prev_xref);
            self.merge_hybrid_xref_stream(&mut xref, prev_trailer.remove(b"XRefStm"))?;

            prev_xref_start = prev_trailer.remove(b"Prev");
            // A newest trailer may omit /Root, /Info, /Encrypt or /ID that
            // an older revision still carries; newer keys win
            // (pdfium CPDF_CrossRefTable::UpdateTrailer).
            for (key, value) in prev_trailer.iter() {
                if trailer.get(key).is_err() {
                    trailer.set(key.clone(), value.clone());
                }
            }
        }
        let xref_entry_count = xref
            .max_id()
            .checked_add(1)
            .ok_or(ParseError::InvalidXref)?;
        if xref.size != xref_entry_count {
            warn!(
                "Size entry of trailer dictionary is {}, correct value is {}.",
                xref.size, xref_entry_count
            );
            xref.size = xref_entry_count;
        }

        if self.sparse.is_some() && !self.locates_objects_sparse(&xref)? {
            // A merged table whose entries mostly point somewhere other than
            // their own object header is unusable (systematically shifted
            // offsets).  The eager loader recovers by scanning; the shell
            // falls back to reconstruction after `ensure_all`.
            return Err(Error::Parse(ParseError::InvalidXref));
        }
        Ok((xref, trailer))
    }

    /// Merge the cross-reference stream that a section of a hybrid-reference file points to
    /// with `/XRefStm`. Its entries come after the section's table and before older sections.
    fn merge_hybrid_xref_stream(
        &mut self,
        xref: &mut Xref,
        xref_stream_start: Option<Object>,
    ) -> Result<()> {
        let Some(start) = xref_stream_start.and_then(|offset| offset.as_i64().ok()) else {
            return Ok(());
        };
        if start < 0 || start as usize > self.logical_len() {
            return Err(Error::Xref(XrefError::StreamStart));
        }
        self.ensure_xref_at(start as usize)?;
        match self.xref_and_trailer_at(start as usize) {
            Ok((_, stream_xref, _)) => xref.merge(stream_xref),
            // The table alone still describes a hybrid-reference file for older readers.
            Err(e) if !self.strict => {
                warn!("ignoring unreadable cross-reference stream at {start}: {e}")
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Last-resort recovery: rebuild the cross-reference table by scanning the
    /// raw bytes for indirect-object headers and locating a trailer with a
    /// usable `/Root`. Returns `None` (caller keeps the original error) when
    /// strict mode forbids recovery or nothing usable was found.
    fn reconstruct_xref_and_trailer(&mut self) -> Option<(Xref, Dictionary)> {
        if self.strict {
            return None;
        }
        u32::try_from(self.buffer.len()).ok()?;

        let markers = Self::scan_object_markers(&self.buffer);
        if markers.is_empty() {
            return None;
        }

        let mut xref = Xref::new(markers.len() as u32, XrefType::CrossReferenceTable);
        for (offset, (number, generation)) in markers {
            // Incremental updates append, so later revisions win.
            xref.insert(number, XrefEntry::Normal { offset, generation });
        }
        // Normalize like `resolve_xref_and_trailer`: size spans object numbers
        // up to the highest, not physical copies across incremental updates.
        xref.size = xref.max_id().saturating_add(1);

        let (_trailer_pos, mut trailer) = self
            .find_latest_trailer(&xref)
            .or_else(|| self.find_latest_xref_stream_trailer(&xref))?;
        // As in resolution, drop the links to other cross-reference sections: the rebuilt
        // table replaces them, and their offsets would go out unchanged on the next save.
        trailer.remove(b"Prev");
        trailer.remove(b"XRefStm");
        // Zero marks the offset "unknown" so `Document::new_from_prev` omits `/Prev`.
        self.document.xref_start = 0;

        warn!(
            "reconstructed cross-reference table with {} objects by scanning for indirect objects",
            xref.entries.len()
        );
        Some((xref, trailer))
    }

    /// Collect `(offset, id)` of every indirect-object header in one pass.
    /// Only headers starting a line (optional leading blanks allowed) count,
    /// stream payloads are skipped wholesale, and object numbers beyond
    /// [`MAX_RECONSTRUCTED_OBJECTS`] are rejected so a forged header can
    /// neither shadow a genuine entry nor poison the reconstructed size.
    fn scan_object_markers(buffer: &[u8]) -> Vec<(u32, ObjectId)> {
        const STREAM_KEYWORD: &[u8] = b"stream";
        const END_STREAM_KEYWORD: &[u8] = b"endstream";

        let mut markers = Vec::new();
        let mut oversized_number_warned = false;
        let mut in_comment = false;
        // Where the dictionary of the next stream can start: past the previous
        // `stream` keyword and the payload skipped after it.
        let mut dictionary_start = 0;
        // No `endstream` follows a later keyword either once one keyword has
        // none, so searching again would rescan the rest of the buffer.
        let mut unterminated = false;
        let mut pos = 0;
        while pos < buffer.len() {
            // Comments are not content: a comment ending in `stream`
            // (`% ... content stream`) must not start a payload skip, or the
            // objects and trailer keyword after it disappear from recovery.
            if in_comment {
                in_comment = !matches!(buffer[pos], b'\r' | b'\n');
                pos += 1;
                continue;
            }
            if buffer[pos] == b'%' {
                in_comment = true;
                pos += 1;
                continue;
            }
            // Literal and hex strings may contain convincing object headers;
            // pdfium's scan skips them (`ReadString`/`ReadHexString`), so the
            // recovery scanner must too. An unterminated string (a stray `(`
            // in damaged data) is not a string: resume at the next byte so it
            // cannot swallow the rest of the file and hide later revisions.
            if buffer[pos] == b'(' {
                if let Some(end) = skip_literal_string(buffer, pos) {
                    pos = end;
                } else {
                    pos += 1;
                }
                continue;
            }
            if buffer[pos] == b'<' {
                // Advance past both bytes of `<<`: its second `<` is not the
                // start of a hex string.
                if buffer.get(pos + 1) == Some(&b'<') {
                    pos += 2;
                    continue;
                }
                if let Some(end) = skip_hex_string(buffer, pos) {
                    pos = end;
                } else {
                    pos += 1;
                }
                continue;
            }
            // Skip raw stream data: a payload may embed convincing `N G obj` lines whose
            // offsets would otherwise override the genuine entries.
            if buffer[pos..].starts_with(STREAM_KEYWORD)
                && !buffer[..pos].ends_with(b"end")
                && matches!(buffer.get(pos + STREAM_KEYWORD.len()), Some(b'\r' | b'\n'))
            {
                let after_keyword = pos + STREAM_KEYWORD.len();
                let dictionary = std::mem::replace(&mut dictionary_start, after_keyword);
                let endstream = if unterminated {
                    None
                } else {
                    find_from(buffer, after_keyword, END_STREAM_KEYWORD)
                };
                unterminated = endstream.is_none();
                // Damaged stream without terminator: fall back to the /Length hint so the
                // payload cannot hide pseudo headers, while later objects stay reachable.
                let payload_end = endstream
                    .map(|end| end + END_STREAM_KEYWORD.len())
                    .or_else(|| Self::payload_end_by_length(buffer, dictionary, pos));
                if let Some(end) = payload_end {
                    pos = end;
                    dictionary_start = end;
                    continue;
                }
                // Unusable /Length too. Binary payloads contain arbitrary
                // bytes (including `(` and convincing headers), so resume
                // after the object's `endobj`, as pdfium's indirect-object
                // reader does; only a stream with no terminator at all falls
                // through to byte scanning.
                if let Some(end) = find_from(buffer, after_keyword, b"endobj").map(|p| p + 6) {
                    pos = end;
                    dictionary_start = end;
                    continue;
                }
            }
            // A header only starts where a number token can start: at the
            // buffer start or after whitespace/an enclosing delimiter.
            // Damaged producers concatenate objects (`>>2 0 obj`,
            // `endobj 2 0 obj`) without line breaks, so a line-start anchor
            // alone hides most of the document (pdfium's token scan accepts
            // any position).
            if buffer[pos].is_ascii_digit()
                && starts_token(buffer, pos)
                && let Some(id) = Self::parse_object_header(&buffer[pos..])
            {
                // A huge bogus number would inflate `size` (and thus `max_id`) via `Xref::insert`.
                if id.0 > MAX_RECONSTRUCTED_OBJECTS as u32 {
                    if !oversized_number_warned {
                        warn!("ignoring object headers numbered above {MAX_RECONSTRUCTED_OBJECTS}");
                        oversized_number_warned = true;
                    }
                } else {
                    if markers.len() == MAX_RECONSTRUCTED_OBJECTS {
                        warn!(
                            "object marker scan stopped at the {MAX_RECONSTRUCTED_OBJECTS}-marker cap; reconstruction may be incomplete"
                        );
                        break;
                    }
                    markers.push((pos as u32, id));
                }
            }
            pos += 1;
        }
        markers
    }

    /// Resume offset past a stream payload according to a *direct* `/Length`
    /// integer in the dictionary preceding the `stream` keyword at
    /// `stream_pos`. The search is scoped to the current object (after the
    /// nearest preceding `obj` keyword) so a missing `/Length` cannot latch
    /// onto a previous object's value. Nor does it reach back before
    /// `dictionary_start`, where the previous keyword or its payload ended:
    /// that dictionary belongs to another stream, and stopping there keeps
    /// the searches of all keywords to one pass over the buffer. Returns
    /// `None` when the hint is absent, indirect, or points outside the
    /// buffer — damaged files tend to carry wrong `/Length` values, so it
    /// must stay a hint, never a hard boundary.
    fn payload_end_by_length(
        buffer: &[u8],
        dictionary_start: usize,
        stream_pos: usize,
    ) -> Option<usize> {
        const LENGTH_KEY: &[u8] = b"/Length";
        const OBJ_KEYWORD: &[u8] = b"obj";
        const STREAM_KEYWORD_LEN: usize = b"stream".len();

        let head = buffer.get(dictionary_start..stream_pos)?;
        let obj_pos = dictionary_start
            + head
                .windows(OBJ_KEYWORD.len())
                .rposition(|window| window == OBJ_KEYWORD)?;
        let dict_region = &buffer[obj_pos + OBJ_KEYWORD.len()..stream_pos];
        // Nearest `/Length` in this object's dictionary; parsing it validates
        // that the match really is a key with an integer value.
        let key_pos = dict_region
            .windows(LENGTH_KEY.len())
            .rposition(|window| window == LENGTH_KEY)?;
        let rest = &dict_region[key_pos + LENGTH_KEY.len()..];
        let digits_start = rest.iter().position(u8::is_ascii_digit)?;
        let digits_len = rest[digits_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits_len > 10 {
            return None;
        }
        // An indirect reference (`/Length 5 0 R`) continues with another
        // number token; a direct integer ends at a name, `>>`, or the keyword.
        match rest[digits_start + digits_len..]
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
        {
            Some(b'/') | Some(b'>') => {}
            _ => return None,
        }
        let length: usize = std::str::from_utf8(&rest[digits_start..digits_start + digits_len])
            .ok()?
            .parse()
            .ok()?;
        // Spec: the EOL after the `stream` keyword counts as CRLF or LF.
        let eol_len = match (
            buffer.get(stream_pos + STREAM_KEYWORD_LEN),
            buffer.get(stream_pos + STREAM_KEYWORD_LEN + 1),
        ) {
            (Some(b'\r'), Some(b'\n')) => 2,
            _ => 1,
        };
        let end = (stream_pos + STREAM_KEYWORD_LEN + eol_len).checked_add(length)?;
        (buffer.len() >= end).then_some(end)
    }

    /// Parse an `N G obj` header; the byte after `obj` must end the token.
    fn parse_object_header(input: &[u8]) -> Option<ObjectId> {
        // deliberate: caps at u32/u16 width bound work on pathological padding.
        fn digits(input: &[u8], cap: usize) -> Option<(&[u8], &[u8])> {
            let n = input.iter().take_while(|b| b.is_ascii_digit()).count();
            (0 < n && n <= cap).then(|| input.split_at(n))
        }
        fn spaces(input: &[u8]) -> Option<&[u8]> {
            let n = input
                .iter()
                .take_while(|&&b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                .count();
            (n > 0).then(|| &input[n..])
        }

        let (number, rest) = digits(input, 10)?;
        let number: u32 = std::str::from_utf8(number).ok()?.parse().ok()?;
        let rest = spaces(rest)?;
        let (generation, rest) = digits(rest, 5)?;
        let generation: u16 = std::str::from_utf8(generation).ok()?.parse().ok()?;
        let rest = spaces(rest)?;
        let tail = rest.strip_prefix(b"obj")?;
        match tail.first() {
            None => {}
            Some(&byte) if !byte.is_ascii_alphanumeric() => {}
            _ => return None,
        }
        Some((number, generation))
    }

    /// Newest cross-reference stream whose dictionary carries a usable
    /// `/Root`.  A file with xref streams only has no `trailer` keyword, so
    /// recovery must read the stream dictionary itself (pdfium
    /// cpdf_parser.cpp cross-reference stream trailer path).
    fn find_latest_xref_stream_trailer(&self, xref: &Xref) -> Option<(usize, Dictionary)> {
        let mut candidates: Vec<usize> = xref
            .entries
            .values()
            .filter_map(|entry| match entry {
                XrefEntry::Normal { offset, .. } => Some(*offset as usize),
                _ => None,
            })
            .collect();
        candidates.sort_unstable();
        for offset in candidates.into_iter().rev() {
            if offset >= self.buffer.len() {
                continue;
            }
            let Ok((_, object)) = self.parse_raw_object(&self.buffer[offset..]) else {
                continue;
            };
            let Ok(stream) = object.as_stream() else {
                continue;
            };
            if !stream.dict.has_type(b"XRef") {
                continue;
            }
            if self.usable_recovered_root(xref, &stream.dict) {
                return Some((offset, stream.dict.clone()));
            }
        }
        None
    }

    /// Newest backwards-scanned `trailer` whose `/Root` references a scanned
    /// catalog with a page tree, ignoring trailer lookalikes inside stream
    /// payloads (pdfium `RebuildCrossRef`, cpdf_parser.cpp:809-859).
    fn find_latest_trailer(&self, xref: &Xref) -> Option<(usize, Dictionary)> {
        let payloads = Self::stream_payload_ranges(&self.buffer);
        let mut bound = self.buffer.len();
        for _ in 0..MAX_TRAILER_CANDIDATES {
            let Some(pos) = Reader::search_substring(&self.buffer[..bound], b"trailer", 0) else {
                break;
            };
            bound = pos;
            if payloads
                .iter()
                .any(|(start, end)| *start <= pos && pos < *end)
            {
                continue;
            }

            let after_keyword = &self.buffer[pos + b"trailer".len()..];
            // A comment may sit between `trailer` and its dictionary
            // (`trailer % The trailer part.`), as in the ISO 32000 example
            // files; comments are whitespace to the object grammar.
            let dict_start = skip_whitespace_and_comments(after_keyword, 0);
            if dict_start == after_keyword.len() {
                continue;
            }
            let Ok((_, dict)) = parser::dictionary(&after_keyword[dict_start..]) else {
                continue;
            };
            if self.usable_recovered_root(xref, &dict) {
                return Some((pos, dict));
            }
        }
        None
    }

    /// The byte ranges occupied by stream payloads.  A `stream` keyword only
    /// counts when it ends a line and is not the tail of `endstream`; the
    /// payload then runs to the next `endstream` (or the end of the buffer
    /// when the producer omitted it).
    fn stream_payload_ranges(buffer: &[u8]) -> Vec<(usize, usize)> {
        const STREAM_KEYWORD: &[u8] = b"stream";
        const END_STREAM_KEYWORD: &[u8] = b"endstream";

        let mut ranges = Vec::new();
        let mut pos = 0;
        let mut in_comment = false;
        // Where the dictionary before the next `stream` keyword can start.
        let mut dictionary_start = 0;
        while pos < buffer.len() {
            if in_comment {
                in_comment = !matches!(buffer[pos], b'\r' | b'\n');
                pos += 1;
                continue;
            }
            if buffer[pos] == b'%' {
                in_comment = true;
                pos += 1;
                continue;
            }
            let keyword_end = pos + STREAM_KEYWORD.len();
            if buffer[pos..].starts_with(STREAM_KEYWORD)
                && !buffer[..pos].ends_with(b"end")
                && matches!(buffer.get(keyword_end), Some(b'\r' | b'\n'))
            {
                let eol_len = if buffer.get(keyword_end) == Some(&b'\r')
                    && buffer.get(keyword_end + 1) == Some(&b'\n')
                {
                    2
                } else {
                    1
                };
                let payload_start = keyword_end + eol_len;
                let dictionary = std::mem::replace(&mut dictionary_start, keyword_end);
                let endstream = find_from(buffer, keyword_end, END_STREAM_KEYWORD)
                    .map(|end| end + END_STREAM_KEYWORD.len());
                // An unterminated stream without /Length ends at its
                // `endobj`, as in the object scanner. Treating the rest of
                // the file as payload would hide the real trailer from
                // recovery (pdfium's indirect-object reader stops at
                // `endobj`).
                let payload_end = endstream
                    .or_else(|| Self::payload_end_by_length(buffer, dictionary, pos))
                    .or_else(|| find_from(buffer, keyword_end, b"endobj").map(|p| p + 6))
                    .unwrap_or(buffer.len());
                ranges.push((payload_start, payload_end));
                pos = payload_end;
                dictionary_start = payload_end;
            } else {
                pos += 1;
            }
        }
        ranges
    }

    /// Whether a recovered trailer's `/Root` is a catalog dictionary whose
    /// `/Pages` resolves to a page-tree dictionary.  A forged `/Root` that
    /// points at an existing but non-catalog object must not select the
    /// recovered trailer.
    fn usable_recovered_root(&self, xref: &Xref, trailer: &Dictionary) -> bool {
        let Ok(root) = trailer.get(b"Root").and_then(Object::as_reference) else {
            return false;
        };
        let Some(XrefEntry::Normal { offset, .. }) = xref.get(root.0) else {
            return false;
        };
        let Ok((_, object)) = self.parse_raw_object(&self.buffer[*offset as usize..]) else {
            return false;
        };
        let Ok(catalog) = object.as_dict() else {
            return false;
        };
        if let Ok(kind) = catalog.get(b"Type").and_then(Object::as_name)
            && kind != b"Catalog"
        {
            return false;
        }
        match catalog.get(b"Pages") {
            Ok(Object::Reference(_)) => {
                // A damaged file may have a `/Pages` reference whose object
                // is unparseable or missing.  The catalog dict with a
                // `/Pages` reference is already strong evidence of a real
                // trailer; don't reject because the target can't be
                // validated (pdfium bug_1327884: `/Pages 2 0 R` where
                // `2 0 obj` holds a malformed dict).
                true
            }
            Ok(Object::Dictionary(pages)) => pages.has(b"Kids") || pages.has_type(b"Pages"),
            _ => false,
        }
    }

    /// Some generators write `startxref` (or trailer `Prev`) values that are
    /// slightly off — most commonly the offset of the line *after* the `xref`
    /// keyword instead of the keyword itself. Such files are otherwise intact,
    /// and common readers (Acrobat, poppler, mupdf, pdf.js) recover from them.
    /// In lenient mode, if `offset` does not point at a cross-reference table
    /// or an indirect object (cross-reference stream), scan a small window
    /// around it for the `xref` keyword and return the nearest match instead,
    /// which `xref_and_trailer_at` checks before using it. Without one, as in
    /// a file with cross-reference streams only, return the nearest object
    /// header that starts a cross-reference stream.
    fn correct_xref_offset(&self, offset: usize) -> usize {
        const RECOVERY_WINDOW: usize = 64;

        if self.strict || offset >= self.buffer.len() {
            return offset;
        }
        let rest = &self.buffer[offset..];
        if rest.starts_with(b"xref") || Self::starts_indirect_object(rest) {
            return offset;
        }

        let window_start = offset.saturating_sub(RECOVERY_WINDOW);
        let window_end = cmp::min(self.buffer.len(), offset + RECOVERY_WINDOW);
        let mut corrected: Option<usize> = None;
        for pos in window_start..window_end.saturating_sub(4) {
            if !self.buffer[pos..].starts_with(b"xref") {
                continue;
            }
            // `startxref` contains `xref`; never match inside it.
            if pos >= 5 && &self.buffer[pos - 5..pos] == b"start" {
                continue;
            }
            if corrected.is_none_or(|best: usize| pos.abs_diff(offset) < best.abs_diff(offset)) {
                corrected = Some(pos);
            }
        }
        corrected
            .or_else(|| self.nearest_xref_stream(offset, window_start, window_end))
            .unwrap_or(offset)
    }

    /// Sparse-window variant of [`Reader::correct_xref_offset`]; all positions
    /// are relative to the window in `buffer`.
    fn correct_xref_offset_window(&self, offset: usize) -> usize {
        const RECOVERY_WINDOW: usize = 64;

        if self.strict || offset >= self.buffer.len() {
            return offset;
        }
        let rest = &self.buffer[offset..];
        if rest.starts_with(b"xref") || Self::starts_indirect_object(rest) {
            return offset;
        }

        let window_start = offset.saturating_sub(RECOVERY_WINDOW);
        let window_end = cmp::min(self.buffer.len(), offset + RECOVERY_WINDOW);
        let mut corrected: Option<usize> = None;
        for pos in window_start..window_end.saturating_sub(4) {
            if !self.buffer[pos..].starts_with(b"xref") {
                continue;
            }
            if pos >= 5 && &self.buffer[pos - 5..pos] == b"start" {
                continue;
            }
            if corrected.is_none_or(|best: usize| pos.abs_diff(offset) < best.abs_diff(offset)) {
                corrected = Some(pos);
            }
        }
        corrected
            .or_else(|| self.nearest_xref_stream_window(offset, window_start, window_end))
            .unwrap_or(offset)
    }

    /// Sparse-window variant of [`Reader::nearest_xref_stream`].
    fn nearest_xref_stream_window(
        &self,
        offset: usize,
        window_start: usize,
        window_end: usize,
    ) -> Option<usize> {
        let rest = &self.buffer[offset..];
        if Self::starts_indirect_object(&rest[skip_whitespace(rest, 0)..]) {
            return None;
        }
        let mut headers: Vec<usize> = (window_start..window_end)
            .filter(|&pos| pos == 0 || !self.buffer[pos - 1].is_ascii_alphanumeric())
            .filter(|&pos| Self::starts_indirect_object(&self.buffer[pos..]))
            .collect();
        headers.sort_by_key(|pos| pos.abs_diff(offset));
        headers.into_iter().find(|&pos| {
            matches!(
                parser::indirect_object(&self.buffer, pos, None, self, &mut HashSet::new(), None),
                Ok((_, Object::Stream(stream))) if stream.dict.has_type(b"XRef")
            )
        })
    }

    /// Start of the object header between `window_start` and `window_end` that
    /// is nearest to `offset` and begins a cross-reference stream. The stream
    /// parser itself skips white-space before a header, so an `offset` there
    /// needs no correction.
    fn nearest_xref_stream(
        &self,
        offset: usize,
        window_start: usize,
        window_end: usize,
    ) -> Option<usize> {
        let rest = &self.buffer[offset..];
        if Self::starts_indirect_object(&rest[skip_whitespace(rest, 0)..]) {
            return None;
        }
        // A header starts a token: `12 0 obj` must not also match as `2 0 obj`.
        let mut headers: Vec<usize> = (window_start..window_end)
            .filter(|&pos| pos == 0 || !self.buffer[pos - 1].is_ascii_alphanumeric())
            .filter(|&pos| Self::starts_indirect_object(&self.buffer[pos..]))
            .collect();
        headers.sort_by_key(|pos| pos.abs_diff(offset));
        headers.into_iter().find(|&pos| {
            matches!(
                parser::indirect_object(&self.buffer, pos, None, self, &mut HashSet::new(), None),
                Ok((_, Object::Stream(stream))) if stream.dict.has_type(b"XRef")
            )
        })
    }

    /// Whether `input` begins with an indirect-object header (`N G obj`), the
    /// form a cross-reference stream starts with.
    fn starts_indirect_object(input: &[u8]) -> bool {
        Self::parse_object_header(input).is_some()
    }

    fn get_xref_start(buffer: &[u8]) -> Result<usize> {
        // Like other readers, tolerate white-space padding (often NUL bytes) after `%%EOF` and
        // look for the marker within the last 1024 bytes of the rest.
        let end = buffer
            .iter()
            .rposition(|c| !b" \t\n\r\0\x0C".contains(c))
            .map_or(0, |pos| pos + 1);
        let seek_pos = end - cmp::min(end, 1024);
        Self::search_substring(&buffer[..end], b"%%EOF", seek_pos)
            .filter(|&eof_pos| eof_pos > 25)
            .and_then(|eof_pos| {
                Self::search_substring(&buffer[..eof_pos], b"startxref", eof_pos - 25)
            })
            .ok_or(Error::Xref(XrefError::Start))
            .and_then(|xref_pos| {
                if xref_pos <= buffer.len() {
                    match parser::xref_start(&buffer[xref_pos..]) {
                        Some(startxref) => Ok(startxref as usize),
                        None => Err(Error::Xref(XrefError::Start)),
                    }
                } else {
                    Err(Error::Xref(XrefError::Start))
                }
            })
    }

    fn search_substring(buffer: &[u8], pattern: &[u8], start_pos: usize) -> Option<usize> {
        buffer
            .get(start_pos..)?
            .windows(pattern.len())
            .rposition(|window| window == pattern)
            .map(|pos| start_pos + pos)
    }
}

/// Advance past ASCII whitespace, returning the first index that is not one (or
/// `input.len()` when the rest is all whitespace).
fn skip_whitespace(input: &[u8], mut pos: usize) -> usize {
    // ISO 32000 Table 1: NUL (0x00) is a white-space character, as are the
    // five `is_ascii_whitespace` bytes.  pdfium's token scanner treats it the
    // same way; a reference such as `1 0\0R` (bug_782596) must still parse.
    while pos < input.len() && (input[pos].is_ascii_whitespace() || input[pos] == 0) {
        pos += 1;
    }
    pos
}

/// Parses an unsigned integer token starting at `pos`, returning its value and
/// the position after it.  `None` when no digit starts there or it overflows.
fn parse_unsigned(input: &[u8], mut pos: usize) -> Option<(u32, usize)> {
    let start = pos;
    let mut value = 0u32;
    while let Some(&byte) = input.get(pos) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        pos += 1;
    }
    (pos > start).then_some((value, pos))
}

/// Indirect `/Length N G R` references written in one object's bytes.
///
/// A range-backed reader uses this to pre-fetch the length objects a stream
/// dictionary names, because stream decoding happens inside the parser, which
/// only holds an immutable reader and cannot fetch.  A false positive (the
/// token appears in stream payload) costs one harmless extra read.
fn length_references(input: &[u8]) -> Vec<(u32, u16)> {
    fn skip_space(input: &[u8], mut pos: usize) -> usize {
        while matches!(
            input.get(pos),
            Some(b' ' | b'\t' | b'\r' | b'\n' | 0 | 0x0C)
        ) {
            pos += 1;
        }
        pos
    }
    const KEY: &[u8] = b"/Length";
    const MAX_REFS: usize = 8;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while out.len() < MAX_REFS {
        let Some(found) = find_from(input, pos, KEY) else {
            break;
        };
        let after_key = found + KEY.len();
        // `/Length1`, `/Length2` … are different keys.
        if let Some(&next) = input.get(after_key)
            && !(next.is_ascii_whitespace()
                || matches!(next, b'/' | b'[' | b']' | b'<' | b'>' | b'(' | b')' | b'%'))
        {
            pos = after_key;
            continue;
        }
        let mut cursor = skip_space(input, after_key);
        let Some((number, after_number)) = parse_unsigned(input, cursor) else {
            pos = after_key;
            continue;
        };
        cursor = skip_space(input, after_number);
        let Some((generation, after_generation)) = parse_unsigned(input, cursor) else {
            pos = after_key;
            continue;
        };
        cursor = skip_space(input, after_generation);
        if input.get(cursor) == Some(&b'R') && out.len() < MAX_REFS {
            out.push((number, u16::try_from(generation).unwrap_or(u16::MAX)));
        }
        pos = after_key;
    }
    out
}

/// Whether the byte at `pos` can begin a number token: the buffer start, or
/// after whitespace or a delimiter that ended the previous token.
fn starts_token(input: &[u8], pos: usize) -> bool {
    match pos.checked_sub(1).and_then(|prev| input.get(prev)) {
        None => true,
        Some(&byte) => {
            byte.is_ascii_whitespace()
                || matches!(
                    byte,
                    b'>' | b']' | b')' | b'}' | b'<' | b'[' | b'{' | b'(' | b'='
                )
        }
    }
}

/// Advance past a literal string that starts at `pos`, honoring backslash
/// escapes and nested parentheses; returns the index after its closing `)`,
/// or `None` when the string never closes.
fn skip_literal_string(input: &[u8], mut pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    while pos < input.len() {
        match input[pos] {
            b'\\' => pos = pos.saturating_add(2),
            b'(' => {
                depth += 1;
                pos += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                pos += 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
            _ => pos += 1,
        }
    }
    None
}

/// Advance past a hex string that starts at `pos` (a `<` that is not the
/// start of `<<`); returns the index after the closing `>`, or `None` when
/// the string never closes.
fn skip_hex_string(input: &[u8], mut pos: usize) -> Option<usize> {
    pos += 1;
    while pos < input.len() {
        if input[pos] == b'>' {
            return Some(pos + 1);
        }
        pos += 1;
    }
    None
}

/// Advance past whitespace and `%` comments (to end of line), returning the
/// first index that starts neither (or `input.len()`).
fn skip_whitespace_and_comments(input: &[u8], mut pos: usize) -> usize {
    loop {
        pos = skip_whitespace(input, pos);
        if input.get(pos) != Some(&b'%') {
            return pos;
        }
        while pos < input.len() && !matches!(input[pos], b'\r' | b'\n') {
            pos += 1;
        }
    }
}

/// Read the run of ASCII digits at `pos` as `N`, together with the index just
/// past it. An empty or unparsable run is a malformed file, not a short read.
fn scan_number<N: std::str::FromStr>(input: &[u8], mut pos: usize) -> Result<(N, usize)> {
    let start = pos;
    while pos < input.len() && input[pos].is_ascii_digit() {
        pos += 1;
    }

    let value = std::str::from_utf8(&input[start..pos])
        .ok()
        .and_then(|digits| digits.parse().ok())
        .ok_or(Error::Parse(ParseError::InvalidXref))?;

    Ok((value, pos))
}

/// The first index at or after `start` where `pattern` begins, or `None`.
fn find_from(input: &[u8], start: usize, pattern: &[u8]) -> Option<usize> {
    // `endstream`/`endobj`/`/Length` scans are the only whole-file byte
    // searches on the load path; memchr's SIMD prefilter replaces the naive
    // window-by-window comparison.
    memchr::memmem::find(input.get(start..)?, pattern).map(|pos| start + pos)
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn load_document() {
    let mut doc = Document::load("assets/example.pdf").unwrap();
    assert_eq!(doc.version, "1.5");

    // Create temporary folder to store file.
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("test_2_load.pdf");
    doc.save(file_path).unwrap();
}

#[cfg(all(test, feature = "async"))]
#[tokio::test]
async fn load_document() {
    let mut doc = Document::load("assets/example.pdf").await.unwrap();
    assert_eq!(doc.version, "1.5");

    // Create temporary folder to store file.
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("test_2_load.pdf");
    doc.save(file_path).unwrap();
}

#[test]
#[should_panic(expected = "Xref(Start)")]
fn load_short_document() {
    let _doc = Document::load_mem(b"%PDF-1.5\n%%EOF\n").unwrap();
}

#[test]
fn load_document_with_preceding_bytes() {
    let mut content = Vec::new();
    content.extend(b"garbage");
    content.extend(include_bytes!("../assets/example.pdf"));
    let doc = Document::load_mem(&content).unwrap();
    assert_eq!(doc.version, "1.5");
}

#[test]
fn load_many_shallow_brackets() {
    let content: String = std::iter::repeat_n("()", MAX_BRACKET * 10)
        .flat_map(|x| x.chars())
        .collect();
    const STREAM_CRUFT: usize = 33;
    let doc = format!(
        "%PDF-1.5
1 0 obj<</Type/Pages/Kids[5 0 R]/Count 1/Resources 3 0 R/MediaBox[0 0 595 842]>>endobj
2 0 obj<</Type/Font/Subtype/Type1/BaseFont/Courier>>endobj
3 0 obj<</Font<</F1 2 0 R>>>>endobj
5 0 obj<</Type/Page/Parent 1 0 R/Contents[4 0 R]>>endobj
6 0 obj<</Type/Catalog/Pages 1 0 R>>endobj
4 0 obj<</Length {}>>stream
BT
/F1 48 Tf
100 600 Td
({}) Tj
ET
endstream endobj\n",
        content.len() + STREAM_CRUFT,
        content
    );
    let doc = format!(
        "{}xref
0 7
0000000000 65535 f 
0000000009 00000 n 
0000000096 00000 n 
0000000155 00000 n 
0000000291 00000 n 
0000000191 00000 n 
0000000248 00000 n 
trailer
<</Root 6 0 R/Size 7>>
startxref
{}
%%EOF",
        doc,
        doc.len()
    );

    let _doc = Document::load_mem(doc.as_bytes()).unwrap();
}

#[test]
fn load_too_deep_brackets() {
    let content: Vec<u8> = std::iter::repeat_n(b'(', MAX_BRACKET + 1)
        .chain(std::iter::repeat_n(b')', MAX_BRACKET + 1))
        .collect();
    let content = String::from_utf8(content).unwrap();
    const STREAM_CRUFT: usize = 33;
    let doc = format!(
        "%PDF-1.5
1 0 obj<</Type/Pages/Kids[5 0 R]/Count 1/Resources 3 0 R/MediaBox[0 0 595 842]>>endobj
2 0 obj<</Type/Font/Subtype/Type1/BaseFont/Courier>>endobj
3 0 obj<</Font<</F1 2 0 R>>>>endobj
5 0 obj<</Type/Page/Parent 1 0 R/Contents[7 0 R 4 0 R]>>endobj
6 0 obj<</Type/Catalog/Pages 1 0 R>>endobj
7 0 obj<</Length 45>>stream
BT /F1 48 Tf 100 600 Td (Hello World!) Tj ET
endstream
endobj
4 0 obj<</Length {}>>stream
BT
/F1 48 Tf
100 600 Td
({}) Tj
ET
endstream endobj\n",
        content.len() + STREAM_CRUFT,
        content
    );
    let doc = format!(
        "{}xref
0 7
0000000000 65535 f 
0000000009 00000 n 
0000000096 00000 n 
0000000155 00000 n 
0000000387 00000 n 
0000000191 00000 n 
0000000254 00000 n 
0000000297 00000 n 
trailer
<</Root 6 0 R/Size 7>>
startxref
{}
%%EOF",
        doc,
        doc.len()
    );

    let doc = Document::load_mem(doc.as_bytes()).unwrap();
    let pages = doc.get_pages().keys().cloned().collect::<Vec<_>>();
    assert_eq!("Hello World!\n", doc.extract_text(&pages).unwrap());
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn search_substring_finds_last_occurrence() {
    assert_eq!(Reader::search_substring(b"hello world", b"xyz", 0), None);
    assert_eq!(
        Reader::search_substring(b"hello world", b"world", 0),
        Some(6)
    );

    let buffer = b"%%EOF\ntest%%EOF\nend";
    assert_eq!(Reader::search_substring(buffer, b"%%EOF", 0), Some(10));
    assert_eq!(Reader::search_substring(buffer, b"%%EOF", 6), Some(10));
    assert_eq!(Reader::search_substring(buffer, b"%%EOF", 15), None);
    assert_eq!(Reader::search_substring(b"%%EOF", b"%%EOF", 0), Some(0));

    let buffer_with_many_percents = b"%%%PDF-1.3%%%comment%%%more%%EOF";
    assert_eq!(
        Reader::search_substring(buffer_with_many_percents, b"%%EOF", 0),
        Some(27)
    );
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn get_xref_start_ignores_startxref_past_eof() {
    // Two revisions, the second with a corrupted %%EOF: the startxref *before* the valid
    // %%EOF must win over the one after it.
    let mut buf = Vec::new();
    // Padding so the buffer is large enough
    buf.extend_from_slice(&[b' '; 200]);
    // First (correct) startxref + xref offset + valid %%EOF
    let xref_offset = 100usize;
    let startxref_block = format!("startxref\n{}\n%%EOF\n", xref_offset);
    let _startxref_pos = buf.len();
    buf.extend_from_slice(startxref_block.as_bytes());
    // Second (corrupted) revision: another startxref pointing elsewhere, with %%EO\0
    let bad_block = "startxref\n999\n%%EO\x00\n".to_string();
    buf.extend_from_slice(bad_block.as_bytes());

    let result = Reader::get_xref_start(&buf).unwrap();
    // Should find the xref_offset from the first startxref (before valid %%EOF)
    assert_eq!(result, xref_offset);
    // Verify it did NOT pick up 999 from the corrupted revision
    assert_ne!(result, 999);
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn get_xref_start_tolerates_data_after_eof() {
    let mut doc = Document::load("assets/example.pdf").unwrap();
    // A file with only a cross-reference stream has no `trailer` keyword, so reconstruction
    // cannot recover it when `startxref` is not found.
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceStream;
    let mut buffer = Vec::new();
    doc.save_to(&mut buffer).unwrap();
    let xref_start = Reader::get_xref_start(&buffer).unwrap();

    // NUL and white-space padding of any length.
    let mut padded = buffer.clone();
    padded.extend_from_slice(&[0; 20_000]);
    padded.extend_from_slice(b"\r\n \x0C\t");
    assert_eq!(Reader::get_xref_start(&padded).unwrap(), xref_start);

    let options = LoadOptions {
        strict: true,
        ..Default::default()
    };
    let loaded = Document::load_mem_with_options(&padded, options).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);

    // Other trailing data, as long as `%%EOF` is within the last 1024 bytes.
    let mut trailing = buffer.clone();
    trailing.extend_from_slice(&[b'x'; 1000]);
    assert_eq!(Reader::get_xref_start(&trailing).unwrap(), xref_start);
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn recovers_from_miswritten_startxref_offset() {
    // Some generators point `startxref` at the line *after* the `xref` keyword: lenient
    // loading must recover, strict loading must still reject.
    let header = "%PDF-1.5\n";
    let obj1 = "1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n";
    let obj2 = "2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n";
    let o1 = header.len();
    let o2 = o1 + obj1.len();
    let body = format!("{header}{obj1}{obj2}");
    let xref_pos = body.len();
    let doc = format!(
        "{body}xref
0 3
0000000000 65535 f 
{o1:010} 00000 n 
{o2:010} 00000 n 
trailer
<</Root 1 0 R/Size 3>>
startxref
{}
%%EOF",
        xref_pos + 4 // past the `xref` keyword, onto the line after it
    );

    let loaded = Document::load_mem(doc.as_bytes()).unwrap();
    assert!(loaded.trailer.get(b"Root").is_ok());
    assert_eq!(loaded.xref_start, xref_pos);

    let strict = Document::load_mem_with_options(
        doc.as_bytes(),
        LoadOptions {
            strict: true,
            ..Default::default()
        },
    );
    assert!(strict.is_err());
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn recovers_from_miswritten_startxref_offset_of_a_cross_reference_stream() {
    // Without an `xref` keyword nearby, the header of the cross-reference stream is used,
    // whether `startxref` stops short of it or points into it.
    let mut document = Document::load("assets/example.pdf").unwrap();
    document.reference_table.cross_reference_type = XrefType::CrossReferenceStream;
    let mut buffer = Vec::new();
    document.save_to(&mut buffer).unwrap();
    let xref_start = Reader::get_xref_start(&buffer).unwrap();
    let keyword = buffer
        .windows(9)
        .rposition(|window| window == b"startxref")
        .unwrap();
    let with_startxref = |offset: usize| {
        let mut damaged = buffer[..keyword].to_vec();
        damaged.extend_from_slice(format!("startxref\n{offset}\n%%EOF").as_bytes());
        damaged
    };

    for offset in [xref_start - 2, xref_start + 3] {
        let damaged = with_startxref(offset);
        let loaded = Document::load_mem(&damaged).unwrap();
        assert_eq!(loaded.xref_start, xref_start);
        assert_eq!(loaded.get_pages().len(), 1);

        let strict = Document::load_mem_with_options(
            &damaged,
            LoadOptions {
                strict: true,
                ..Default::default()
            },
        );
        assert!(strict.is_err());
    }

    // The parser skips white-space before the header, so that offset is kept.
    let loaded = Document::load_mem(&with_startxref(xref_start - 1)).unwrap();
    assert_eq!(loaded.xref_start, xref_start - 1);
    assert_eq!(loaded.get_pages().len(), 1);
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn rebuilds_rather_than_correcting_to_a_table_of_shifted_objects() {
    // The table and `startxref` were written for a file with 10 more bytes before the
    // objects, as when bytes are later removed. The nearby `xref` keyword leads to a table
    // whose offsets all miss their objects; scanning finds the objects where they are.
    let header = "%PDF-1.5\n";
    let obj1 = "1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n";
    let obj2 = "2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n";
    let obj3 = "3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\n";
    let removed = 10;
    let o1 = header.len() + removed;
    let o2 = o1 + obj1.len();
    let o3 = o2 + obj2.len();
    let body = format!("{header}{obj1}{obj2}{obj3}");
    let doc = format!(
        "{body}xref
0 4
0000000000 65535 f 
{o1:010} 00000 n 
{o2:010} 00000 n 
{o3:010} 00000 n 
trailer
<</Root 1 0 R/Size 4>>
startxref
{}
%%EOF",
        body.len() + removed
    );

    let loaded = Document::load_mem(doc.as_bytes()).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);
    // Zero marks a table rebuilt by scanning rather than read at a corrected offset.
    assert_eq!(loaded.xref_start, 0);
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn stream_length_written_as_real() {
    // "/Length 42." must still resolve to the stream content rather than load as empty.
    let obj4 = "4 0 obj\n<< /Length 42. >>\nstream\nBT /F1 12 Tf 20 100 Td (Hello World) Tj ET\nendstream\nendobj\n";
    let header = "%PDF-1.7\n";
    let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
    let obj2 = "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n";
    let obj3 = "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj\n";
    let o1 = header.len();
    let o2 = o1 + obj1.len();
    let o3 = o2 + obj2.len();
    let o4 = o3 + obj3.len();
    let body = format!("{header}{obj1}{obj2}{obj3}{obj4}");
    let xref_pos = body.len();
    let doc = format!(
        "{body}xref
0 5
0000000000 65535 f 
{o1:010} 00000 n 
{o2:010} 00000 n 
{o3:010} 00000 n 
{o4:010} 00000 n 
trailer
<< /Size 5 /Root 1 0 R >>
startxref
{xref_pos}
%%EOF
"
    );

    let loaded = Document::load_mem(doc.as_bytes()).unwrap();
    let stream = loaded.get_object((4, 0)).unwrap().as_stream().unwrap();
    assert_eq!(
        stream.content,
        b"BT /F1 12 Tf 20 100 Td (Hello World) Tj ET"
    );
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn object_marker_parsing_accepts_only_real_headers() {
    assert_eq!(Reader::parse_object_header(b"12 0 obj<<"), Some((12, 0)));
    assert_eq!(Reader::parse_object_header(b"007 0 obj\n"), Some((7, 0)));
    assert_eq!(Reader::parse_object_header(b"5 2 obj>>"), Some((5, 2)));
    assert_eq!(Reader::parse_object_header(b"12\t3\r\nobj["), Some((12, 3)));
    assert_eq!(Reader::parse_object_header(b"2 0 objects"), None);
    assert_eq!(Reader::parse_object_header(b"99 88 objx"), None);
    assert_eq!(Reader::parse_object_header(b"12345678901 0 obj"), None);
    assert_eq!(Reader::parse_object_header(b"1 70000 obj"), None);
    assert_eq!(Reader::parse_object_header(b"1 0obj"), None);
    assert_eq!(Reader::parse_object_header(b"1  obj"), None);
    assert_eq!(Reader::parse_object_header(b"x 0 obj"), None);
}

/// A hybrid-reference file: the page tree lives in an object stream that only the
/// cross-reference stream at `/XRefStm` lists. Returns the file and the offset of its
/// `xref` table.
#[cfg(all(test, not(feature = "async")))]
fn hybrid_reference_file() -> (Vec<u8>, usize) {
    let header = "%PDF-1.5\n";
    let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
    let pages = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> >>";
    let offsets = format!("2 0 3 {} ", pages.len() + 1);
    let content = format!("{offsets}{pages} {page}");
    let obj4 = format!(
        "4 0 obj\n<< /Type /ObjStm /N 2 /First {} /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
        offsets.len(),
        content.len(),
    );
    let o1 = header.len();
    let o4 = o1 + obj1.len();
    let o5 = o4 + obj4.len();

    // Entries for objects 2 to 5 with /W [1 2 1].
    let [o4_hi, o4_lo] = u16::try_from(o4).unwrap().to_be_bytes();
    let [o5_hi, o5_lo] = u16::try_from(o5).unwrap().to_be_bytes();
    let entries = [
        2, 0, 4, 0, 2, 0, 4, 1, 1, o4_hi, o4_lo, 0, 1, o5_hi, o5_lo, 0,
    ];
    let mut buffer = format!("{header}{obj1}{obj4}").into_bytes();
    buffer.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XRef /Size 6 /Index [2 4] /W [1 2 1] /Length {} >>\nstream\n",
            entries.len()
        )
        .as_bytes(),
    );
    buffer.extend_from_slice(&entries);
    buffer.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_pos = buffer.len();
    buffer.extend_from_slice(
        format!(
            "xref
0 2
0000000000 65535 f 
{o1:010} 00000 n 
trailer
<< /Size 6 /Root 1 0 R /XRefStm {o5} >>
startxref
{xref_pos}
%%EOF
"
        )
        .as_bytes(),
    );
    (buffer, xref_pos)
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn reads_xref_stream_of_hybrid_reference_file() {
    let (buffer, _) = hybrid_reference_file();

    let loaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);
    // The offset is meaningless once the document is written again.
    assert!(!loaded.trailer.has(b"XRefStm"));
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn reads_xref_stream_of_earlier_hybrid_reference_section() {
    // An incremental update with a plain `xref` table on top of a hybrid-reference file.
    let (mut buffer, prev) = hybrid_reference_file();
    let obj1 = buffer.len();
    buffer.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Lang (en) >>\nendobj\n");
    let xref_pos = buffer.len();
    buffer.extend_from_slice(
        format!(
            "xref
1 1
{obj1:010} 00000 n 
trailer
<< /Size 6 /Root 1 0 R /Prev {prev} >>
startxref
{xref_pos}
%%EOF
"
        )
        .as_bytes(),
    );

    let loaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);
    assert!(loaded.catalog().unwrap().has(b"Lang"));
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn saving_a_reconstructed_hybrid_reference_file_writes_no_stale_xref_stm() {
    // With `startxref` past the end, the file is rebuilt by scanning, and its trailer
    // still names the cross-reference stream by an offset into the damaged file.
    let (mut buffer, xref_pos) = hybrid_reference_file();
    let tail = format!("startxref\n{xref_pos}\n%%EOF\n");
    buffer.truncate(buffer.len() - tail.len());
    buffer.extend_from_slice(format!("startxref\n{}\n%%EOF\n", buffer.len() + 4096).as_bytes());

    let mut loaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);
    assert!(!loaded.trailer.has(b"XRefStm"));

    let mut saved = Vec::new();
    loaded.save_to(&mut saved).unwrap();
    let strict = LoadOptions {
        strict: true,
        ..Default::default()
    };
    let reloaded = Document::load_mem_with_options(&saved, strict).unwrap();
    assert_eq!(reloaded.get_pages().len(), 1);
}

#[cfg(all(test, not(feature = "async")))]
#[test]
fn ignores_unreadable_hybrid_xref_stream_when_lenient() {
    // `/XRefStm` pointing at an object that is not a cross-reference stream.
    let header = "%PDF-1.5\n";
    let obj1 = "1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n";
    let obj2 = "2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n";
    let o1 = header.len();
    let o2 = o1 + obj1.len();
    let body = format!("{header}{obj1}{obj2}");
    let xref_pos = body.len();
    let doc = format!(
        "{body}xref
0 3
0000000000 65535 f 
{o1:010} 00000 n 
{o2:010} 00000 n 
trailer
<</Root 1 0 R/Size 3/XRefStm {o2}>>
startxref
{xref_pos}
%%EOF"
    );

    let loaded = Document::load_mem(doc.as_bytes()).unwrap();
    // Read through the table rather than by reconstructing the cross-reference table.
    assert_eq!(loaded.xref_start, xref_pos);
    assert!(loaded.get_object((2, 0)).is_ok());

    let strict = Document::load_mem_with_options(
        doc.as_bytes(),
        LoadOptions {
            strict: true,
            ..Default::default()
        },
    );
    assert!(strict.is_err());
}
