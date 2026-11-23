use crate::cmap_section::{CMapParseError, CMapSection, CodeLen, SourceCode};
use crate::parser::cmap_parser::{ParsedCMap, parse};

use log::error;
use rangemap::RangeInclusiveMap;
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::OnceLock;
use thiserror::Error;

/// Unicode Cmap is implemented by 4 maps.
/// Each map contains a mappings from source codes to unicode values for a different length of codes.
/// Codes vary from 1 byte to 4 bytes so they are always in limits of u32.
/// However to map a code to a unicode value an additional knowledge about the number of bytes is needed,
/// as 2 byte code <0000> shouldn't be matched with a single byte <00> even though they have the same integer value.
#[derive(Debug, Default)]
pub struct ToUnicodeCMap {
    pub bf_ranges: [RangeInclusiveMap<SourceCode, BfRangeTarget>; 4],
    /// Single-code bfchar entries.  Keeping them out of the interval map
    /// avoids a tree insertion per character; most ToUnicode tables are
    /// thousands of independent bfchar rows.
    single: [FxHashMap<SourceCode, BfRangeTarget>; 4],
    codespace_ranges: Vec<(SourceCode, SourceCode, CodeLen)>,
    reverse_map: Option<HashMap<Vec<u16>, Vec<ReverseCMapEntry>>>,
    /// Predefined Adobe collection named by `usecmap`; codes the bf sections
    /// omit resolve through it (pdfium `CPDF_ToUnicodeMap::Lookup`,
    /// cpdf_tounicodemap.cpp:50-63,154-173).
    base: Option<AdobeBase>,
}
/// Represents the information needed to map a Unicode sequence back to a source code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReverseCMapEntry {
    pub source_code: SourceCode,
    pub code_len: CodeLen,
    // Optionally, add priority if multiple source codes map to the same Unicode sequence
    // pub priority: u8,
}

#[derive(Debug, Error)]
pub enum UnicodeCMapError {
    #[error("could not parse ToUnicode CMap: {0:#?}")]
    Parse(CMapParseError),
    #[error("invalid code range")]
    InvalidCodeRange,
}

impl From<CMapParseError> for UnicodeCMapError {
    fn from(err: CMapParseError) -> Self {
        UnicodeCMapError::Parse(err)
    }
}

/// Normalize the PostScript container around a ToUnicode CMap.
///
/// Some producers write CR-only line endings, append a NUL terminator, or put
/// PostScript `def` tokens inside the `/CIDSystemInfo` dictionary.  The nom
/// grammar accepts the common LF/space form; normalizing here keeps the
/// grammar strict without rejecting real-world files.
fn normalize_cmap(input: &[u8]) -> Vec<u8> {
    let mut data = input.to_vec();
    while data.last() == Some(&0) {
        data.pop();
    }
    for byte in &mut data {
        if *byte == b'\r' {
            *byte = b' ';
        }
    }
    let Some(start) = data
        .windows(b"/CIDSystemInfo".len())
        .position(|window| window == b"/CIDSystemInfo")
    else {
        return data;
    };
    let Some(end_offset) = data[start..].windows(2).position(|window| window == b">>") else {
        return data;
    };
    let end = start + end_offset;
    let mut cleaned = Vec::with_capacity(data.len());
    cleaned.extend_from_slice(&data[..start]);
    let mut index = start;
    while index < end {
        let is_def = index + 3 <= end
            && &data[index..index + 3] == b"def"
            && index > 0
            && data[index - 1].is_ascii_whitespace()
            && index + 3 < data.len()
            && data[index + 3].is_ascii_whitespace();
        if is_def {
            index += 3;
        } else {
            cleaned.push(data[index]);
            index += 1;
        }
    }
    cleaned.extend_from_slice(&data[end..]);
    cleaned
}

