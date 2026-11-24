# Torcular changes to vendor/lopdf

Upstream: J-F-Liu/lopdf, pinned at the commit in `../../refs.lock`
(`rust/lopdf`).

- `src/lib.rs`: re-export `encodings::cmap::ToUnicodeCMap` so the syntax facade
  can store an owned ToUnicode map without exposing the private module path.
- `src/encodings/cmap.rs`: make `ToUnicodeCMap::parse` public so the same facade
  can parse `/ToUnicode` when a font also has an `/Encoding` dictionary.
  No semantic change.

- `src/object.rs`: add `Stream::decompressed_before_image_filter` so an image
  with a filter chain such as ASCIIHex -> Flate -> DCT can hand the JPEG bytes
  to the image decoder instead of failing at the terminal codec.

- `src/object.rs`: apply per-filter `/DecodeParms` predictors after
  `BrotliDecode`.  Array-form parameters are already associated with their own
  filter; ignoring them left PNG-predicted image rows encoded and produced
  wrong colours.
- `src/parser/mod.rs`: `header` additionally accepts a `%PDF` header line
  with no dash and no version digits (and `%PDF-` with empty digits),
  defaulting to `1.7`, matching pdfium's tolerance for damaged headers; xref
  reconstruction then runs as usual.  Strict mode keeps rejecting a bare
  `%PDF`.  No change for well-formed files.

- `src/outlines.rs`: the `/Next` sibling walk keeps a visited object-id set
  (seeded with the first item) and stops on a repeated edge.  A crafted cyclic
  outline chain previously hung `document_outlines`; pdfium
  `cpdf_bookmarktree.cpp` and pdf.js `catalog.js` both guard this.  No change
  for well-formed trees.

- `src/outlines.rs`, `src/destinations.rs`: `/Names /Dests` is consulted
  before the legacy catalog `/Dests` dictionary, and direct destination
  arrays in a name tree are accepted.  Outline destinations may also be name
  objects.  pdfium `cpdf_nametree.cpp` and pdf.js `catalog.js` both prefer
  the name tree; no change for files that only use one form.

- `src/parser/mod.rs`: inline images now expand the abbreviated dictionary
  keys (`W`, `H`, `BPC`, `CS`, `F`, `DP`, `D`, `I`, `IM`), map abbreviated
  filter/colour-space names to their long forms, and support filtered inline
  data by scanning for the whitespace-framed `EI` marker.  Previously any
  `/Filter` made the parser skip the image.  `torcular-content` paints the
  resulting stream through the normal image path.

- `src/document.rs`: the page-tree iterator keeps typeless leaf dictionaries
  and `/Pages` nodes without `/Kids` as page slots, skips a self-referencing
  `/Pages` kid via a visited set, and keeps unloadable kids as blank
  stand-ins.  Inline page dictionaries and inline nested `/Pages` nodes are
  materialised as indirect objects at load (`materialize_inline_pages`), so
  the iterator yields an id for every leaf; encrypted files keep the old
  behaviour because their dictionaries are decrypted in place.

- `src/encodings/cmap.rs`: a multi-unit `bfrange` destination is incremented
  as a big-endian base-65536 sequence, so a carry advances the preceding
  UTF-16 unit instead of wrapping only the last one (pdfium `StringDataAdd`,
  `cpdf_tounicodemap.cpp:31-47`).  A surrogate pair such as `<D83DFFFF>`
  now yields `<D83E0000>` for the next code rather than a lone low surrogate.

- `src/outlines.rs`, `src/destinations.rs`: a bookmark with a non-GoTo action
  (URI, Launch, JavaScript, Named, ...) is returned as a destination carrying
  an `/Action` name instead of being rejected, and `/Count`, `/F` and `/C`
  are copied onto every destination.  Named destinations are cloned rather
  than mutated in place, so two bookmarks sharing one name keep their own
  style.

- `src/reader.rs`: the `/Prev` walk now copies every key missing from the
  newest trailer, so `/Root`, `/Info`, `/Encrypt` and `/ID` survive an
  incremental update that omits them; newer keys still win (pdfium
  `CPDF_CrossRefTable::UpdateTrailer`).

- `src/encryption/crypt_filters.rs`: AES-CBC decryption now fails soft.  A
  payload shorter than the IV returns empty, a partial trailing block is
  ignored, and invalid PKCS#5 padding keeps the decrypted block (dropping a
  plausible 1..15 tail) instead of returning an error that left the object
  as ciphertext (pdfium `DecryptFinish`, cpdf_crypto_handler.cpp:211-233).

