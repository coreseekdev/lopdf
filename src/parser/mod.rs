use super::{Dictionary, Object, ObjectId, PdfName, PdfString, Reader, Stream, StringFormat};
use crate::Error;
use crate::content::*;
use crate::error;
use crate::xref::*;
use std::collections::HashSet;
use std::borrow::Cow;
use std::str;
use std::str::FromStr;

pub(crate) mod cmap_parser;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    Alt, Digit, Eof, Fail, LengthValue, Many0, MapRes, OneOf, Tag, TakeWhile1, TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NomError<'a> { input: ParserInput<'a>, kind: ErrorKind }
impl<'a> NomError<'a> {
    pub(crate) fn new(input: ParserInput<'a>, kind: ErrorKind) -> Self { Self { input, kind } }
    pub(crate) fn from_error_kind(input: ParserInput<'a>, kind: ErrorKind) -> Self { Self::new(input, kind) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParseErr<E> { Error(E), Failure(E), Incomplete }

pub(crate) type ParserInput<'a> = &'a [u8];
pub(crate) type NomResult<'a, O> = Result<(ParserInput<'a>, O), ParseErr<NomError<'a>>>;

#[inline]
fn strip_nom<O>(r: NomResult<O>) -> Option<O> {
    r.ok().map(|(_, o)| o)
}

#[inline]
fn convert_result<O, E>(
    result: Result<O, E>,
    input: ParserInput,
    error_kind: ErrorKind,
) -> NomResult<O> {
    result.map(|o| (input, o)).map_err(|_| {
        // this is a unit bind if NomError = ()
        ParseErr::Error(NomError::from_error_kind(input, error_kind))
    })
}

#[inline]
fn offset_stream(object: &mut Object, offset: usize) {
    if let Object::Stream(stream) = object {
        stream.start_position = stream.start_position.and_then(|sp| sp.checked_add(offset));
    }
}

pub(crate) fn eol(input: ParserInput) -> NomResult<ParserInput> {
    let rest = input
        .strip_prefix(b"\r\n")
        .or_else(|| input.strip_prefix(b"\n"))
        .or_else(|| input.strip_prefix(b"\r"))
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    Ok((rest, &input[..input.len() - rest.len()]))
}

pub(crate) fn comment(input: ParserInput) -> NomResult<()> {
    let rest = input
        .strip_prefix(b"%")
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    let end = rest
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))
        .unwrap_or(rest.len());
    let (rest, _) = eol(&rest[end..])?;
    Ok((rest, ()))
}

#[inline]
fn is_whitespace(c: u8) -> bool {
    matches!(c, 0 | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

#[inline]
fn is_delimiter(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// PDF regular-character table; `name` scans it once per byte.
const REGULAR: [bool; 256] = {
    let mut table = [true; 256];
    table[0] = false;
    table[b'\t' as usize] = false;
    table[b'\n' as usize] = false;
    table[0x0C] = false;
    table[b'\r' as usize] = false;
    table[b' ' as usize] = false;
    let delimiters = b"()<>[]{}/%";
    let mut index = 0;
    while index < delimiters.len() {
        table[delimiters[index] as usize] = false;
        index += 1;
    }
    table
};

#[inline]
fn is_regular(c: u8) -> bool {
    REGULAR[c as usize]
}

#[inline]
fn space(input: ParserInput) -> NomResult<()> {
    let mut rest = input;
    loop {
        let bytes = rest;
        let mut index = 0;
        while index < bytes.len() && is_whitespace(bytes[index]) {
            index += 1;
        }
        rest = &rest[index..];
        if rest.first() != Some(&b'%') {
            return Ok((rest, ()));
        }
        // A comment only counts when its terminating EOL is present; a `%`
        // at EOF is left for the caller, matching the old fold behaviour.
        let mut end = 1;
        while end < rest.len() && !matches!(rest[end], b'\r' | b'\n') {
            end += 1;
        }
        match rest.get(end) {
            Some(b'\r') => {
                end += 1;
                if rest.get(end) == Some(&b'\n') {
                    end += 1;
                }
            }
            Some(b'\n') => end += 1,
            _ => return Ok((rest, ())),
        }
        rest = &rest[end..];
    }
}

fn integer(input: ParserInput) -> NomResult<i64> {
    let bytes = input;
    let mut index = 0;
    let negative = match bytes.first() {
        Some(b'-') => {
            index = 1;
            true
        }
        Some(b'+') => {
            index = 1;
            false
        }
        _ => false,
    };
    let digits_start = index;
    let mut value = 0u64;
    let mut overflow = false;
    while let Some(&byte) = bytes.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        if !overflow {
            match value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(byte - b'0')))
            {
                Some(next) => value = next,
                None => overflow = true,
            }
        }
        index += 1;
    }
    if index == digits_start {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Digit,
        )));
    }
    let rest = &bytes[index..];
    let too_large = if negative {
        value > i64::MAX as u64 + 1
    } else {
        value > i64::MAX as u64
    };
    if overflow || too_large {
        // Rare oversized values keep the standard parser's error/saturation
        // behavior.
        return convert_result(
            i64::from_str(str::from_utf8(&input[..index]).unwrap()),
            rest,
            ErrorKind::Digit,
        );
    }
    let value = if negative {
        if value == i64::MAX as u64 + 1 {
            i64::MIN
        } else {
            -(value as i64)
        }
    } else {
        value as i64
    };
    Ok((rest, value))
}

#[inline]
fn real(input: ParserInput) -> NomResult<f32> {
    let bytes = input;
    let mut offset = 0usize;
    let negative = match bytes.first() {
        Some(b'-') => {
            offset = 1;
            true
        }
        Some(b'+') => {
            offset = 1;
            false
        }
        _ => false,
    };
    let mut integer = 0u64;
    let mut fraction = 0u64;
    let mut fraction_digits = 0u32;
    let mut digits = 0u32;
    let mut saw_dot = false;
    let mut long = false;
    while let Some(&byte) = bytes.get(offset) {
        match byte {
            b'0'..=b'9' => {
                let digit = u64::from(byte - b'0');
                digits = digits.saturating_add(1);
                if digits > 15 {
                    long = true;
                }
                if saw_dot {
                    fraction = fraction.saturating_mul(10).saturating_add(digit);
                    fraction_digits = fraction_digits.saturating_add(1);
                } else {
                    integer = integer.saturating_mul(10).saturating_add(digit);
                }
                offset += 1;
            }
            b'.' if !saw_dot => {
                saw_dot = true;
                offset += 1;
            }
            // Some producers write a minus sign inside the fraction, as in
            // `0.00-4142502`. Like pdf.js and Poppler, ignore it rather than
            // splitting the number in two.
            b'-' if saw_dot => offset += 1,
            _ => break,
        }
    }
    if digits == 0 || !saw_dot {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Digit,
        )));
    }
    let rest = &input[offset..];
    if long {
        // Rare long mantissas fall back to the correctly-rounded standard
        // parser; the common short path avoids UTF-8 validation and
        // `from_str`.
        let mut cleaned = Vec::with_capacity(offset + 1);
        if negative {
            cleaned.push(b'-');
        }
        cleaned.extend(bytes[..offset].iter().copied().filter(|&byte| byte != b'-'));
        let value = f32::from_str(str::from_utf8(&cleaned).unwrap())
            .map_err(|_| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Digit)))?;
        return Ok((rest, value));
    }
    let scale = 10u64.pow(fraction_digits) as f64;
    let value = (integer as f64 + fraction as f64 / scale) * if negative { -1.0 } else { 1.0 };
    Ok((rest, value as f32))
}

#[inline]
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn hex_char(input: ParserInput) -> NomResult<u8> {
    let bytes = input
        .get(..2)
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Digit)))?;
    let high = hex_value(bytes[0])
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Digit)))?;
    let low = hex_value(bytes[1])
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Digit)))?;
    Ok((&input[2..], high << 4 | low))
}

#[inline]
pub(crate) fn name(input: ParserInput) -> NomResult<Vec<u8>> {
    let Some(rest) = input.strip_prefix(b"/") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    // The overwhelmingly common name is a run of regular bytes with no
    // `#xx` escape.  Scan it once and make one exact-size copy instead of
    // driving the byte at a time through nom's choice/iterator stack.
    let mut end = 0;
    while end < rest.len() && rest[end] != b'#' && is_regular(rest[end]) {
        end += 1;
    }
    if end == rest.len() || rest[end] != b'#' {
        return Ok((&rest[end..], rest[..end].to_vec()));
    }
    let mut out = Vec::with_capacity(rest.len());
    out.extend_from_slice(&rest[..end]);
    let mut index = end;
    while index < rest.len() {
        if rest[index] == b'#' {
            if index + 2 < rest.len()
                && rest[index + 1].is_ascii_hexdigit()
                && rest[index + 2].is_ascii_hexdigit()
            {
                fn nibble(byte: u8) -> u8 {
                    match byte {
                        b'0'..=b'9' => byte - b'0',
                        b'a'..=b'f' => byte - b'a' + 10,
                        _ => byte - b'A' + 10,
                    }
                }
                out.push(nibble(rest[index + 1]) << 4 | nibble(rest[index + 2]));
                index += 3;
                continue;
            }
            break;
        }
        if !is_regular(rest[index]) {
            break;
        }
        out.push(rest[index]);
        index += 1;
    }
    Ok((&rest[index..], out))
}

/// Scans a literal string body starting just after `(`, appending decoded
/// bytes.  Returns the consumed length including the closing `)` or the
/// offset where parsing failed.
#[inline]
fn literal_body(
    input: ParserInput,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<usize, usize> {
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b')' => return Ok(index + 1),
            b'(' => {
                if depth == 0 {
                    return Err(index);
                }
                let consumed = literal_body(&input[index + 1..], depth - 1, out)?;
                index += 1 + consumed;
            }
            b'\\' => {
                index += 1;
                let Some(&next) = input.get(index) else {
                    return Err(index);
                };
                if next.is_ascii_digit() && next < b'8' {
                    let mut value = 0u16;
                    let mut digits = 0;
                    while digits < 3 {
                        let Some(&byte) = input.get(index) else {
                            break;
                        };
                        if !(b'0'..=b'7').contains(&byte) {
                            break;
                        }
                        value = value * 8 + u16::from(byte - b'0');
                        index += 1;
                        digits += 1;
                    }
                    out.push(value as u8);
                } else if next == b'\r' {
                    index += 1;
                    if input.get(index) == Some(&b'\n') {
                        index += 1;
                    }
                } else if next == b'\n' {
                    index += 1;
                } else {
                    out.push(match next {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => b'\x08',
                        b'f' => b'\x0C',
                        other => other,
                    });
                    index += 1;
                }
            }
            b'\r' => {
                out.push(b'\n');
                index += 1;
                if input.get(index) == Some(&b'\n') {
                    index += 1;
                }
            }
            b'\n' => {
                out.push(b'\n');
                index += 1;
            }
            _ => {
                let start = index;
                while index < input.len()
                    && !matches!(input[index], b'(' | b')' | b'\\' | b'\r' | b'\n')
                {
                    index += 1;
                }
                out.extend_from_slice(&input[start..index]);
            }
        }
    }
    Err(index)
}