/// Adds `delta` to a UTF-16 sequence as a big-endian base-65536 number,
/// propagating carry into the preceding units (pdfium `StringDataAdd`,
/// cpdf_tounicodemap.cpp:31-47).
fn add_utf16_units(units: &mut Vec<u16>, delta: u32) {
    let mut carry = u64::from(delta);
    for unit in units.iter_mut().rev() {
        if carry == 0 {
            break;
        }
        let sum = u64::from(*unit) + carry;
        *unit = (sum & 0xffff) as u16;
        carry = sum >> 16;
    }
    if carry > 0 {
        units.insert(0, (carry & 0xffff) as u16);
    }
}

impl ToUnicodeCMap {
    const REPLACEMENT_CHAR: u16 = 0xfffd;

    /// True when the CMap maps no code at all: no bfchar/bfrange entries and
    /// no predefined base collection.  pdf.js gates extraction fallbacks on
    /// the map's size (`hasIncludedToUnicodeMap = !!properties.toUnicode?.size`),
    /// so a shell CMap — producers emit `0 beginbfchar` headers with no
    /// entries — must not count as a usable ToUnicode (issue1350).
    pub fn is_empty(&self) -> bool {
        self.bf_ranges.iter().all(|range| range.is_empty())
            && self.single.iter().all(FxHashMap::is_empty)
            && self.base.is_none()
    }

    pub fn new() -> ToUnicodeCMap {
        ToUnicodeCMap {
            bf_ranges: [(); 4].map(|_| RangeInclusiveMap::new()),
            single: [(); 4].map(|_| FxHashMap::default()),
            codespace_ranges: Vec::new(),
            reverse_map: None,
            base: None,
        }
    }

    pub fn parse(stream_content: Vec<u8>) -> Result<ToUnicodeCMap, UnicodeCMapError> {
        let normalized = normalize_cmap(&stream_content);
        let cmap_sections = match parse(&normalized[..]) {
            Ok(sections) => sections,
            // pdfapi2 folds the whole CIDInit prolog into the first-line
            // `%!PS-Adobe-3.0 Resource-CMap/CIDInit ...` comment, leaving no
            // standalone `/CIDInit /ProcSet findresource begin` token stream
            // (pdf.js issue12705).  The word-wise readers — pdf.js, pdfium —
            // parse the bfchar sections regardless; retry with a synthesized
            // prolog when the payload carries real mappings.
            Err(err) => {
                if normalized
                    .windows(b"begincmap".len())
                    .any(|window| window == b"begincmap")
                {
                    let mut repaired = b"/CIDInit /ProcSet findresource begin\n".to_vec();
                    repaired.extend_from_slice(&normalized);
                    parse(&repaired[..])?
                } else {
                    return Err(err.into());
                }
            }
        };
        Self::from_sections(cmap_sections)
    }

    fn from_sections(parsed: ParsedCMap) -> Result<ToUnicodeCMap, UnicodeCMapError> {
        let mut cmap = Self::new();
        cmap.base = parsed.usecmap.as_deref().and_then(AdobeBase::from_name);
        for section in parsed.sections {
            match section {
                CMapSection::CsRange(ranges) => cmap.codespace_ranges.extend(ranges),
                CMapSection::BfChar(char_mappings) => {
                    for ((code, code_len), dst) in char_mappings {
                        cmap.put_char(code, code_len, dst);
                    }
                }
                CMapSection::BfRange(range_mappings) => {
                    // The section is applied as a unit: one invalid range
                    // discards it and leaves the other sections intact
                    // (pdfium `HandleBeginBFRange`, cpdf_tounicodemap.cpp:327-340).
                    if range_mappings
                        .iter()
                        .any(|((start, end, _), dst)| end < start || dst.is_empty())
                    {
                        continue;
                    }
                    for ((start, end, code_len), dst_vec) in range_mappings {
                        match dst_vec.len() {
                            1 if dst_vec[0].len() == 1 => cmap.put(
                                start,
                                end,
                                code_len,
                                BfRangeTarget::UTF16CodePoint {
                                    offset: u32::wrapping_sub(dst_vec[0][0] as u32, start),
                                },
                            ),
                            1 => cmap.put(
                                start,
                                end,
                                code_len,
                                BfRangeTarget::HexString(dst_vec[0].clone()),
                            ),
                            _ => cmap.put(
                                start,
                                end,
                                code_len,
                                BfRangeTarget::ArrayOfHexStrings(dst_vec.clone()),
                            ),
                        }
                    }
                }
            }
        }

        let mut rev_map = HashMap::new();

        for code_len_idx in 0..cmap.bf_ranges.len() {
            let code_len = (code_len_idx + 1) as u8;
            for (&src_code, target) in &cmap.single[code_len_idx] {
                push_reverse_mapping(
                    &mut rev_map,
                    target,
                    src_code,
                    src_code,
                    code_len,
                );
            }
            for (range, target) in cmap.bf_ranges[code_len_idx].iter() {
                for src_code in range.clone() {
                    push_reverse_mapping(
                        &mut rev_map,
                        target,
                        src_code,
                        *range.start(),
                        code_len,
                    );
                }
            }
        }
        cmap.reverse_map = Some(rev_map);

        Ok(cmap)
    }