- `src/parser/mod.rs`, `src/parser_aux.rs`, `src/xref.rs`: free xref entries
  from tables and cross-reference streams are now retained, so a newer free
  entry shadows an older live object instead of resurrecting it.  `Xref::merge`
  keeps the newer section but lets a higher generation win between two live
  entries (pdfium `CPDF_CrossRefTable`).

- `src/reader.rs`: when xref resolution fails, recovery now also inspects each
  `/Type /XRef` stream dictionary for a usable `/Root` instead of requiring a
  `trailer` keyword, so cross-reference-stream-only files recover.

- `src/incremental_document.rs`, `src/writer.rs`, `src/xref.rs`: `XrefEntry::Free`
  carries its generation; `IncrementalDocument::delete_object` records the
  removal and an incremental save appends a free entry with generation + 1,
  so the object is not resolvable through `/Prev`.

- `src/encryption.rs`: a dictionary recognised as a signature (`/Type` or `/FT`
  `Sig`/`DocTimeStamp`, or `/ByteRange` with `/Contents`) keeps its `/Contents`
  untouched during encryption and decryption, so the PKCS#7 blob stays
  byte-identical (pdfium `DecryptObjectTree`).

- `src/parser/cmap_parser.rs`, `src/encodings/cmap.rs`: a ToUnicode CMap is
  now parsed section by section.  A `bfchar`/`bfrange` section whose declared
  count does not match its entries (or that contains a malformed line) is
  dropped on its own; the surrounding sections survive, matching pdfium
  `HandleBeginBFChar`/`HandleBeginBFRange` (cpdf_tounicodemap.cpp:180-259).
  Later duplicate codes replace earlier ones.  `usecmap` names an Adobe UCS2
  collection, and codes the bf sections omit resolve through it
  (cpdf_tounicodemap.cpp:50-63,154-173); the four pdf.js binary CMaps are
  vendored under `src/encodings/data/`.  Previously one bad section abandoned
  the whole map and `usecmap` was not understood.

- `src/document.rs`, `src/encryption.rs`: password probing now matches the
  standard handler.  A non-empty credential is tried as the owner password
  first and then as the user password; the empty credential is only ever a
  user password, and `UnlockOwner` refuses it.  `EncryptionState` records
  whether the owner unlocked the document; `permissions()` reports the
  standard-handler normalised `/P` (`& 0xFFFFFFFC | 0xFFFFF0C0`) and all
  permissions for an owner authentication (pdfium `CheckSecurity` /
  `GetPermissions`, cpdf_security_handler.cpp:213-233,467-497).

- `src/encryption.rs`, `src/object.rs`: a stream whose `/Filter` chain contains
  `/Crypt` now uses the crypt filter named by that filter's own
  `/DecodeParms` entry (dictionary or per-index array element); a missing,
  unnamed or unknown name means Identity, overriding the document default.
  After decryption the `Crypt` entry (and its parameters) is removed from the
  chain, so the remaining filters decode the plaintext; a stray `Crypt` filter
  in an unencrypted stream is a no-op decoder instead of an
  "unimplemented" error.  `/EFF` now selects the crypt filter for
  `/Type /EmbeddedFile` streams, and V4/V5 `/StmF`/`/StrF` default to Identity
  when absent (pdf.js parser.js:728-735, crypto.js:824-831,1305-1352).

- `src/parser/mod.rs`: stream length recovery accepts the first whole-word
  `endstream`, pdf.js's misspellings `endsteam`/`endstrea`, or a bare `endobj`
  when `endstream` is missing, still bounded by the next xref-derived object
  start.  A negative or out-of-range `/Length` now falls back to the same scan
  instead of aborting the object, and a stream with no terminator inside its
  bound stays empty rather than swallowing the neighbour (pdfium
  `FindStreamEndPos`/`ReadStream`, cpdf_syntax_parser.cpp:742-810; pdf.js
  parser.js:616-737).

- `src/reader.rs`: xref recovery no longer accepts a `trailer` lookalike
  embedded in a stream payload (`stream_payload_ranges` skips payload bytes),
  and a candidate `/Root` must resolve to a catalog dictionary whose `/Pages`
  resolves to a page-tree dictionary.  A forged trailer can no longer select
  an unrelated object as the recovered root (pdfium `RebuildCrossRef`,
  cpdf_parser.cpp:809-859; pdf.js xref.js:557-587).

- `src/object_stream.rs`, `src/reader.rs`: a type-2 xref entry now selects the
  member at its declared index, and that member's object number must equal the
  requested one; a corrupt index no longer falls back to a same-numbered
  member elsewhere in the stream.  Compressed-object containers must be
  `/Type /ObjStm` with integer `/N` and `/First`, so an arbitrary stream is
  never parsed as an object table (pdfium `CPDF_ObjectStream::ParseObject` /
  `IsObjectStream`, cpdf_object_stream.cpp:23-101).