fn literal_string_pdf(input: ParserInput) -> NomResult<PdfString> {
    let Some(rest) = input.strip_prefix(b"(") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut index = 0;
    while index < rest.len() {
        match rest[index] {
            b')' => return Ok((&rest[index + 1..], PdfString::new(&rest[..index]))),
            b'\\' | b'(' | b'\r' | b'\n' => break,
            _ => index += 1,
        }
    }
    let (rest, bytes) = literal_string(input)?;
    Ok((rest, PdfString::new(bytes)))
}

fn literal_string(input: ParserInput) -> NomResult<Vec<u8>> {
    let Some(rest) = input.strip_prefix(b"(") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut out = Vec::with_capacity(16);
    match literal_body(rest, crate::reader::MAX_BRACKET, &mut out) {
        Ok(consumed) => Ok((&rest[consumed..], out)),
        Err(offset) => Err(ParseErr::Error(NomError::from_error_kind(
            &rest[offset..],
            ErrorKind::Tag,
        ))),
    }
}

fn hexadecimal_string(input: ParserInput) -> NomResult<Object> {
    let Some(rest) = input.strip_prefix(b"<") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut out = Vec::with_capacity((rest.len() / 2).min(256));
    let mut high = None;
    let mut index = 0;
    while index < rest.len() {
        let byte = rest[index];
        if is_whitespace(byte) {
            index += 1;
            continue;
        }
        if byte == b'>' {
            if let Some(high) = high {
                out.push(high << 4);
            }
            return Ok((
                &rest[index + 1..],
                Object::String(out.into(), StringFormat::Hexadecimal),
            ));
        }
        let Some(value) = hex_value(byte) else {
            break;
        };
        match high.take() {
            None => high = Some(value),
            Some(high) => out.push(high << 4 | value),
        }
        index += 1;
    }
    Err(ParseErr::Error(NomError::from_error_kind(
        &rest[index..],
        ErrorKind::Tag,
    )))
}

#[inline]
fn boolean(input: ParserInput) -> NomResult<Object> {
    if let Some(rest) = input.strip_prefix(b"true") {
        Ok((rest, Object::Boolean(true)))
    } else if let Some(rest) = input.strip_prefix(b"false") {
        Ok((rest, Object::Boolean(false)))
    } else {
        Err(ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))
    }
}

#[inline]
fn null(input: ParserInput) -> NomResult<Object> {
    input
        .strip_prefix(b"null")
        .map(|rest| (rest, Object::Null))
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))
}

fn array(depth: usize) -> impl Fn(ParserInput) -> NomResult<Vec<Object>> {
    move |input| direct_array(input, depth)
}

pub(crate) fn dictionary(input: ParserInput) -> NomResult<Dictionary> {
    direct_dictionary(input, crate::reader::MAX_NESTING_DEPTH)
}

fn name_pdf(input: ParserInput) -> NomResult<'_, PdfName> {
    let Some(rest) = input.strip_prefix(b"/") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut end = 0;
    while end < rest.len() && rest[end] != b'#' && is_regular(rest[end]) {
        end += 1;
    }
    if end == rest.len() || rest[end] != b'#' {
        return Ok((&rest[end..], PdfName::new(&rest[..end])));
    }
    let (rest, bytes) = name(input)?;
    Ok((rest, PdfName::new(bytes)))
}

fn dictionary_key(input: ParserInput) -> NomResult<'_, PdfName> {
    name_pdf(input)
}

fn inner_dictionary(depth: usize) -> impl Fn(ParserInput) -> NomResult<Dictionary> {
    move |input| {
        let mut rest = input;
        let mut dict = Dictionary::builder(4);
        loop {
            let (next, _) = space(rest)?;
            rest = next;
            if rest.starts_with(b">>")
                || rest.starts_with(b"endobj")
                || (rest.starts_with(b"ID")
                    && rest
                        .get(2)
                        .is_some_and(|byte| byte.is_ascii_whitespace()))
            {
                return Ok((rest, dict.finish()));
            }
            let (next, key) = dictionary_key(rest)?;
            let (next, _) = space(next)?;
            let (next, value) = _direct_object(depth)(next)?;
            if next.len() == rest.len() {
                return Err(ParseErr::Error(NomError::from_error_kind(
                    rest,
                    ErrorKind::Many0,
                )));
            }
            dict.set(key, value);
            rest = next;
        }
    }
}

/// Recover the payload end from the first unambiguous terminator.
///
/// Accepted terminators are `endstream`, the misspellings pdf.js tolerates
/// (`endsteam`, `endstrea`, parser.js:629-667), and a bare `endobj` when the
/// producer omitted `endstream` altogether (pdfium `FindStreamEndPos`,
/// cpdf_syntax_parser.cpp:742-775).  The caller passes only bytes up to the
/// next xref-derived object start, so the scan cannot swallow a neighbour.
fn recover_stream_length(input: ParserInput) -> Option<(ParserInput, ParserInput)> {
    const ENDSTREAM: &[u8] = b"endstream";
    const MISSPELLED: [&[u8]; 2] = [b"endsteam", b"endstrea"];
    const ENDOBJ: &[u8] = b"endobj";

    let mut best: Option<(usize, usize)> = None;
    for word in [ENDSTREAM, MISSPELLED[0], MISSPELLED[1], ENDOBJ] {
        let Some(position) = input.windows(word.len()).position(|window| window == word) else {
            continue;
        };
        let after = position + word.len();
        let right_ok = match input.get(after).copied() {
            None => true,
            Some(byte) => {
                if MISSPELLED.contains(&word) {
                    // pdf.js requires whitespace after a corrupt
                    // `endsteam`/`endstrea` to avoid false positives.
                    byte.is_ascii_whitespace()
                } else {
                    byte.is_ascii_whitespace() || is_delimiter(byte)
                }
            }
        };
        if !right_ok {
            continue;
        }
        if best.is_none_or(|(best_position, _)| position < best_position) {
            best = Some((position, word.len()));
        }
    }

    let (position, word_len) = best?;
    // Trim the EOL that belongs to the terminator, not to the payload.
    let data_end = if input[..position].ends_with(b"\r\n") {
        position - 2
    } else if input[..position].ends_with(b"\n") || input[..position].ends_with(b"\r") {
        position - 1
    } else {
        // Damaged producers omit the EOL before `endstream` too; the unique
        // whole-word terminator is still a usable recovery signal.
        position
    };
    Some((&input[position + word_len..], &input[..data_end]))
}

#[inline]
fn consume_eol(input: ParserInput) -> Option<ParserInput> {
    if let Some(rest) = input.strip_prefix(b"\r\n") {
        Some(rest)
    } else if let Some(rest) = input.strip_prefix(b"\n") {
        Some(rest)
    } else {
        input.strip_prefix(b"\r")
    }
}

#[inline]
fn skip_stream_space(input: ParserInput) -> ParserInput {
    let count = input
        .iter()
        .take_while(|byte| matches!(**byte, b' ' | b'\t'))
        .count();
    &input[count..]
}

fn stream<'a>(
    input: ParserInput<'a>,
    reader: &Reader,
    already_seen: &mut HashSet<ObjectId>,
    recover_length: bool,
    recovery_bound: Option<usize>,
) -> NomResult<'a, Object> {
    let (rest, dict) = direct_dictionary(input, crate::reader::MAX_NESTING_DEPTH)?;
    let rest = space(rest)?.0;
    let Some(rest) = rest.strip_prefix(b"stream") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            rest,
            ErrorKind::Tag,
        )));
    };
    let rest = skip_stream_space(rest);
    let Some(i) = consume_eol(rest) else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            rest,
            ErrorKind::Tag,
        )));
    };

    let declared_length = dict.get(b"Length").ok().and_then(|value| {
        if let Ok(id) = value.as_reference() {
            reader
                .get_object_cached(id, already_seen)
                .ok()
                .and_then(|value| value.as_i64().ok())
        } else {
            value.as_i64().ok()
        }
    });

    if let Some(length) = declared_length {
        if let Ok(length) = usize::try_from(length) {
            if length <= i.len() {
                let data = &i[..length];
                let after = &i[length..];
                if let Some(after) = after.strip_prefix(b"\r\n").or_else(|| after.strip_prefix(b"\n")).or_else(|| after.strip_prefix(b"\r")).or(Some(after)) {
                    if let Some(remaining) = after.strip_prefix(b"endstream") {
                        return Ok((remaining, Object::Stream(Stream::new(dict, data.to_vec()))));
                    }
                }
            }
        }
    }

    if recover_length && !reader.strict {
        let scan_end = recovery_bound.map_or(i.len(), |bound| i.len().saturating_sub(bound));
        match recover_stream_length(&i[..scan_end]) {
            Some((remaining, data)) => {
                if let Some(length) = declared_length {
                    log::warn!(
                        "Stream Length is {length}, but the object boundary gives {} bytes; using the recovered length.",
                        data.len()
                    );
                }
                Ok((remaining, Object::Stream(Stream::new(dict, data.to_vec()))))
            }
            None => Ok((
                i,
                Object::Stream(Stream::with_position(dict, input.len() - i.len())),
            )),
        }
    } else if declared_length.is_none() {
        Ok((
            i,
            Object::Stream(Stream::with_position(dict, input.len() - i.len())),
        ))
    } else {
        Err(ParseErr::Failure(NomError::from_error_kind(
            i,
            ErrorKind::LengthValue,
        )))
    }
}

trait UnsignedToken: Sized {
    fn from_u64(value: u64) -> Option<Self>;
}

impl UnsignedToken for u16 {
    fn from_u64(value: u64) -> Option<Self> {
        u16::try_from(value).ok()
    }
}

impl UnsignedToken for u32 {
    fn from_u64(value: u64) -> Option<Self> {
        u32::try_from(value).ok()
    }
}

impl UnsignedToken for u64 {
    fn from_u64(value: u64) -> Option<Self> {
        Some(value)
    }
}

impl UnsignedToken for usize {
    fn from_u64(value: u64) -> Option<Self> {
        usize::try_from(value).ok()
    }
}

#[inline]
fn unsigned_int<I: UnsignedToken>(input: ParserInput) -> NomResult<I> {
    let bytes = input;
    let mut index = 0;
    let mut value = 0u64;
    while let Some(&byte) = bytes.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.wrapping_mul(10).wrapping_add(u64::from(byte - b'0'));
        index += 1;
    }
    if index == 0 {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::MapRes,
        )));
    }
    if index > 19 {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::MapRes,
        )));
    }
    match I::from_u64(value) {
        Some(value) => Ok((&bytes[index..], value)),
        None => Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::MapRes,
        ))),
    }
}

fn object_id(input: ParserInput) -> NomResult<ObjectId> {
    let (rest, number) = unsigned_int::<u32>(input)?;
    let (rest, _) = space(rest)?;
    let (rest, generation) = unsigned_int::<u16>(rest)?;
    let (rest, _) = space(rest)?;
    Ok((rest, (number, generation)))
}