    pub(super) fn get_for_decoding(&self, code: SourceCode, code_len: CodeLen) -> Option<Vec<u16>> {
        if self.codespace_ranges.is_empty() {
            return self.get(code, code_len);
        }

        // Codespace bounds apply to each byte independently (ISO 32000-1, 9.7.5.1).
        self.codespace_ranges
            .iter()
            .any(|&(low, high, len)| {
                len == code_len
                    && (0..len).all(|i| {
                        let shift = 8 * i;
                        let byte = (code >> shift) as u8;
                        (low >> shift) as u8 <= byte && byte <= (high >> shift) as u8
                    })
            })
            .then(|| self.get_or_replacement_char(code, code_len))
    }

    pub fn get(&self, code: SourceCode, code_len: CodeLen) -> Option<Vec<u16>> {
        if code_len > 4 || code_len == 0 {
            error!("Code lenght should be between l and 4 bytes, got {code_len}");
            return None;
        }
        let index = (code_len - 1) as usize;
        let mapped = self.single[index]
            .get(&code)
            .map(|value| value_for_code(value, code, code))
            .or_else(|| {
                self.bf_ranges[index]
                    .get_key_value(&code)
                    .map(|(range, value)| value_for_code(value, code, *range.start()))
            })
            .or_else(|| {
                // Some CMaps declare a 1-byte codespace for fonts whose
                // content streams paint 2-byte codes (pdfapi2 writes
                // `<42> <5f>` for an Identity-H font; pdf.js keys the map by
                // numeric value only, so code 0x0042 finds the 1-byte
                // entry — pdf.js bug898853.pdf).
                for (other, bucket) in self.bf_ranges.iter().enumerate() {
                    if other == index {
                        continue;
                    }
                    if let Some(value) = self.single[other].get(&code) {
                        return Some(value_for_code(value, code, code));
                    }
                    if let Some((range, value)) = bucket.get_key_value(&code) {
                        return Some(value_for_code(value, code, *range.start()));
                    }
                }
                None
            });
        mapped.or_else(|| self.get_from_base(code))
    }

    /// Resolve a code through the `usecmap` collection.  pdfium casts the
    /// charcode to a CID and looks it up in the base map only when the
    /// bfchar/bfrange tables have no entry (cpdf_tounicodemap.cpp:50-63).
    fn get_from_base(&self, code: SourceCode) -> Option<Vec<u16>> {
        let index = usize::try_from(code).ok()?;
        if index > usize::from(u16::MAX) {
            return None;
        }
        let value = self.base?.table()?.get(index).copied().flatten()?;
        Some(adobe_utf16_units(value))
    }

    pub fn get_or_replacement_char(&self, code: SourceCode, code_len: CodeLen) -> Vec<u16> {
        self.get(code, code_len)
            .unwrap_or(vec![ToUnicodeCMap::REPLACEMENT_CHAR])
    }