- `src/writer.rs`: incremental saves now measure the appended cross-reference
  section and object offsets from the `%PDF-` header, the base the reader
  uses after skipping junk before the header (pdfium `GetHeaderOffset`,
  fpdf_parser_utility.cpp:79-94; pdf.js `checkHeader`/`xref.js:753`).  A file
  with a junk prefix previously got a `startxref` and object offsets that were
  `prefix_len` bytes too large, so the appended revision was unreadable.
  `/Prev` already used the header-relative `xref_start`.  A previous revision
  whose xref was reconstructed by scanning (`xref_start == 0`) cannot be
  chained: `IncrementalDocument::save_internal` now refuses with
  `ErrorKind::Unsupported` instead of appending without `/Prev` and dropping
  every unchanged object.  `IncrementalDocument::new()` (no previous bytes)
  is unaffected.

- `src/object.rs`, `src/filters/png.rs`, `src/error.rs`: `/Filter` chains stop
  at the first image codec and return the bytes decoded so far, matching
  pdfium `PDF_DataDecode` (fpdf_parser_decode.cpp:462-533) and pdf.js
  (parser.js:747-793).  A non-terminal filter must be a general-purpose
  decoder (Flate/Fl/LZW/ASCII85/ASCIIHex/RunLength/Brotli/Crypt), as in
  pdfium `ValidateDecoderPipeline` (:80-113); a decoder after an image codec
  now fails with `DecompressError::FilterChain` instead of being silently
  ignored.  `decompress()`/`decompress_inner` leaves a stream that ends in an
  image codec untouched, because dropping the codec from `/Filter` would make
  the partially decoded bytes look like plain samples.

- `src/object.rs`: predictor parameters must be non-negative (pdfium
  `GetAndCheckDecodeParams`, fpdf_parser_decode.cpp:114-136), their row size
  is capped at pdfium's `INT_MAX - 7` before a buffer is sized, and predictor
  values other than 1, 2 and 10-15 are rejected (pdf.js
  predictor_stream.js:27-46) instead of being ignored.  `filters/png.rs`
  rejects a row wider than the decoded data before allocating the two row
  buffers, so `/Columns` cannot request a multi-gigabyte allocation.

- `src/object_stream.rs`, `src/reader.rs`: the eager bulk loader now keeps
  the declared index of each member and requires the merged xref to select
  that member at that index, so a type-2 entry cannot be satisfied by a
  same-numbered member at another position (pdfium
  `CPDF_ObjectStream::ParseObject`, cpdf_object_stream.cpp:82-101).  The
  encrypted load path uses `member_at(index)` for the same check, only the
  first `/N` index pairs are addressable (`Init`, :104-120), and a negative
  `/N` or `/First` rejects the container (`IsObjectStream`, :26-52).

- `src/filters/png.rs`, `src/parser_aux.rs`: a predictor-decoded body whose
  final row is cut short now decodes the bytes that are present instead of
  dropping the row, matching pdfium `fxcodec/image_predictors.cpp`
  `PngPredictor` (which shrinks the last output row by the remainder).  This
  matters for cross-reference streams: on
  `testing/corpus/fx/FRC_8.2.4_part1/FRC_1_8.2.4__original.pdf` the last
  three bytes of the final 6-byte xref entry exist only in the cut-short row.
  The xref decoder no longer rejects a stream whose `/Index` declares more
  records than the decoded body holds; it resolves the complete leading
  records and leaves the rest unresolved (pdfium skips the whole `/Index`
  segment and later rebuilds, pdf.js throws).  Row buffers stay sized from the
  input, not from `/Columns`.

- `src/reader.rs`, `src/parser/mod.rs`: cross-reference reconstruction now
  ignores comments and literal/hex strings while scanning for `N G obj`
  headers, accepts a header anywhere a number token can start (damaged
  producers concatenate `endobj 2 0 obj` and `>>2 0 obj` without line breaks),
  bounds an unterminated stream at its `/Length` or `endobj` so later objects
  and the real trailer stay reachable, and accepts a comment between
  `trailer` and its dictionary (`trailer % The trailer part.`).  This matches
  pdfium `CPDF_Parser::RebuildCrossRef` (cpdf_parser.cpp:785-867) and fixes
  `2_halftone.pdf`, `jetman_std.pdf`, `jetman_std_fixed.pdf` and
  `bug_511_jbig_null_crash.pdf`.  The object parser also ends an array at the
  enclosing `>>`/`endobj` without consuming it when `]` is missing, as
  pdfium's loose `GetObjectBodyInternal` does, so `/Kids[8 0 R>>` still yields
  a readable page tree.