fn reference(input: ParserInput) -> NomResult<Object> {
    let (rest, id) = object_id(input)?;
    let Some(rest) = rest.strip_prefix(b"R") else {
        return Err(ParseErr::Error(NomError::from_error_kind(rest, ErrorKind::Tag)));
    };
    Ok((rest, Object::Reference(id)))
}

fn try_direct_reference(input: ParserInput) -> Option<(ParserInput, Object)> {
    let mut index = 0usize;
    let mut number = 0u64;
    while let Some(byte) = input.get(index).copied().filter(u8::is_ascii_digit) {
        number = number.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        index += 1;
    }
    if index == 0 {
        return None;
    }
    let spaces = input[index..]
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    if spaces == 0 {
        return None;
    }
    index += spaces;
    let mut generation = 0u64;
    let start = index;
    while let Some(byte) = input.get(index).copied().filter(u8::is_ascii_digit) {
        generation = generation.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        index += 1;
    }
    if index == start {
        return None;
    }
    let spaces = input[index..]
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    if spaces == 0 {
        return None;
    }
    index += spaces;
    if input.get(index) != Some(&b'R')
        || input.get(index + 1).is_some_and(|byte| is_regular(*byte))
    {
        return None;
    }
    Some((
        &input[index + 1..],
        Object::Reference((
            u32::try_from(number).ok()?,
            u16::try_from(generation).ok()?,
        )),
    ))
}


fn direct_array(input: ParserInput, depth: usize) -> NomResult<Vec<Object>> {
    let Some(rest) = input.strip_prefix(b"[") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut rest = space(rest)?.0;
    let mut items = Vec::new();
    loop {
        if rest.starts_with(b"]") {
            return Ok((&rest[1..], items));
        }
        if rest.starts_with(b">>") || rest.starts_with(b"endobj") {
            return Ok((rest, items));
        }
        let (next, item) = direct_value(rest, depth - 1)?;
        items.push(item);
        rest = space(next)?.0;
    }
}

fn direct_dictionary(input: ParserInput, depth: usize) -> NomResult<Dictionary> {
    let Some(rest) = input.strip_prefix(b"<<") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut rest = space(rest)?.0;
    let mut dict = Dictionary::builder(4);
    loop {
        if rest.starts_with(b">>") {
            return Ok((&rest[2..], dict.finish()));
        }
        if rest.starts_with(b"endobj") {
            return Ok((rest, dict.finish()));
        }
        let (next, key) = name_pdf(rest)?;
        let (next, _) = space(next)?;
        let (next, value) = direct_value(next, depth - 1)?;
        dict.set(key, value);
        rest = space(next)?.0;
    }
}

fn direct_value(input: ParserInput, depth: usize) -> NomResult<Object> {
    if depth == 0 {
        return Err(ParseErr::Failure(NomError::from_error_kind(
            input,
            ErrorKind::TooLarge,
        )));
    }
    match input.first() {
        Some(b'/') => {
            let (rest, name) = name_pdf(input)?;
            Ok((rest, Object::Name(name)))
        }
        Some(b'(') => {
            let (rest, string) = literal_string_pdf(input)?;
            Ok((rest, Object::String(string, StringFormat::Literal)))
        }
        Some(b'<') => {
            if input.get(1) == Some(&b'<') {
                let (rest, dictionary) = direct_dictionary(input, depth)?;
                Ok((rest, Object::Dictionary(dictionary)))
            } else {
                hexadecimal_string(input)
            }
        }
        Some(b'[') => {
            let (rest, array) = direct_array(input, depth)?;
            Ok((rest, Object::Array(array)))
        }
        Some(b't') if input.starts_with(b"true") => Ok((&input[4..], Object::Boolean(true))),
        Some(b'f') if input.starts_with(b"false") => Ok((&input[5..], Object::Boolean(false))),
        Some(b'n') if input.starts_with(b"null") => Ok((&input[4..], Object::Null)),
        Some(b'0'..=b'9') => match try_direct_reference(input) {
            Some((rest, reference)) => Ok((rest, reference)),
            None => number(input),
        },
        Some(b'+' | b'-' | b'.') => number(input),
        _ => Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Alt,
        ))),
    }
}

fn _direct_objects(depth: usize) -> impl Fn(ParserInput) -> NomResult<Object> {
    move |input| direct_value(input, depth)
}

fn _direct_object(depth: usize) -> impl Fn(ParserInput) -> NomResult<Object> {
    move |input| {
        if depth == 0 {
            return Err(ParseErr::Failure(NomError::from_error_kind(
                input,
                ErrorKind::TooLarge,
            )));
        }
        let (rest, value) = direct_value(input, depth - 1)?;
        let (rest, _) = space(rest)?;
        Ok((rest, value))
    }
}

pub fn direct_object(input: ParserInput) -> Option<Object> {
    strip_nom(_direct_object(crate::reader::MAX_NESTING_DEPTH)(input))
}

fn object<'a>(
    input: ParserInput<'a>,
    reader: &Reader,
    already_seen: &mut HashSet<ObjectId>,
    recover_stream_length: bool,
    recovery_bound: Option<usize>,
) -> NomResult<'a, Object> {
    match stream(
        input,
        reader,
        already_seen,
        recover_stream_length,
        recovery_bound,
    ) {
        Ok(result) => Ok(result),
        Err(ParseErr::Failure(error)) => Err(ParseErr::Failure(error)),
        Err(_) => {
            let (rest, value) = direct_value(input, crate::reader::MAX_NESTING_DEPTH)?;
            let (rest, _) = space(rest)?;
            Ok((rest, value))
        }
    }
}

pub fn indirect_object(
    input: ParserInput,
    offset: usize,
    expected_id: Option<ObjectId>,
    reader: &Reader,
    already_seen: &mut HashSet<ObjectId>,
    recovery_bound: Option<usize>,
) -> crate::Result<(ObjectId, Object)> {
    // Every downstream slice is a suffix of `input`, so express the absolute
    // end offset as the maximum length a suffix may have.
    let recovery_bound = recovery_bound.map(|end| input.len().saturating_sub(end));
    let (id, mut object) = _indirect_object(
        &input[offset..],
        offset,
        expected_id,
        reader,
        already_seen,
        true,
        recovery_bound,
    )?;

    offset_stream(&mut object, offset);

    Ok((id, object))
}

fn _indirect_object<'a>(
    input: ParserInput<'a>,
    offset: usize,
    expected_id: Option<ObjectId>,
    reader: &Reader,
    already_seen: &mut HashSet<ObjectId>,
    recover_stream_length: bool,
    recovery_bound: Option<usize>,
) -> crate::Result<(ObjectId, Object)> {
    let (rest, _) = space(input).map_err(|_| Error::IndirectObject { offset })?;
    let (rest, object_id) =
        object_id(rest).map_err(|_| Error::IndirectObject { offset })?;
    let rest = rest
        .strip_prefix(b"obj")
        .ok_or(Error::IndirectObject { offset })?;
    let (rest, _) = space(rest).map_err(|_| Error::IndirectObject { offset })?;
    let (i, object_id) = (rest, object_id);
    if let Some(expected_id) = expected_id
        && object_id != expected_id
    {
        return Err(crate::error::Error::ObjectIdMismatch);
    }

    let object_offset = input.len() - i.len();
    let (rest, mut object) = object(
        i,
        reader,
        already_seen,
        recover_stream_length,
        recovery_bound,
    )
    .map_err(|_| Error::IndirectObject { offset })?;
    let (rest, _) = space(rest).map_err(|_| Error::IndirectObject { offset })?;
    let rest = rest.strip_prefix(b"endobj").unwrap_or(rest);
    let _ = space(rest).map_err(|_| Error::IndirectObject { offset })?;

    offset_stream(&mut object, object_offset);

    Ok((object_id, object))
}

pub fn header(input: ParserInput, strict: bool) -> Option<String> {
    if let Some(rest) = input.strip_prefix(b"%PDF-") {
        let mut version_end = 0;
        while rest
            .get(version_end)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            version_end += 1;
        }
        let line_end = rest
            .iter()
            .position(|byte| matches!(*byte, b'\r' | b'\n'))
            .unwrap_or(rest.len());
        if line_end == rest.len() {
            return None;
        }
        let trailing = &rest[version_end..line_end];
        if strict && !trailing.is_empty() {
            return None;
        }
        let version = &rest[..version_end];
        if version.is_empty() {
            return Some("1.7".to_string());
        }
        return str::from_utf8(version).ok().map(|version| version.to_string());
    }
    if strict || !input.starts_with(b"%PDF") {
        return None;
    }
    let line_end = input
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))
        .unwrap_or(input.len());
    (line_end < input.len()).then(|| "1.7".to_string())
}

pub fn binary_mark(input: ParserInput) -> Option<Vec<u8>> {
    let rest = input.strip_prefix(b"%")?;
    let end = rest
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))?;
    Some(rest[..end].to_vec())
}


/// Decode CrossReferenceTable
fn xref(input: ParserInput, strict: bool) -> NomResult<Xref> {
    let mut rest = input
        .strip_prefix(b"xref")
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    if rest.starts_with(b" ") {
        rest = &rest[1..];
    }
    rest = xref_eol(rest, strict)?;
    let mut xref = Xref::new(0, XrefType::CrossReferenceTable);
    loop {
        rest = space(rest)?.0;
        if rest.starts_with(b"trailer") {
            return Ok((rest, xref));
        }
        let (next, start) = unsigned_int::<usize>(rest)?;
        let Some(next) = next.strip_prefix(b" ") else {
            return Err(ParseErr::Error(NomError::from_error_kind(
                next,
                ErrorKind::Tag,
            )));
        };
        let (next, count) = unsigned_int::<u32>(next)?;
        let next = xref_eol(next.strip_prefix(b" ").unwrap_or(next), strict)?;
        rest = next;
        for index in 0..count as usize {
            let entry = match xref_entry(rest, strict) {
                Ok((next, entry)) => {
                    rest = next;
                    entry
                }
                Err(ParseErr::Error(_)) => break,
                Err(error) => return Err(error),
            };
            let ((offset, generation), is_normal) = entry;
            let Some(id) = start.checked_add(index).and_then(|id| u32::try_from(id).ok()) else {
                continue;
            };
            if is_normal {
                if let Ok(generation) = generation.try_into() {
                    xref.insert(id, XrefEntry::Normal { offset, generation });
                }
            } else {
                xref.insert(
                    id,
                    XrefEntry::Free {
                        generation: generation.try_into().unwrap_or(0),
                    },
                );
            }
        }
    }
}