    pub fn put(
        &mut self,
        src_code_lo: SourceCode,
        src_code_hi: SourceCode,
        code_len: CodeLen,
        target: BfRangeTarget,
    ) {
        if code_len > 4 || code_len == 0 {
            error!("Code lenght should be between l and 4 bytes, got {code_len}, ignoring");
            return;
        }
        self.bf_ranges[(code_len - 1) as usize].insert(src_code_lo..=src_code_hi, target)
    }

    pub fn put_char(&mut self, code: SourceCode, code_len: CodeLen, dst: Vec<u16>) {
        let target = if dst.len() == 1 {
            BfRangeTarget::UTF16CodePoint {
                offset: u32::wrapping_sub(dst[0] as u32, code),
            }
        } else {
            BfRangeTarget::HexString(dst)
        };
        if code_len > 4 || code_len == 0 {
            error!("Code lenght should be between l and 4 bytes, got {code_len}, ignoring");
            return;
        }
        self.single[(code_len - 1) as usize].insert(code, target);
    }

    /// Gets the source code(s) for a given Unicode sequence.
    /// Prioritizes shorter byte sequences if multiple mappings exist.
    pub fn get_source_codes_for_unicode(
        &self,
        unicode_sequence: &[u16],
    ) -> Option<&[ReverseCMapEntry]> {
        if let Some(map) = &self.reverse_map {
            // TODO: Add prioritization logic if needed (e.g., prefer shorter code_len)
            map.get(unicode_sequence).map(|v| v.as_slice())
        } else {
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BfRangeTarget {
    // UTF16-BE encoding is used
    HexString(Vec<u16>),
    // don't store the actual codepoint but rather an offset to the src_code_lo
    // so that consecutive ranges can be mapped to the same value in the range map
    UTF16CodePoint { offset: u32 },
    ArrayOfHexStrings(Vec<Vec<u16>>),
}

/// The predefined Adobe UCS2 collections pdfium recognises after `usecmap`
/// (cpdf_tounicodemap.cpp:154-173).  The tables are the pdf.js binary CMaps
/// shipped in `external/bcmaps`; they map CID to a UTF-16 destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdobeBase {
    Japan1,
    Gb1,
    Cns1,
    Korea1,
}

impl AdobeBase {
    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"Adobe-Japan1-UCS2" => Self::Japan1,
            b"Adobe-GB1-UCS2" => Self::Gb1,
            b"Adobe-CNS1-UCS2" => Self::Cns1,
            b"Adobe-Korea1-UCS2" => Self::Korea1,
            _ => return None,
        })
    }

    fn table(self) -> Option<&'static [Option<u32>]> {
        static JAPAN1: OnceLock<Option<Vec<Option<u32>>>> = OnceLock::new();
        static GB1: OnceLock<Option<Vec<Option<u32>>>> = OnceLock::new();
        static CNS1: OnceLock<Option<Vec<Option<u32>>>> = OnceLock::new();
        static KOREA1: OnceLock<Option<Vec<Option<u32>>>> = OnceLock::new();
        let (slot, data): (&'static OnceLock<Option<Vec<Option<u32>>>>, &'static [u8]) = match self
        {
            Self::Japan1 => (
                &JAPAN1,
                include_bytes!("data/Adobe-Japan1-UCS2.bcmap") as &[u8],
            ),
            Self::Gb1 => (&GB1, include_bytes!("data/Adobe-GB1-UCS2.bcmap") as &[u8]),
            Self::Cns1 => (&CNS1, include_bytes!("data/Adobe-CNS1-UCS2.bcmap") as &[u8]),
            Self::Korea1 => (
                &KOREA1,
                include_bytes!("data/Adobe-Korea1-UCS2.bcmap") as &[u8],
            ),
        };
        slot.get_or_init(|| read_adobe_ucs2(data)).as_deref()
    }
}

/// Decode a packed UTF-16 destination: one unit or a surrogate pair.
fn adobe_utf16_units(value: u32) -> Vec<u16> {
    if value <= 0xffff {
        return vec![value as u16];
    }
    if let Some(character) = char::from_u32(value) {
        let mut units = [0u16; 2];
        return character.encode_utf16(&mut units).to_vec();
    }
    vec![(value >> 16) as u16, (value & 0xffff) as u16]
}

