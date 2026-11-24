use super::{Object, Stream};
use crate::Result;
pub use crate::parser::{ContentOperations, InlineColorspaceResolver};
use crate::writer::Writer;
use std::borrow::Cow;
use std::io::Write;

#[derive(Debug, Clone)]
pub struct Operation {
    pub operator: Cow<'static, str>,
    pub operands: Vec<Object>,
}

impl Operation {
    pub fn new(operator: &str, operands: Vec<Object>) -> Operation {
        Operation {
            operator: Cow::Owned(operator.to_string()),
            operands,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Content<Operations: AsRef<[Operation]> = Vec<Operation>> {
    pub operations: Operations,
}

impl<Operations: AsRef<[Operation]>> Content<Operations> {
    /// Encode content operations.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let mut first_operation = true;
        for operation in self.operations.as_ref() {
            // Add new line after each operation except the last one.
            if first_operation {
                first_operation = false;
            } else {
                buffer.write_all(b"\n")?;
            }
            // A decoded inline image is a BI operation holding the image as a stream.
            if let ("BI", [Object::Stream(image)]) =
                (operation.operator.as_ref(), operation.operands.as_slice())
            {
                encode_inline_image(&mut buffer, image)?;
                continue;
            }
            for operand in &operation.operands {
                Writer::write_object(&mut buffer, operand)?;
                buffer.write_all(b" ")?;
            }
            buffer.write_all(operation.operator.as_bytes())?;
        }
        Ok(buffer)
    }
}

/// The full name of an inline-image dictionary entry, used to recognize the
/// abbreviated and long spellings of the same entry (ISO 32000-1, Table 93).
fn inline_image_logical_key(key: &[u8]) -> &[u8] {
    match key {
        b"W" => b"Width",
        b"H" => b"Height",
        b"BPC" => b"BitsPerComponent",
        b"CS" => b"ColorSpace",
        b"D" => b"Decode",
        b"DP" => b"DecodeParms",
        b"F" => b"Filter",
        b"IM" => b"ImageMask",
        b"I" => b"Interpolate",
        other => other,
    }
}

/// The abbreviation an inline-image dictionary may use for `key`
/// (ISO 32000-1, Table 93).  Keys without an abbreviation are written as-is.
fn inline_image_key(key: &[u8]) -> &[u8] {
    match key {
        b"Width" => b"W",
        b"Height" => b"H",
        b"BitsPerComponent" => b"BPC",
        b"ColorSpace" => b"CS",
        b"Decode" => b"D",
        b"DecodeParms" => b"DP",
        b"Filter" => b"F",
        b"ImageMask" => b"IM",
        b"Interpolate" => b"I",
        other => other,
    }
}

/// The inline-image spelling of a colour space: the device spaces and
/// /Indexed may be abbreviated (ISO 32000-1, 8.9.7).
fn inline_image_colorspace(colorspace: &Object) -> Object {
    fn abbreviate(name: &[u8]) -> Vec<u8> {
        match name {
            b"DeviceGray" => b"G".to_vec(),
            b"DeviceRGB" => b"RGB".to_vec(),
            b"DeviceCMYK" => b"CMYK".to_vec(),
            b"Indexed" => b"I".to_vec(),
            other => other.to_vec(),
        }
    }
    match colorspace {
        Object::Name(name) => Object::Name(abbreviate(name).into()),
        Object::Array(array) => {
            let mut array = array.clone();
            if let Some(Object::Name(name)) = array.first_mut() {
                *name = abbreviate(name).into();
            }
            Object::Array(array)
        }
        other => other.clone(),
    }
}

/// Write an inline image as `BI`, its dictionary entries, `ID`, its data and `EI`.
fn encode_inline_image(buffer: &mut Vec<u8>, image: &Stream) -> Result<()> {
    buffer.write_all(b"BI")?;
    // A decoded inline image carries both the producer's key and its long-name
    // alias; write each logical entry once, in the order it was read.
    let mut written: Vec<&[u8]> = Vec::new();
    for (key, value) in &image.dict {
        // Readers find the end of the data from the image size, and `Stream::new` adds a
        // Length the source dictionary did not have.
        if key == b"Length" {
            continue;
        }
        let logical = inline_image_logical_key(key);
        if written.contains(&logical) {
            continue;
        }
        written.push(logical);
        buffer.write_all(b" ")?;
        Writer::write_name(buffer, inline_image_key(logical))?;
        buffer.write_all(b" ")?;
        if logical == b"ColorSpace" {
            Writer::write_object(buffer, &inline_image_colorspace(value))?;
        } else {
            Writer::write_object(buffer, value)?;
        }
    }
    buffer.write_all(b" ID ")?;
    buffer.write_all(&image.content)?;
    buffer.write_all(b"\nEI")?;
    Ok(())
}