fn xref_eol(input: ParserInput, strict: bool) -> Result<ParserInput, ParseErr<NomError<'_>>> {
    if let Some(rest) = input
        .strip_prefix(b" \r")
        .or_else(|| input.strip_prefix(b" \n"))
        .or_else(|| input.strip_prefix(b"\r\n"))
    {
        return Ok(rest);
    }
    if !strict {
        if let Some(rest) = input
            .strip_prefix(b"\n")
            .or_else(|| input.strip_prefix(b"\r"))
        {
            return Ok(rest);
        }
    }
    Err(ParseErr::Error(NomError::from_error_kind(
        input,
        ErrorKind::Tag,
    )))
}

fn xref_entry(
    input: ParserInput,
    strict: bool,
) -> Result<(ParserInput, ((u32, u32), bool)), ParseErr<NomError<'_>>> {
    let (rest, offset) = unsigned_int::<u32>(input)?;
    let Some(rest) = rest.strip_prefix(b" ") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            rest,
            ErrorKind::Tag,
        )));
    };
    let (rest, generation) = unsigned_int::<u32>(rest)?;
    let Some(rest) = rest.strip_prefix(b" ") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            rest,
            ErrorKind::Tag,
        )));
    };
    let (rest, is_normal) = match rest.first() {
        Some(b'n') => (&rest[1..], true),
        Some(b'f') => (&rest[1..], false),
        _ => {
            return Err(ParseErr::Error(NomError::from_error_kind(
                rest,
                ErrorKind::OneOf,
            )))
        }
    };
    let rest = xref_eol(rest, strict)?;
    Ok((rest, ((offset, generation), is_normal)))
}

fn trailer(input: ParserInput) -> NomResult<Dictionary> {
    let rest = input
        .strip_prefix(b"trailer")
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    let (rest, _) = space(rest)?;
    let (rest, dictionary) = direct_dictionary(rest, crate::reader::MAX_NESTING_DEPTH)?;
    let (rest, _) = space(rest)?;
    Ok((rest, dictionary))
}

pub fn xref_and_trailer(input: ParserInput, reader: &Reader) -> crate::Result<(Xref, Dictionary)> {
    if let Ok((rest, mut xref)) = xref(input, reader.strict) {
        if let Ok((_, trailer)) = trailer(rest) {
            match trailer.get(b"Size").and_then(Object::as_i64) {
                Ok(size) => xref.size = size as u32,
                Err(_) if !reader.strict => {
                    xref.size = xref.max_id().saturating_add(1);
                }
                Err(_) => return Err(error::ParseError::InvalidTrailer.into()),
            }
            return Ok((xref, trailer));
        }
    }
    let (_, object) =
        _indirect_object(input, 0, None, reader, &mut HashSet::new(), false, None)?;
    match object {
        Object::Stream(stream) => {
            decode_xref_stream_with_limit(stream, reader.max_decompressed_size)
        }
        _ => Err(crate::error::ParseError::InvalidXref.into()),
    }
}

fn trim_leading_spaces(mut rest: ParserInput) -> ParserInput {
    while rest.first() == Some(&b' ') {
        rest = &rest[1..];
    }
    rest
}

pub fn xref_start(input: ParserInput) -> Option<i64> {
    let rest = input.strip_prefix(b"startxref")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    let rest = consume_eol(rest)?;
    let rest = trim_leading_spaces(rest);
    let (rest, offset) = integer(rest).ok()?;
    let rest = trim_leading_spaces(rest);
    let rest = consume_eol(rest)?;
    let rest = rest.strip_prefix(b"%%EOF")?;
    let (_, _) = space(rest).ok()?;
    Some(offset)
}


// The following code create parser to parse content stream.

fn content_space(input: ParserInput) -> NomResult<()> {
    let rest = skip_content_space(input);
    Ok((rest, ()))
}

fn operator(input: ParserInput) -> NomResult<Cow<'static, str>> {
    let count = input
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || b"*'\"".contains(byte))
        .count();
    if count == 0 || input[0].is_ascii_digit() {
        return Err(ParseErr::Error(NomError::from_error_kind(input, ErrorKind::TakeWhile1)));
    }
    let text = str::from_utf8(&input[..count]).unwrap();
    Ok((&input[count..], intern_operator(text)))
}

/// Standard PDF operators borrow a static string; unknown operators keep
/// their text.  Content streams repeat a handful of operators, so this
/// removes a heap allocation from every operation.
fn intern_operator(operator: &str) -> Cow<'static, str> {
    match operator {
        "q" => Cow::Borrowed("q"),
        "Q" => Cow::Borrowed("Q"),
        "BT" => Cow::Borrowed("BT"),
        "ET" => Cow::Borrowed("ET"),
        "Tf" => Cow::Borrowed("Tf"),
        "Td" => Cow::Borrowed("Td"),
        "TD" => Cow::Borrowed("TD"),
        "Tm" => Cow::Borrowed("Tm"),
        "T*" => Cow::Borrowed("T*"),
        "Tj" => Cow::Borrowed("Tj"),
        "TJ" => Cow::Borrowed("TJ"),
        "Tc" => Cow::Borrowed("Tc"),
        "Tw" => Cow::Borrowed("Tw"),
        "Tz" => Cow::Borrowed("Tz"),
        "TL" => Cow::Borrowed("TL"),
        "Ts" => Cow::Borrowed("Ts"),
        "Tr" => Cow::Borrowed("Tr"),
        "'" => Cow::Borrowed("'"),
        "\"" => Cow::Borrowed("\""),
        "cm" => Cow::Borrowed("cm"),
        "w" => Cow::Borrowed("w"),
        "J" => Cow::Borrowed("J"),
        "j" => Cow::Borrowed("j"),
        "M" => Cow::Borrowed("M"),
        "d" => Cow::Borrowed("d"),
        "ri" => Cow::Borrowed("ri"),
        "i" => Cow::Borrowed("i"),
        "gs" => Cow::Borrowed("gs"),
        "m" => Cow::Borrowed("m"),
        "l" => Cow::Borrowed("l"),
        "c" => Cow::Borrowed("c"),
        "v" => Cow::Borrowed("v"),
        "y" => Cow::Borrowed("y"),
        "h" => Cow::Borrowed("h"),
        "re" => Cow::Borrowed("re"),
        "S" => Cow::Borrowed("S"),
        "s" => Cow::Borrowed("s"),
        "f" => Cow::Borrowed("f"),
        "F" => Cow::Borrowed("F"),
        "f*" => Cow::Borrowed("f*"),
        "B" => Cow::Borrowed("B"),
        "B*" => Cow::Borrowed("B*"),
        "b" => Cow::Borrowed("b"),
        "b*" => Cow::Borrowed("b*"),
        "n" => Cow::Borrowed("n"),
        "W" => Cow::Borrowed("W"),
        "W*" => Cow::Borrowed("W*"),
        "Do" => Cow::Borrowed("Do"),
        "BI" => Cow::Borrowed("BI"),
        "ID" => Cow::Borrowed("ID"),
        "EI" => Cow::Borrowed("EI"),
        "BMC" => Cow::Borrowed("BMC"),
        "BDC" => Cow::Borrowed("BDC"),
        "EMC" => Cow::Borrowed("EMC"),
        "MP" => Cow::Borrowed("MP"),
        "DP" => Cow::Borrowed("DP"),
        "BX" => Cow::Borrowed("BX"),
        "EX" => Cow::Borrowed("EX"),
        "g" => Cow::Borrowed("g"),
        "G" => Cow::Borrowed("G"),
        "rg" => Cow::Borrowed("rg"),
        "RG" => Cow::Borrowed("RG"),
        "k" => Cow::Borrowed("k"),
        "K" => Cow::Borrowed("K"),
        "cs" => Cow::Borrowed("cs"),
        "CS" => Cow::Borrowed("CS"),
        "sc" => Cow::Borrowed("sc"),
        "SC" => Cow::Borrowed("SC"),
        "scn" => Cow::Borrowed("scn"),
        "SCN" => Cow::Borrowed("SCN"),
        "sh" => Cow::Borrowed("sh"),
        "d0" => Cow::Borrowed("d0"),
        "d1" => Cow::Borrowed("d1"),
        _ => Cow::Owned(operator.to_string()),
    }
}

/// Decides between the real and integer grammar after one dot scan; the
/// selected parser then produces the exact same value as before.
#[inline]
fn number(input: ParserInput) -> NomResult<Object> {
    let bytes = input;
    let mut index = 0;
    let negative = match bytes.first() {
        Some(b'-') => {
            index = 1;
            true
        }
        Some(b'+') => {
            index = 1;
            false
        }
        _ => false,
    };
    let digits_start = index;
    let mut value = 0i64;
    while let Some(&byte) = bytes.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.wrapping_mul(10).wrapping_add(i64::from(byte - b'0'));
        index += 1;
    }
    if bytes.get(index) == Some(&b'.') {
        return real(input).map(|(rest, value)| (rest, Object::Real(value)));
    }
    let digits = index - digits_start;
    if digits > 0 && digits <= 18 {
        return Ok((
            &bytes[index..],
            Object::Integer(if negative { -value } else { value }),
        ));
    }
    integer(input).map(|(rest, value)| (rest, Object::Integer(value)))
}

#[inline]
fn operand_value(input: ParserInput) -> NomResult<Object> {
    match input.first() {
        Some(b'/') => {
            let (rest, name) = name_pdf(input)?;
            Ok((rest, Object::Name(name)))
        }
        Some(b'(') => {
            let (rest, string) = literal_string_pdf(input)?;
            Ok((rest, Object::String(string, StringFormat::Literal)))
        }
        Some(b'<') => {
            if input.get(1) == Some(&b'<') {
                let (rest, dict) = dictionary(input)?;
                Ok((rest, Object::Dictionary(dict)))
            } else {
                hexadecimal_string(input)
            }
        }
        Some(b'[') => {
            let (rest, array) = array(crate::reader::MAX_NESTING_DEPTH)(input)?;
            Ok((rest, Object::Array(array)))
        }
        Some(b't' | b'f') => boolean(input),
        Some(b'n') => null(input),
        Some(b'0'..=b'9' | b'+' | b'-' | b'.') => number(input),
        _ => Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Alt,
        ))),
    }
}

#[inline]
fn operand(input: ParserInput) -> NomResult<Object> {
    let (rest, object) = operand_value(input)?;
    let (rest, _) = content_space(rest)?;
    Ok((rest, object))
}

#[inline]
fn skip_content_space(input: ParserInput) -> ParserInput {
    let count = input
        .iter()
        .take_while(|byte| is_whitespace(**byte))
        .count();
    &input[count..]
}

fn operation(input: ParserInput) -> NomResult<Operation> {
    let mut operands = Vec::new();
    let (rest, operator) = operation_into(input, &mut operands)?;
    Ok((
        rest,
        Operation {
            operator,
            operands,
        },
    ))
}

/// Parses one operation, filling `out` with its operands.  Reusing the
/// caller's vector avoids a heap allocation per operation when a page is
/// interpreted as a stream.
#[inline]
fn skip_token_space(input: ParserInput) -> ParserInput {
    let count = input
        .iter()
        .take_while(|byte| is_whitespace(**byte))
        .count();
    &input[count..]
}