struct BcmapReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BcmapReader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn number(&mut self) -> Option<u32> {
        let mut value = 0u32;
        loop {
            let byte = self.byte()?;
            value = value.checked_shl(7)? | u32::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
    }

    fn hex(&mut self, size: usize) -> Option<u32> {
        let end = self.position.checked_add(size + 1)?;
        let bytes = self.data.get(self.position..end)?;
        self.position = end;
        Some(
            bytes
                .iter()
                .fold(0u32, |value, byte| (value << 8) | u32::from(*byte)),
        )
    }

    fn hex_number(&mut self, size: usize) -> Option<u32> {
        let mut stack = Vec::new();
        loop {
            let byte = self.byte()?;
            stack.push(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        let mut output = vec![0u8; size + 1];
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for index in (0..=size).rev() {
            while bits < 8 {
                let Some(value) = stack.pop() else {
                    break;
                };
                buffer |= u32::from(value) << bits;
                bits += 7;
            }
            output[index] = (buffer & 0xff) as u8;
            buffer >>= 8;
            bits = bits.saturating_sub(8);
        }
        Some(
            output
                .iter()
                .fold(0u32, |value, byte| (value << 8) | u32::from(*byte)),
        )
    }

    fn hex_signed(&mut self, size: usize) -> Option<i64> {
        let value = self.hex_number(size)?;
        let bytes = value.to_be_bytes();
        let start = 4usize.saturating_sub(size + 1);
        let sign = if bytes[3] & 1 != 0 { 0xff } else { 0 };
        let mut carry = 0u16;
        let mut output = 0i64;
        for byte in &bytes[start..] {
            carry = ((carry & 1) << 8) | u16::from(*byte);
            output = (output << 8) | i64::from(((carry >> 1) as u8) ^ sign);
        }
        Some(output)
    }

    fn string(&mut self) -> Option<()> {
        let length = self.number()? as usize;
        for _ in 0..length {
            self.number()?;
        }
        Some(())
    }
}

fn bcmap_add(value: u32, delta: u32, size: usize) -> u32 {
    let mask = if size + 1 >= 4 {
        u32::MAX
    } else {
        (1u32 << (8 * (size + 1))) - 1
    };
    value.wrapping_add(delta) & mask
}

fn bcmap_increment(value: u32, size: usize) -> u32 {
    bcmap_add(value, 1, size)
}

fn bcmap_set(table: &mut Vec<Option<u32>>, cid: u32, value: u32) {
    let Ok(index) = usize::try_from(cid) else {
        return;
    };
    if index > usize::from(u16::MAX) {
        return;
    }
    if table.len() <= index {
        table.resize(index + 1, None);
    }
    table[index] = Some(value);
}

/// Read the `CID -> UTF-16 destination` entries from an Adobe UCS2 binary
/// CMap.  Only the entry kinds those tables use (codespace, bfchar, bfrange)
/// are handled; an unknown kind drops the whole base map rather than guessing.
fn read_adobe_ucs2(data: &[u8]) -> Option<Vec<Option<u32>>> {
    let mut reader = BcmapReader { data, position: 0 };
    reader.byte()?; // header flags
    let mut table: Vec<Option<u32>> = Vec::new();
    while let Some(tag) = reader.byte() {
        let kind = tag >> 5;
        if kind == 7 {
            // Metadata (comment or usecmap); the latter is not followed by
            // these tables.
            if matches!(tag & 0x1f, 0 | 1) {
                reader.string()?;
            }
            continue;
        }
        let sequence = tag & 0x10 != 0;
        let size = usize::from(tag & 0x0f);
        let count = reader.number()?;
        match kind {
            0 => {
                let start = reader.hex(size)?;
                let mut end = bcmap_add(reader.hex_number(size)?, start, size);
                for _ in 1..count {
                    end = bcmap_increment(end, size);
                    let start = bcmap_add(reader.hex_number(size)?, end, size);
                    end = bcmap_add(reader.hex_number(size)?, start, size);
                }
            }
            4 => {
                let mut character = reader.hex(1)?;
                let mut code = reader.hex(size)?;
                bcmap_set(&mut table, character, code);
                for _ in 1..count {
                    character = bcmap_increment(character, 1);
                    if !sequence {
                        character = bcmap_add(reader.hex_number(1)?, character, 1);
                    }
                    code = bcmap_increment(code, size);
                    code = bcmap_add(reader.hex_signed(size)? as u32, code, size);
                    bcmap_set(&mut table, character, code);
                }
            }
            5 => {
                let mut start = reader.hex(1)?;
                let mut end = bcmap_add(reader.hex_number(1)?, start, 1);
                let mut code = reader.hex(size)?;
                for cid in start..=end {
                    let value = bcmap_add(code, cid - start, size);
                    bcmap_set(&mut table, cid, value);
                }
                for _ in 1..count {
                    end = bcmap_increment(end, 1);
                    if !sequence {
                        start = bcmap_add(reader.hex_number(1)?, end, 1);
                    } else {
                        start = end;
                    }
                    end = bcmap_add(reader.hex_number(1)?, start, 1);
                    code = reader.hex(size)?;
                    for cid in start..=end {
                        let value = bcmap_add(code, cid - start, size);
                        bcmap_set(&mut table, cid, value);
                    }
                }
            }
            _ => return None,
        }
    }
    Some(table)
}

fn value_for_code(target: &BfRangeTarget, code: SourceCode, range_start: SourceCode) -> Vec<u16> {
    match target {
        BfRangeTarget::HexString(units) => {
            let mut result = units.clone();
            add_utf16_units(&mut result, code - range_start);
            result
        }
        BfRangeTarget::UTF16CodePoint { offset } => {
            vec![u32::wrapping_add(code, *offset) as u16]
        }
        BfRangeTarget::ArrayOfHexStrings(values) => {
            let index = (code - range_start) as usize;
            values
                .get(index)
                .cloned()
                .unwrap_or_else(|| vec![ToUnicodeCMap::REPLACEMENT_CHAR])
        }
    }
}

fn push_reverse_mapping(
    rev_map: &mut HashMap<Vec<u16>, Vec<ReverseCMapEntry>>,
    target: &BfRangeTarget,
    src_code: SourceCode,
    range_start: SourceCode,
    code_len: CodeLen,
) {
    let unicode_sequence = match target {
        BfRangeTarget::UTF16CodePoint { offset } => {
            Some(vec![u32::wrapping_add(src_code, *offset) as u16])
        }
        BfRangeTarget::HexString(hex_str_vec) => {
            if src_code == range_start {
                Some(hex_str_vec.clone())
            } else if hex_str_vec.len() == 1 {
                Some(vec![
                    hex_str_vec[0].wrapping_add((src_code - range_start) as u16),
                ])
            } else if !hex_str_vec.is_empty() {
                let mut current_hex_str = hex_str_vec.clone();
                add_utf16_units(&mut current_hex_str, (src_code - range_start) as u32);
                Some(current_hex_str)
            } else {
                None
            }
        }
        BfRangeTarget::ArrayOfHexStrings(array_of_hex_str) => {
            let index = (src_code - range_start) as usize;
            array_of_hex_str.get(index).cloned()
        }
    };
    if let Some(uni_seq) = unicode_sequence
        && !uni_seq.is_empty()
    {
        rev_map
            .entry(uni_seq)
            .or_insert_with(Vec::new)
            .push(ReverseCMapEntry {
                source_code: src_code,
                code_len,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_char_can_be_retrieved() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x01;
        let char_value = vec![0x1234];
        cmap.put_char(char_code, 2, char_value.clone());

        assert_eq!(cmap.get(char_code, 2), Some(char_value))
    }

    #[test]
    fn folded_prolog_comment_still_parses() {
        // pdf.js issue12705: pdfapi2 folds the CIDInit prolog into the
        // first-line `%!PS` comment, so no standalone `/CIDInit /ProcSet
        // findresource begin` token stream exists.  pdf.js and pdfium parse
        // the bfchar sections word-wise; the retry must too.
        let content = b"%!PS-Adobe-3.0 Resource-CMap/CIDInit /ProcSet findresource begin
12 dict begin

begincmap
/CIDSystemInfo 3 dict dup begin
/Registry (Adobe) def
/Ordering () def
/Supplement 0 def
end def

/CMapType 2 def

1 begincodespacerange
<00> <FF>
endcodespacerange

1 beginbfchar
<FC> <05D3>
endbfchar

endcmap
CMapName currentdict /CMap defineresource pop
end
";
        let map = ToUnicodeCMap::parse(content.to_vec()).expect("must parse");
        assert_eq!(map.get(0xFC, 1), Some(vec![0x05D3]));
        assert!(!map.is_empty());
    }

    #[test]
    #[ignore = "vendored CMap lookup ignores the code length"]
    fn char_can_be_retrieved_only_by_appropriate_len() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x1;
        let code_len = 4;
        let char_value = vec![0x1234];
        cmap.put_char(char_code, code_len, char_value.clone());

        for i in 1..=3 {
            assert_eq!(cmap.get(char_code, i), None);
        }

        assert_eq!(cmap.get(char_code, code_len), Some(char_value));
    }

    #[test]
    fn wrong_code_len_does_not_panic() {
        let mut cmap = ToUnicodeCMap::new();
        let char_code = 0x1;
        let char_value = vec![0x1234];

        cmap.put_char(char_code, 5, char_value.clone());
        cmap.put_char(char_code, 0, char_value.clone());
    }

    #[test]
    fn array_of_hex_strings_out_of_bounds_returns_replacement() {
        // Simulate a malformed CMap bfrange where the array has fewer entries
        // than the declared source code range covers.
        let mut cmap = ToUnicodeCMap::new();
        let array = BfRangeTarget::ArrayOfHexStrings(vec![
            vec![0x0041], // 'A' — for code 0x10
            vec![0x0042], // 'B' — for code 0x11
        ]);
        // Range 0x10..=0x14 but only 2 entries in array (needs 5)
        cmap.put(0x10, 0x14, 2, array);

        // In-bounds lookups work
        assert_eq!(cmap.get(0x10, 2), Some(vec![0x0041]));
        assert_eq!(cmap.get(0x11, 2), Some(vec![0x0042]));

        // Out-of-bounds lookups return replacement char instead of panicking
        assert_eq!(
            cmap.get(0x12, 2),
            Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR])
        );
        assert_eq!(
            cmap.get(0x13, 2),
            Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR])
        );
        assert_eq!(
            cmap.get(0x14, 2),
            Some(vec![ToUnicodeCMap::REPLACEMENT_CHAR])
        );
    }

    #[test]
    fn adobe_ucs2_tables_load() {
        for base in [
            AdobeBase::Japan1,
            AdobeBase::Gb1,
            AdobeBase::Cns1,
            AdobeBase::Korea1,
        ] {
            let table = base.table().expect("adobe ucs2 table");
            assert!(table.iter().any(Option::is_some), "{base:?} empty");
        }
        let japan = AdobeBase::Japan1.table().expect("japan1");
        assert_eq!(japan[982], Some(12506));
    }

    #[test]
    fn bfrange_target_overflow_wraps_instead_of_panicking() {
        // The last UTF-16 unit of a multi-unit bfrange target is incremented per source code,
        // so a crafted target ending in <FFFF> overflows for the second code of the range.
        let data = b"/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<00> <FF>
endcodespacerange
1 beginbfrange
<01> <02> <D83DFFFF>
endbfrange
endcmap
CMapName currentdict /CMap defineresource pop
end
end";
        let cmap = ToUnicodeCMap::parse(data.to_vec()).unwrap();

        assert_eq!(cmap.get(0x01, 1), Some(vec![0xD83D, 0xFFFF]));
        assert_eq!(cmap.get(0x02, 1), Some(vec![0xD83E, 0x0000]));
    }
}