- `src/document.rs`, `src/encryption.rs`, `src/reader.rs`: a file whose
  effective crypt filters are Identity (`/V` 4/5 with `/StmF` and `/StrF`
  both `/Identity`) loads without a password check, as pdfium
  `CPDF_SecurityHandler::OnInit` returns success when the cipher is `kNone`
  (cpdf_security_handler.cpp:199-204).  Page content and strings stay
  plaintext; `/EFF` still selects the embedded-file filter; the document
  records `was_encrypted()` so the writer continues to refuse it.  Fixes
  `FRC_3.5_AuthEvent_EFOpen.pdf` and
  `FRC_3.5_CF_EFF_StdCF_Strf_Stmf_Identity.pdf`.


- `src/reader.rs`: the buffer is now `Cow<'a, [u8]>`, so a reader can own its
  bytes (`Reader::owned_with_options`) without a self-referential borrow.
  `Reader::read_shell` resolves the header/xref/trailer and loads only the
  catalog, page-tree nodes and page dictionaries; other objects stay on
  demand through the public `Reader::get_object`.  This is the browser's
  lazy-open path (`open_lazy`): a 54 MB PDF opens at +99 MB RSS instead of
  +212 MB, and page content/fonts/images are fetched when a page is first
  rendered.  Encrypted documents are rejected by `read_shell` and fall back
  to the eager loader.

- `src/parser/mod.rs`: stream `/Length` recovery is now spec-strict (ISO
  32000-1 7.3.8.2).  Recovery runs only when a present `/Length` fails to
  delimit the payload, and accepts exactly one EOL-framed `endstream` that is
  followed by `endobj`; a missing, duplicated or truncated terminator drops
  the object (strict mode aborts) instead of returning an empty stream.
  Restores the semantics of upstream 675f451/#568;
  `tests/stream_length_recovery.rs` is 9/9.

- `src/object.rs`, `src/creator.rs`: new `MAX_OBJECT_NUMBER = 0x7FFF_FFFE`,
  one below ISO 32000-2 7.3.10's `2^31 - 1` because `/Size = max_id + 1`
  must still be a representable PDF integer.  Object headers and references
  above it do not parse, `Document::try_new_object_id` / `try_add_object`
  return `Error::ObjectIdOverflow`, and `add_object` / `new_object_id`
  panic with the same message rather than wrapping.

- `src/object_store.rs`, `src/xref.rs`: object and cross-reference tables are
  now a dense `Vec` below `1 << 20` plus a `BTreeMap` above it.  A high
  object number costs one entry instead of an allocation proportional to the
  id; `len`, generations, ascending iteration and `IntoIterator` (including
  rayon) keep their previous semantics.

- `src/writer.rs`: cross-reference sections are built from the actual entries
  (upstream 01d9bfb) instead of walking `1..=max_id`, so a sparse
  high-numbered document saves quickly.  The cross-reference stream takes a
  checked object number and `/Size` is computed in i64.

- `tests/save_options_xref_streams_test.rs`: the high-id round-trip uses
  `MAX_OBJECT_NUMBER - 1` (leaving one number for the xref stream) and
  asserts the gap is skipped.  New tests reject `0x8000_0000` and
  `u32::MAX - 2`, and check that the last legal number saves as a classic
  table but fails cleanly as an xref stream.

- `src/writer.rs`: `Object::Real` is written with a decimal point (`5.0`,
  `0.0`, `-0.0`), as ISO 32000-1 7.3.3 requires, so a whole-number real
  reloads as a real instead of an `Object::Integer`.  The digits come from
  `Display` (shortest round-trip form, never an exponent); NaN and the
  infinities keep their previous spelling.  Loading and saving no longer
  change a real's type.  `tests/real_point.rs` is 6/6.

- `src/writer.rs`: `save`, `save_to`, `save_with_options` and
  `IncrementalDocument::save_to` no longer change `Document::trailer` or
  `Document::max_id`.  The `/Size`, the cross-reference stream keys and the
  `/Encrypt` entry a save needs are written into a copy of the trailer, and
  object streams and the cross-reference stream take their numbers from a
  counter copied from `max_id`.  Saving twice writes the same file, and
  objects added after a save take the next number.  The PDF version is still
  raised to 1.5 when the output needs it (`tests/xref_version.rs`).
  `tests/save_keeps_document_state.rs` is 7/7.