fn token_value(input: ParserInput, depth: usize) -> NomResult<Object> {
    match input.first() {
        Some(b'/') => {
            let (rest, name) = name_pdf(input)?;
            Ok((rest, Object::Name(name)))
        }
        Some(b'(') => {
            let (rest, string) = literal_string_pdf(input)?;
            Ok((rest, Object::String(string, StringFormat::Literal)))
        }
        Some(b'<') => {
            if input.get(1) == Some(&b'<') {
                let (rest, dict) = token_dictionary(input, depth)?;
                Ok((rest, Object::Dictionary(dict)))
            } else {
                hexadecimal_string(input)
            }
        }
        Some(b'[') => {
            let (rest, array) = token_array(input, depth)?;
            Ok((rest, Object::Array(array)))
        }
        Some(b't') if input.starts_with(b"true") => {
            Ok((&input[4..], Object::Boolean(true)))
        }
        Some(b'f') if input.starts_with(b"false") => {
            Ok((&input[5..], Object::Boolean(false)))
        }
        Some(b'n') if input.starts_with(b"null") => Ok((&input[4..], Object::Null)),
        Some(b'0'..=b'9' | b'+' | b'-' | b'.') => number(input),
        _ => Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Alt,
        ))),
    }
}

fn token_array(input: ParserInput, depth: usize) -> NomResult<Vec<Object>> {
    if depth == 0 {
        return Err(ParseErr::Failure(NomError::from_error_kind(
            input,
            ErrorKind::TooLarge,
        )));
    }
    let Some(rest) = input.strip_prefix(b"[") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut rest = skip_token_space(rest);
    let mut items = Vec::new();
    loop {
        if rest.starts_with(b"]") {
            return Ok((&rest[1..], items));
        }
        if rest.is_empty() {
            return Err(ParseErr::Error(NomError::from_error_kind(
                rest,
                ErrorKind::Tag,
            )));
        }
        let (next, item) = token_value(rest, depth - 1)?;
        items.push(item);
        rest = skip_token_space(next);
    }
}

fn token_dictionary(input: ParserInput, depth: usize) -> NomResult<Dictionary> {
    if depth == 0 {
        return Err(ParseErr::Failure(NomError::from_error_kind(
            input,
            ErrorKind::TooLarge,
        )));
    }
    let Some(rest) = input.strip_prefix(b"<<") else {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::Tag,
        )));
    };
    let mut rest = skip_token_space(rest);
    let mut dict = Dictionary::builder(4);
    loop {
        if rest.starts_with(b">>") {
            return Ok((&rest[2..], dict.finish()));
        }
        let (next, key) = name_pdf(rest)?;
        rest = skip_token_space(next);
        let (next, value) = token_value(rest, depth - 1)?;
        dict.set(key, value);
        rest = skip_token_space(next);
    }
}

fn token_operator(input: ParserInput) -> NomResult<Cow<'static, str>> {
    let count = input
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric() || matches!(**byte, b'*' | b'\'' | b'"')
        })
        .count();
    if count == 0 {
        return Err(ParseErr::Error(NomError::from_error_kind(
            input,
            ErrorKind::TakeWhile1,
        )));
    }
    let text = str::from_utf8(&input[..count]).unwrap();
    Ok((&input[count..], intern_operator(text)))
}

fn token_operation<'a>(
    input: ParserInput<'a>,
    out: &mut Vec<Object>,
) -> Option<NomResult<'a, Cow<'static, str>>> {
    let mut rest = input;
    while rest.first() == Some(&b'%') {
        let end = rest
            .iter()
            .position(|byte| matches!(*byte, b'\r' | b'\n'))
            .unwrap_or(rest.len());
        rest = skip_token_space(&rest[end..]);
    }
    if rest.starts_with(b"BI")
        && rest
            .get(2)
            .is_none_or(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    out.clear();
    loop {
        rest = skip_token_space(rest);
        let first = rest.first().copied()?;
        let is_word = matches!(
            first,
            b'/' | b'(' | b'<' | b'[' | b'+' | b'-' | b'.' | b'0'..=b'9'
        ) || (first == b't' && !rest.starts_with(b"T"))
            || (first == b'f' && !rest.starts_with(b"f*") && rest.get(1) != Some(&b'*'))
            || (first == b'n' && !rest.starts_with(b"n "));
        if !is_word || (first == b'n' && !rest.starts_with(b"null")) {
            let (next, operator) = match token_operator(rest) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            return Some(Ok((skip_token_space(next), operator)));
        }
        let (next, value) = match token_value(rest, crate::reader::MAX_NESTING_DEPTH) {
            Ok(value) => value,
            Err(_) => {
                let (next, operator) = match token_operator(rest) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
                return Some(Ok((skip_token_space(next), operator)));
            }
        };
        out.push(value);
        rest = next;
    }
}

fn operation_into<'a>(
    input: ParserInput<'a>,
    out: &mut Vec<Object>,
) -> NomResult<'a, Cow<'static, str>> {
    match token_operation(input, out) {
        Some(result) => result,
        None => operation_into_nom(input, out),
    }
}

fn operation_into_nom<'a>(
    input: ParserInput<'a>,
    out: &mut Vec<Object>,
) -> NomResult<'a, Cow<'static, str>> {
    let mut rest = input;
    // A comment consumes its end-of-line marker, so also skip any white space
    // that follows it (e.g. an indented next line).  Comments are rare; a
    // first-byte check keeps the nom fold off the common path.
    while rest.first() == Some(&b'%') {
        match comment(rest) {
            Ok((next, ())) => rest = skip_content_space(next),
            Err(ParseErr::Error(_)) => break,
            Err(error) => return Err(error),
        }
    }
    match inline_image(rest) {
        Ok((rest, (mut operands, operator))) => {
            out.clear();
            out.append(&mut operands);
            return Ok((rest, operator));
        }
        Err(ParseErr::Error(_)) => {}
        Err(error) => return Err(error),
    }
    out.clear();
    loop {
        let len = rest.len();
        match rest.first() {
            Some(b'/') => {
                let (next, name) = name_pdf(rest)?;
                out.push(Object::Name(name));
                rest = skip_content_space(next);
            }
            Some(b'(') => {
                let (next, string) = literal_string_pdf(rest)?;
                out.push(Object::String(string, StringFormat::Literal));
                rest = skip_content_space(next);
            }
            Some(b'0'..=b'9' | b'+' | b'-' | b'.') => {
                let (next, number) = number(rest)?;
                out.push(number);
                rest = skip_content_space(next);
            }
            _ => match operand(rest) {
                Ok((next, value)) => {
                    if next.len() == len {
                        return Err(ParseErr::Error(NomError::from_error_kind(
                            rest,
                            ErrorKind::Many0,
                        )));
                    }
                    out.push(value);
                    rest = next;
                }
                Err(ParseErr::Error(_)) => break,
                Err(error) => return Err(error),
            },
        }
    }
    let (rest, operator) = operator(rest)?;
    let (rest, _) = content_space(rest)?;
    Ok((rest, operator))
}

fn inline_image(input: ParserInput) -> NomResult<(Vec<Object>, Cow<'static, str>)> {
    let rest = input
        .strip_prefix(b"BI")
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    let rest = skip_content_space(rest);
    if rest.is_empty() {
        return Err(ParseErr::Failure(NomError::from_error_kind(
            rest,
            ErrorKind::Eof,
        )));
    }
    inline_image_impl(rest)
}

/// The single white-space character between `ID` and the image data (ISO 32000-2, 8.9.7),
/// with a CR LF pair read as one end-of-line. White space after it is image data: a gray
/// level of 32 is a space byte.
fn inline_image_data_separator(input: ParserInput) -> NomResult<()> {
    if let Some(rest) = consume_eol(input) {
        return Ok((rest, ()));
    }
    if input.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        return Ok((&input[1..], ()));
    }
    Ok((input, ()))
}

fn inline_image_impl(input: ParserInput) -> NomResult<(Vec<Object>, Cow<'static, str>)> {
    let (input, stream_dict) = inner_dictionary(crate::reader::MAX_NESTING_DEPTH)(input)?;
    let input = input
        .strip_prefix(b"ID")
        .ok_or_else(|| ParseErr::Error(NomError::from_error_kind(input, ErrorKind::Tag)))?;
    let (input, _) = inline_image_data_separator(input)?;
    match image_data_stream(input, stream_dict) {
        Ok((input, stream)) => {
            let input = skip_content_space(input);
            let input = input
                .strip_prefix(b"EI")
                .ok_or_else(|| ParseErr::Failure(NomError::from_error_kind(input, ErrorKind::Tag)))?;
            let input = skip_content_space(input);
            Ok((input, (vec![Object::Stream(stream)], Cow::Borrowed("BI"))))
        }
        Err(e) => {
            // Skip to EI marker so the rest of the content stream can still be parsed.
            log::warn!("Skipping unparseable inline image: {e}");
            let bytes = input;
            // EI must appear after whitespace to distinguish from data bytes.
            let ei_pos = bytes
                .windows(4)
                .position(|w| {
                    (w[0] == b' ' || w[0] == b'\n' || w[0] == b'\r')
                        && w[1] == b'E'
                        && w[2] == b'I'
                        && (w[3] == b' ' || w[3] == b'\n' || w[3] == b'\r')
                })
                .ok_or_else(|| {
                    ParseErr::Failure(NomError::from_error_kind(input, ErrorKind::Fail))
                })?;
            let input = &input[ei_pos + 3..];
            let input = skip_content_space(input);
            Ok((input, (vec![], Cow::Borrowed("BI"))))
        }
    }
}

fn image_data_stream(
    input: ParserInput,
    stream_dict: Dictionary,
) -> crate::Result<(ParserInput, Stream)> {
    let get_abbr =
        |key_abbr: &[u8], key: &[u8]| stream_dict.get(key_abbr).or_else(|_| stream_dict.get(key));
    let invalid_size = || Error::InvalidInlineImage(String::from("image size out of range"));
    let get_size = |key_abbr: &[u8], key: &[u8]| -> crate::Result<usize> {
        usize::try_from(get_abbr(key_abbr, key)?.as_i64()?).map_err(|_| invalid_size())
    };
    let width = get_size(b"W", b"Width")?;
    let height = get_size(b"H", b"Height")?;
    let bpc = get_size(b"BPC", b"BitsPerComponent")?;
    let im = get_abbr(b"IM", b"ImageMask").and_then(|x| x.as_bool());
    let image_mask = im.unwrap_or(false);
    let colorspace = if image_mask {
        None
    } else {
        Some(get_abbr(b"CS", b"ColorSpace")?.clone())
    };
    let num_colors = match &colorspace {
        None => 1,
        Some(Object::Array(array))
            if matches!(
                array.first().map(Object::as_name),
                Some(Ok(b"Indexed" | b"I"))
            ) =>
        {
            1
        }
        Some(colorspace) => match colorspace.as_name()? {
            b"DeviceGray" | b"G" | b"Gray" => 1,
            b"DeviceRGB" | b"RGB" => 3,
            b"DeviceRGBA" | b"RGBA" => 4,
            b"DeviceCMYK" | b"CMYK" => 4,
            b"Pattern" => {
                log::warn!("Pattern colorspace is not allowed in inline images");
                return Err(Error::InvalidInlineImage(String::from(
                    "Pattern colorspace is not allowed in inline images",
                )));
            }
            _ => {
                log::warn!("Colorspace of inline image not recognized / not yet implemented");
                return Err(Error::Unimplemented("inline image colorspaces"));
            }
        },
    };

    let length = bpc
        .checked_mul(num_colors)
        .and_then(|bits_per_pixel| width.checked_mul(bits_per_pixel))
        .and_then(|row_bits| row_bits.div_ceil(8).checked_mul(height))
        .ok_or_else(invalid_size)?;

    // Build a full-key dictionary: the content interpreter and the stream
    // decoders use the long PDF names, while inline images abbreviate them.
    let mut full = Dictionary::new();
    full.set("Width", Object::Integer(width as i64));
    full.set("Height", Object::Integer(height as i64));
    full.set(
        "BitsPerComponent",
        Object::Integer(if image_mask { 1 } else { bpc as i64 }),
    );
    if image_mask {
        full.set("ImageMask", Object::Boolean(true));
    } else if let Some(colorspace) = &colorspace {
        full.set("ColorSpace", expand_inline_colorspace(colorspace));
    }
    let filter = get_abbr(b"F", b"Filter").ok().map(expand_inline_filter);
    if let Some(filter) = &filter {
        full.set("Filter", filter.clone());
    }
    if let Ok(parms) = get_abbr(b"DP", b"DecodeParms") {
        full.set("DecodeParms", parms.clone());
    }
    if let Ok(decode) = get_abbr(b"D", b"Decode") {
        full.set("Decode", decode.clone());
    }
    if let Ok(interpolate) = get_abbr(b"I", b"Interpolate") {
        full.set("Interpolate", interpolate.clone());
    }

    let (input, content) = match filter {
        None => {
            if input.len() < length {
                return Err(crate::error::ParseError::EndOfInput.into());
            }
            (&input[length..], &input[..length])
        }
        Some(_) => {
            // With a filter the compressed length is unknown; the data ends
            // at the first whitespace-framed `EI` marker.
            let end = find_inline_image_end(input)
                .ok_or_else(|| Error::InvalidInlineImage(String::from("missing EI")))?;
            let mut data = &input[..end];
            while data.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
                data = &data[..data.len() - 1];
            }
            (&input[end..], data)
        }
    };
    Ok((input, Stream::new(full, content.to_vec())))
}

fn expand_inline_filter(filter: &Object) -> Object {
    let expand = |name: &[u8]| -> Vec<u8> {
        match name {
            b"Fl" => b"FlateDecode".to_vec(),
            b"AHx" => b"ASCIIHexDecode".to_vec(),
            b"A85" => b"ASCII85Decode".to_vec(),
            b"RL" => b"RunLengthDecode".to_vec(),
            b"LZW" => b"LZWDecode".to_vec(),
            b"CCF" => b"CCITTFaxDecode".to_vec(),
            b"DCT" => b"DCTDecode".to_vec(),
            b"JPX" => b"JPXDecode".to_vec(),
            other => other.to_vec(),
        }
    };
    match filter {
        Object::Name(name) => Object::Name(expand(name).into()),
        Object::Array(filters) => Object::Array(
            filters
                .iter()
                .map(|filter| match filter {
                    Object::Name(name) => Object::Name(expand(name).into()),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn expand_inline_colorspace(colorspace: &Object) -> Object {
    match colorspace {
        Object::Name(name) => match name.as_slice() {
            b"G" | b"Gray" => Object::Name(b"DeviceGray".to_vec().into()),
            b"RGB" => Object::Name(b"DeviceRGB".to_vec().into()),
            b"CMYK" => Object::Name(b"DeviceCMYK".to_vec().into()),
            other => Object::Name(other.to_vec().into()),
        },
        Object::Array(array) => {
            let mut array = array.clone();
            if let Some(Object::Name(name)) = array.first() {
                if name == b"I" || name == b"Indexed" {
                    array[0] = Object::Name(if name == b"I" {
                        b"Indexed".to_vec().into()
                    } else {
                        name.clone()
                    });
                } else {
                    array[0] = expand_inline_colorspace(&array[0].clone());
                }
            }
            Object::Array(array)
        }
        other => other.clone(),
    }
}

fn find_inline_image_end(input: ParserInput) -> Option<usize> {
    let bytes = input;
    let is_space = |byte: u8| byte.is_ascii_whitespace();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index] == b'E'
            && bytes[index + 1] == b'I'
            && (index == 0 || is_space(bytes[index - 1]))
            && (index + 2 >= bytes.len() || is_space(bytes[index + 2]))
        {
            return Some(index);
        }
    }
    None
}

fn skip_trailing_content_space(mut rest: ParserInput) -> ParserInput {
    loop {
        rest = skip_content_space(rest);
        if rest.first() != Some(&b'%') {
            return rest;
        }
        let end = rest
            .iter()
            .position(|byte| matches!(*byte, b'\r' | b'\n'))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
}

fn _content(input: ParserInput) -> NomResult<Content<Vec<Operation>>> {
    let mut rest = input;
    let mut operations = Vec::with_capacity(input.len() / 32);
    let mut operands = Vec::new();
    loop {
        let len = rest.len();
        match operation_into(rest, &mut operands) {
            Ok((next, operator)) => {
                if next.len() == len {
                    return Err(ParseErr::Error(NomError::from_error_kind(
                        rest,
                        ErrorKind::Many0,
                    )));
                }
                operations.push(Operation {
                    operator,
                    operands: std::mem::take(&mut operands),
                });
                rest = next;
            }
            Err(ParseErr::Error(_)) => break,
            Err(error) => return Err(error),
        }
    }
    let rest = skip_trailing_content_space(rest);
    Ok((rest, Content { operations }))
}


pub fn content(input: ParserInput) -> Option<Content<Vec<Operation>>> {
    strip_nom(_content(input))
}

/// The operations of a content stream, decoded one at a time with the grammar of
/// [`content`].
///
/// Yields exactly the operations `content` collects, in order, then an error where
/// `content` fails, so the operations of a large stream need not all be held at once.
pub struct ContentOperations<'a> {
    input: ParserInput<'a>,
    started: bool,
    finished: bool,
}

impl<'a> ContentOperations<'a> {
    pub(crate) fn new(input: ParserInput<'a>) -> Self {
        ContentOperations {
            input,
            started: false,
            finished: false,
        }
    }

    fn fail(&mut self) -> Option<crate::Result<Operation>> {
        self.finished = true;
        Some(Err(error::ParseError::InvalidContentStream.into()))
    }

    /// Parses the next operation into `operands` and returns its operator.
    /// The vector is cleared first, so one allocation can serve a whole
    /// stream.  `Err` means the stream is malformed, matching [`Iterator`].
    pub fn next_into(
        &mut self,
        operands: &mut Vec<Object>,
    ) -> Option<crate::Result<Cow<'static, str>>> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.started = true;
            match content_space(self.input) {
                Ok((input, ())) => self.input = input,
                Err(_) => {
                    self.finished = true;
                    return Some(Err(error::ParseError::InvalidContentStream.into()));
                }
            }
        }
        let len = self.input.len();
        match operation_into(self.input, operands) {
            Ok((input, operator)) => {
                if input.len() == len {
                    self.finished = true;
                    return Some(Err(error::ParseError::InvalidContentStream.into()));
                }
                self.input = input;
                Some(Ok(operator))
            }
            Err(ParseErr::Error(_)) => {
                self.finished = true;
                None
            }
            Err(_) => {
                self.finished = true;
                Some(Err(error::ParseError::InvalidContentStream.into()))
            }
        }
    }
}

impl Iterator for ContentOperations<'_> {
    type Item = crate::Result<Operation>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.started = true;
            match content_space(self.input) {
                Ok((input, ())) => self.input = input,
                Err(_) => return self.fail(),
            }
        }
        let len = self.input.len();
        let mut operands = Vec::new();
        match operation_into(self.input, &mut operands) {
            Ok((input, operator)) if input.len() == len => self.fail(),
            Ok((input, operator)) => {
                self.input = input;
                Some(Ok(Operation { operator, operands }))
            }
            Err(ParseErr::Error(_)) => {
                self.finished = true;
                None
            }
            Err(_) => self.fail(),
        }
    }
}

pub fn content_strict(input: ParserInput) -> Result<Content<Vec<Operation>>, error::ParseError> {
    let (rest, content) =
        _content(input).map_err(|_| error::ParseError::InvalidContentStream)?;
    if !rest.is_empty() {
        return Err(error::ParseError::InvalidContentStream);
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_span(s: &'_ [u8]) -> ParserInput<'_> {
        s
    }

    fn tstrip<O>(r: NomResult<O>) -> Option<O> {
        r.ok()
            .and_then(|(i, o)| if !i.is_empty() { None } else { Some(o) })
    }

    #[test]
    fn content_operations_match_content() {
        let streams: [&[u8]; 6] = [
            b"",
            b"q 1 0 0 1 72 720 cm BT /F1 12 Tf [(A) -120 (B)] TJ ET Q",
            b"  % leading comment\n0 0 m 10 10 l S\n% trailing comment\n",
            b"BT /F1 12 Tf (text) Tj ET\n) unparsable trailing input is ignored",
            b"BI /W 1 /H 1 /BPC 8 /CS /G ID X EI 0 0 m",
            b"/P <</MCID 0>> BDC BT (marked) Tj ET EMC",
        ];
        for stream in streams {
            let expected = content(test_span(stream)).expect("content decodes");
            let operations = ContentOperations::new(test_span(stream))
                .collect::<crate::Result<Vec<_>>>()
                .expect("operations decode");
            assert_eq!(
                format!("{:?}", operations),
                format!("{:?}", expected.operations),
                "stream: {:?}",
                String::from_utf8_lossy(stream)
            );
        }
    }

    #[test]
    fn content_operations_fail_where_content_fails() {
        // Without EI the inline image cannot be skipped, so the whole decode fails.
        let stream =
            test_span(b"BT (before) Tj ET\nBI /W 1 /H 1 /BPC 8 /CS /G ID X\nBT (after) Tj ET");
        assert!(content(stream).is_none());

        let mut operations = ContentOperations::new(stream);
        for operator in ["BT", "Tj", "ET"] {
            assert_eq!(operations.next().unwrap().unwrap().operator, operator);
        }
        assert!(matches!(
            operations.next(),
            Some(Err(Error::Parse(error::ParseError::InvalidContentStream)))
        ));
        assert!(operations.next().is_none());
    }

    #[test]
    fn parse_real_number() {
        let real = |i| tstrip(real(i));

        assert_eq!(real(test_span(b"0.12")), Some(0.12));
        assert_eq!(real(test_span(b"-.12")), Some(-0.12));
        assert_eq!(real(test_span(b"10.")), Some(10.0));
        // A minus sign inside the fraction is ignored.
        assert_eq!(real(test_span(b"0.00-4142502")), Some(0.004142502));
        assert_eq!(real(test_span(b"-1.5-5")), Some(-1.55));
    }

    #[test]
    fn parse_string() {
        let literal_string = |i| tstrip(literal_string(i));

        let data = vec![
            ("()", ""),
            ("(text())", "text()"),
            (
                "(text\r\n\\\\(nested\\t\\b\\f))",
                "text\n\\(nested\t\x08\x0C)",
            ),
            // An unescaped end-of-line marker of any kind reads as a single line feed.
            ("(a\rb\nc\r\n\r\nd\n\re)", "a\nb\nc\n\nd\n\ne"),
            ("(text\\0\\53\\053\\0053)", "text\0++\x053"),
            ("(text line\\\n())", "text line()"),
        ];

        for (input, expected) in data {
            assert_eq!(
                literal_string(test_span(input.as_bytes())),
                Some(expected.as_bytes().to_vec()),
                "input: {:?} output: {:?}",
                input,
                expected,
            );
        }
    }

    #[test]
    fn parse_name() {
        let (text, expected) = (b"/ABC#5f", b"ABC\x5F");
        let result = tstrip(name(test_span(text)));
        assert_eq!(result, Some(expected.to_vec()));

        let (text, expected) = (b"/#cb#ce#cc#e5", b"\xcb\xce\xcc\xe5");
        let result = tstrip(name(test_span(text)));
        assert_eq!(result, Some(expected.to_vec()));
    }

    #[test]
    /// Run `cargo test -- --nocapture` to see output
    fn parse_content() {
        let stream = b"
2 J
BT
/F1 12 Tf
0 Tc
0 Tw
72.5 712 TD
[(Unencoded streams can be read easily) 65 (,) ] TJ
0 -14 TD
[(b) 20 (ut generally tak) 10 (e more space than \\311)] TJ
T* (encoded streams.) Tj
		";
        let content = tstrip(_content(test_span(stream)));
        assert!(content.is_some());
    }

    #[test]
    fn hex_partial() {
        // Example from PDF specification.
        let out = tstrip(hexadecimal_string(test_span(b"<901FA>")));

        match out {
            Some(Object::String(s, _)) => assert_eq!(s, b"\x90\x1F\xA0".to_vec()),
            _ => panic!("unexpected {:?}", out),
        }
    }

    #[test]
    fn hex_separated() {
        let out = tstrip(hexadecimal_string(test_span(b"<9 01F A>")));

        match out {
            Some(Object::String(s, _)) => assert_eq!(s, b"\x90\x1F\xA0".to_vec()),
            _ => panic!("unexpected {:?}", out),
        }
    }

    #[test]
    fn big_generation_value() {
        let input = b"xref
0 1
0000000000 65536 f\x20
0 16
0000000000 65535 f\x20
0000153238 00000 n\x20
0000000019 00000 n\x20
0000000313 00000 n\x20
0000000333 00000 n\x20
0000145531 00000 n\x20
0000153407 00000 n\x20
0000145554 00000 n\x20
0000152303 00000 n\x20
0000152324 00000 n\x20
0000152514 00000 n\x20
0000152880 00000 n\x20
0000153106 00000 n\x20
0000153139 00000 n\x20
0000153532 00000 n\x20
0000153629 00000 n\x20
trailer
<</Size 16/Root 14 0 R
/Info 15 0 R
/ID [ <9DDC4B621B3F485FF5ED0F57D00A028F>
<9DDC4B621B3F485FF5ED0F57D00A028F> ]
/DocChecksum /2BCC3C7DE26E6BF3573E4A6E8362221F
>>
startxref
153804\x20
%%EOF
";
        match xref(test_span(input), false) {
            Ok((_, re)) => assert_eq!(re.entries.len(), 15),
            Err(err) => panic!("unexpected {:?}", err),
        }
    }

    #[test]
    fn space_in_startxref_number() {
        let input = b"startxref
153804\x20
%%EOF
";
        match xref_start(test_span(input)) {
            Some(num) => assert_eq!(num, 153804),
            None => panic!("could not parse number in startxref"),
        }
    }

    #[test]
    fn header_standard() {
        // Standard header with proper EOL
        let input = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n";
        assert_eq!(header(test_span(input), false), Some("1.7".to_string()));
    }

    #[test]
    fn header_with_binary_bytes_on_same_line() {
        // Some generators (e.g. ImageMill) place binary marker bytes on the
        // header line without a separating newline or '%' prefix.
        let input = b"%PDF-1.3 \xb0\x9f\x92\x9c\x9f\xd4\xe0\xce\xd0\xd0\xd0\r1 0 obj\r";
        assert_eq!(header(test_span(input), false), Some("1.3".to_string()));
    }

    #[test]
    fn header_with_binary_bytes_strict_rejects() {
        // In strict mode, binary bytes on the header line should cause a
        // parse failure (the raw bytes are not valid UTF-8).
        let input = b"%PDF-1.3 \xb0\x9f\x92\x9c\x9f\xd4\xe0\xce\xd0\xd0\xd0\r1 0 obj\r";
        assert_eq!(header(test_span(input), true), None);
    }

    #[test]
    fn header_cr_line_ending() {
        // CR-only line ending (common in older PDFs)
        let input = b"%PDF-1.3\r%\xe2\xe3\xcf\xd3\r";
        assert_eq!(header(test_span(input), false), Some("1.3".to_string()));
    }

    #[test]
    fn header_crlf_line_ending() {
        // CRLF line ending (common on Windows-generated PDFs)
        let input = b"%PDF-1.7\r\n%\xe2\xe3\xcf\xd3\r\n";
        assert_eq!(header(test_span(input), false), Some("1.7".to_string()));
    }

    #[test]
    fn header_pdf_2_0() {
        let input = b"%PDF-2.0\n%\xe2\xe3\xcf\xd3\n";
        assert_eq!(header(test_span(input), false), Some("2.0".to_string()));
    }

    #[test]
    fn content_with_comments() {
        // It should be processed as usual but ignoring the comments
        let input = b"0.5 0.5 0.5 setrgbcolor
% This is a comment
100 100 moveto
(Hello, world!) show
% Another comment
";
        let out = content(test_span(input)).unwrap();
        let out_strict = content_strict(test_span(input)).unwrap();
        assert_eq!(out.operations.len(), out_strict.operations.len());
        assert_eq!(out.operations.len(), 3);
    }

    #[test]
    fn content_with_comment_followed_by_indented_line() {
        // A comment is a single whitespace character (ISO 32000-2, 7.2.4), so an indented
        // line right after one must not stop the parser (issue #535).
        let input = b"BT /F1 24 Tf
% comment
  100 100 Td (Hello World) Tj ET";
        let out = content(test_span(input)).unwrap();
        let out_strict = content_strict(test_span(input)).unwrap();
        assert_eq!(out.operations.len(), out_strict.operations.len());
        let ops: Vec<&str> = out.operations.iter().map(|o| o.operator.as_ref()).collect();
        assert_eq!(ops, vec!["BT", "Tf", "Td", "Tj", "ET"]);
    }

    #[test]
    fn inline_image_unknown_colorspace_skipped() {
        // Inline image with an unrecognized colorspace ("ICCBased" is not handled).
        // The parser should skip it and still parse the surrounding operations.
        let input = b"q 100 100 moveto
BI /W 2 /H 2 /CS /ICCBased /BPC 8
ID
\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00
EI
(Hello) Tj Q";
        let out = content(test_span(input)).unwrap();
        // Should have: q, moveto, BI (skipped), Tj, Q = 5 operations
        let ops: Vec<&str> = out.operations.iter().map(|o| o.operator.as_ref()).collect();
        assert!(ops.contains(&"q"), "missing q, got: {:?}", ops);
        assert!(ops.contains(&"Tj"), "missing Tj, got: {:?}", ops);
        assert!(ops.contains(&"Q"), "missing Q, got: {:?}", ops);
    }

    #[test]
    fn inline_image_with_invalid_dimensions_skipped() {
        // Negative dimensions or a byte count that doesn't fit in usize must not overflow.
        for dims in [
            &b"/W -1 /H 1 /BPC 8"[..],
            b"/W 4294967296 /H 4294967296 /BPC 8",
            b"/W 1 /H 1 /BPC 9223372036854775807",
        ] {
            let mut input = b"q BI ".to_vec();
            input.extend_from_slice(dims);
            input.extend_from_slice(b" /CS /DeviceRGB ID \x00\x00\x00 EI (Hello) Tj Q");
            let out = content(test_span(&input)).unwrap();
            let ops: Vec<&str> = out.operations.iter().map(|o| o.operator.as_ref()).collect();
            assert_eq!(ops, vec!["q", "BI", "Tj", "Q"]);
            assert!(out.operations[1].operands.is_empty());
        }
    }

    #[test]
    fn inline_image() {
        let _ = env_logger::try_init();
        let input = b"BI /W 4 /H 4 /CS /RGB /BPC 8
ID
00000z0z00zzz00z0zzz0zzzEI aazazaazzzaazazzzazzz
EI";
        let out = super::inline_image(test_span(input)).unwrap().1;
        assert_eq!(&out.1, "BI");
        assert_eq!(
            &out.0[0].as_stream().unwrap().content,
            b"00000z0z00zzz00z0zzz0zzzEI aazazaazzzaazazzzazzz"
        )
    }

    #[test]
    fn inline_image_abbreviated_color_spaces() {
        // 2x1 images using the abbreviated colour space names allowed in inline images.
        for (color_space, data) in [
            (&b"/G"[..], &b"\x00\xff"[..]),
            (b"/RGB", b"\x00\x00\x00\xff\xff\xff"),
            (b"/CMYK", b"\x00\x00\x00\x00\xff\xff\xff\xff"),
            (b"[/I /RGB 1 <000000FFFFFF>]", b"\x00\x01"),
            (b"[/Indexed /DeviceRGB 1 <000000FFFFFF>]", b"\x00\x01"),
        ] {
            let mut input = b"q BI /W 2 /H 1 /BPC 8 /CS ".to_vec();
            input.extend_from_slice(color_space);
            input.extend_from_slice(b" ID ");
            input.extend_from_slice(data);
            input.extend_from_slice(b" EI Q");
            let out = content(test_span(&input)).unwrap();
            let ops: Vec<&str> = out.operations.iter().map(|o| o.operator.as_ref()).collect();
            assert_eq!(ops, vec!["q", "BI", "Q"]);
            let image = out.operations[1].operands[0].as_stream().unwrap();
            assert_eq!(image.content, data);
        }
    }

    #[test]
    fn inline_image_data_starting_with_white_space() {
        // A 3x1 gray image whose three pixels have level 32, i.e. space bytes. Only the
        // first white-space character after ID separates it from the data.
        let input = b"q\rBI\r/CS/DeviceGray/W 3/H 1/BPC 8\rID\r   \nEI\rQ\r";
        let out = content_strict(test_span(input)).unwrap();
        let ops: Vec<&str> = out.operations.iter().map(|o| o.operator.as_ref()).collect();
        assert_eq!(ops, vec!["q", "BI", "Q"]);
        assert_eq!(
            &out.operations[1].operands[0].as_stream().unwrap().content,
            b"   "
        );
    }

    #[test]
    fn inline_image_data_after_crlf() {
        // A CR LF after ID is one end-of-line, so the data starts after the LF.
        let input = b"BI /CS /DeviceGray /W 2 /H 1 /BPC 8 ID\r\n\n\x80\nEI";
        let out = super::inline_image(test_span(input)).unwrap().1;
        assert_eq!(&out.0[0].as_stream().unwrap().content, b"\n\x80");
    }

    #[test]
    fn xref_trailing_space_after_keyword() {
        // Some PDF generators emit "xref \n" with a trailing space.
        let input = b"xref \n0 3\n0000000000 65535 f \n0000000017 00000 n \n0000000081 00000 n \ntrailer\n<</Size 3/Root 1 0 R>>\nstartxref\n175\n%%EOF\n";
        match xref(test_span(input), false) {
            Ok((_, re)) => assert_eq!(re.entries.len(), 2),
            Err(err) => panic!("xref with trailing space should parse: {:?}", err),
        }
    }

    #[test]
    fn xref_entries_with_bare_eol_terminator() {
        // 19-byte entries with a bare "\n" terminator are non-conforming but accepted by
        // qpdf, pikepdf, PDFium and PDF.js, so lenient parsing accepts them too.
        for (name, input) in [
            (
                "bare LF",
                &b"xref\n0 3\n0000000000 65535 f\n0000000017 00000 n\n0000000081 00000 n\ntrailer\n<</Size 3/Root 1 0 R>>\n"[..],
            ),
            (
                "bare CR",
                &b"xref\n0 3\n0000000000 65535 f\r0000000017 00000 n\r0000000081 00000 n\rtrailer\n<</Size 3/Root 1 0 R>>\n"[..],
            ),
        ] {
            match xref(test_span(input), false) {
                Ok((_, re)) => assert_eq!(re.entries.len(), 2, "{name} should yield both normal entries"),
                Err(err) => panic!("19-byte entries ({name}) should parse when lenient: {err:?}"),
            }
        }
    }

    #[test]
    fn xref_entries_with_bare_eol_rejected_when_strict() {
        // Rejection is indirect: the entries are not recognised, leaving an empty section
        // whose unconsumed lines then displace `trailer`.
        let input =
            b"xref\n0 3\n0000000000 65535 f\n0000000017 00000 n\n0000000081 00000 n\ntrailer\n<</Size 3/Root 1 0 R>>\n";
        if let Ok((_, re)) = xref(test_span(input), true) {
            assert_eq!(
                re.entries.len(),
                0,
                "strict must not accept 19-byte entries"
            );
        }

        // The 20-byte build of the same document is the control: it must load in both
        // modes, so any strict rejection is attributable to the terminator alone.
        let load = |bytes: &[u8], strict: bool| {
            crate::Document::load_mem_with_options(
                bytes,
                crate::LoadOptions {
                    strict,
                    ..Default::default()
                },
            )
        };

        for (name, term, strict_should_load) in [("19-byte", "\n", false), ("20-byte", " \n", true)]
        {
            let header = "%PDF-1.7\n";
            let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
            let obj2 = "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n";
            let obj3 = "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n";
            let o1 = header.len();
            let o2 = o1 + obj1.len();
            let o3 = o2 + obj2.len();
            let body = format!("{header}{obj1}{obj2}{obj3}");
            let xref_pos = body.len();
            let doc = format!(
                "{body}xref\n0 4\n0000000000 65535 f{term}{o1:010} 00000 n{term}{o2:010} 00000 n{term}\
                 {o3:010} 00000 n{term}trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n"
            );

            assert!(
                load(doc.as_bytes(), false).is_ok(),
                "{name} entries should load when lenient"
            );
            assert_eq!(
                load(doc.as_bytes(), true).is_ok(),
                strict_should_load,
                "{name} entries under strict parsing"
            );
        }
    }

    #[test]
    fn xref_entries_with_conforming_terminators() {
        // The three 20-byte terminators of s7.5.4 must keep parsing in both modes.
        for (name, input) in [
            (
                "SP LF",
                &b"xref\n0 3\n0000000000 65535 f \n0000000017 00000 n \n0000000081 00000 n \ntrailer\n<</Size 3/Root 1 0 R>>\n"[..],
            ),
            (
                "SP CR",
                &b"xref\n0 3\n0000000000 65535 f \r0000000017 00000 n \r0000000081 00000 n \rtrailer\n<</Size 3/Root 1 0 R>>\n"[..],
            ),
            (
                "CR LF",
                &b"xref\n0 3\n0000000000 65535 f\r\n0000000017 00000 n\r\n0000000081 00000 n\r\ntrailer\n<</Size 3/Root 1 0 R>>\n"[..],
            ),
        ] {
            for strict in [false, true] {
                match xref(test_span(input), strict) {
                    Ok((_, re)) => assert_eq!(re.entries.len(), 2, "{name} (strict={strict}) lost an entry"),
                    Err(err) => panic!("conforming {name} entries should parse (strict={strict}): {err:?}"),
                }
            }
        }
    }

    #[test]
    fn xref_subsection_start_near_usize_max_is_skipped() {
        // A start of usize::MAX made `start + index` overflow: a panic where overflow checks
        // are on, and a silent wrap to object 0 where they are not.
        let input = &b"xref\n18446744073709551615 2\n0000000000 65535 f \n0000000009 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\n"[..];
        for strict in [false, true] {
            match xref(test_span(input), strict) {
                Ok((_, re)) => assert!(
                    re.entries.is_empty(),
                    "unrepresentable object number was inserted (strict={strict}): {:?}",
                    re.entries
                ),
                Err(err) => panic!("xref should still parse (strict={strict}): {err:?}"),
            }
        }
    }

    #[test]
    fn xref_subsection_start_beyond_u32_does_not_displace_entries() {
        // A start above u32::MAX truncated into a valid-looking number (4294967297 becomes 1),
        // overwriting a legitimate entry. No overflow occurs, so a checked add alone misses it.
        let input = &b"xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \n4294967297 1\n0000000999 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\n"[..];
        for strict in [false, true] {
            match xref(test_span(input), strict) {
                Ok((_, re)) => {
                    assert_eq!(
                        re.entries.len(),
                        1,
                        "(strict={strict}) unexpected entries: {:?}",
                        re.entries
                    );
                    assert!(
                        matches!(
                            re.get(1),
                            Some(XrefEntry::Normal {
                                offset: 9,
                                generation: 0
                            })
                        ),
                        "entry for object 1 was displaced (strict={strict}): {:?}",
                        re.get(1)
                    );
                }
                Err(err) => panic!("xref should still parse (strict={strict}): {err:?}"),
            }
        }
    }

    #[test]
    fn startxref_trailing_space_after_keyword() {
        // Some PDF generators emit "startxref \n" with a trailing space.
        let input = b"startxref \n135738\n%%EOF\n";
        match xref_start(test_span(input)) {
            Some(num) => assert_eq!(num, 135738),
            None => panic!("startxref with trailing space should parse"),
        }
    }

    #[test]
    fn content_silently_truncates_corrupted_data() {
        // Corrupted data with unterminated string literal
        let data = b"q 1 0 0 1 10 10 cm (corrupted Q";

        let content = content(data).unwrap();

        // Operations before the corruption returned without an error.
        // Trailing Q was silently dropped.
        assert_eq!(content.operations.len(), 2);
        assert_eq!(content.operations[0].operator, "q");
        assert_eq!(content.operations[1].operator, "cm");
    }

    #[test]
    fn content_strict_rejects_corrupted_data() {
        let data = b"q 1 0 0 1 10 10 cm (corrupted Q";
        assert!(content_strict(data).is_err());
    }

    fn on_big_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deeply_nested_array_is_rejected() {
        on_big_stack(|| {
            let depth = 200_000;
            let mut input = Vec::with_capacity(depth * 2);
            input.extend(std::iter::repeat_n(b'[', depth));
            input.extend(std::iter::repeat_n(b']', depth));
            let result = _direct_object(crate::reader::MAX_NESTING_DEPTH)(input.as_slice());
            assert!(result.is_err());
        });
    }

    #[test]
    fn deeply_nested_dictionary_is_rejected() {
        on_big_stack(|| {
            let depth = 200_000;
            let mut input = Vec::with_capacity(depth * 6);
            for _ in 0..depth {
                input.extend_from_slice(b"<</K ");
            }
            for _ in 0..depth {
                input.extend_from_slice(b">>");
            }
            let result = _direct_object(crate::reader::MAX_NESTING_DEPTH)(input.as_slice());
            assert!(result.is_err());
        });
    }

    #[test]
    fn modestly_nested_array_still_parses() {
        let input = b"[[[[[1]]]]]";
        let obj = _direct_object(crate::reader::MAX_NESTING_DEPTH)(test_span(input));
        assert!(obj.is_ok());
    }

    #[test]
    fn content_treats_nul_and_form_feed_as_white_space() {
        // Some generators (e.g. Polaris Office) pad between operators with NUL bytes.
        let data = b"BT\n/F1 12 Tf\n[(a)] TJ\n\0\0\0\0ET\x0Cq\0Q";
        let content = content_strict(data).unwrap();
        let operators: Vec<_> = content
            .operations
            .iter()
            .map(|op| op.operator.as_ref())
            .collect();
        assert_eq!(operators, ["BT", "Tf", "TJ", "ET", "q", "Q"]);
    }

    #[test]
    fn content_parses_type3_glyph_operators() {
        // Type 3 glyph descriptions start with d0 or d1, whose names end in a digit.
        let data = b"750 0 d0\n0 0 m 10 10 l S\n500 0 0 -10 480 700 d1\n0 0 480 700 re f";
        let content = content_strict(data).unwrap();
        let operators: Vec<_> = content
            .operations
            .iter()
            .map(|op| op.operator.as_ref())
            .collect();
        assert_eq!(operators, ["d0", "m", "l", "S", "d1", "re", "f"]);
        assert_eq!(
            content.operations[0].operands,
            [Object::Integer(750), Object::Integer(0)]
        );
        assert_eq!(content.operations[4].operands.len(), 6);
    }

    #[test]
    fn content_reads_minus_sign_inside_real_as_one_operand() {
        let data = b"q\n0.99999994\n0.00-4142502\n0\n1.0000001\n106.729645\n232.27554\ncm\nQ";
        let content = content_strict(data).unwrap();
        let operators: Vec<_> = content
            .operations
            .iter()
            .map(|op| op.operator.as_ref())
            .collect();
        assert_eq!(operators, ["q", "cm", "Q"]);
        assert_eq!(content.operations[1].operands.len(), 6);
        assert_eq!(content.operations[1].operands[1], Object::Real(0.004142502));
    }
}
