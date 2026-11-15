use std::convert::{TryFrom, TryInto};
use std::io::{Error, ErrorKind, Result};
use std::mem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    None,
    Sub,
    Up,
    Avg,
    Paeth,
}

impl TryFrom<u8> for FilterType {
    type Error = ();

    fn try_from(n: u8) -> std::result::Result<FilterType, ()> {
        match n {
            0 => Ok(FilterType::None),
            1 => Ok(FilterType::Sub),
            2 => Ok(FilterType::Up),
            3 => Ok(FilterType::Avg),
            4 => Ok(FilterType::Paeth),
            _ => Err(()),
        }
    }
}

fn paeth_predict(left: u8, above: u8, upperleft: u8) -> u8 {
    let expand_left = i16::from(left);
    let expand_above = i16::from(above);
    let expand_upperleft = i16::from(upperleft);

    let initial_estimate = expand_left + expand_above - expand_upperleft;

    let dist_left = (initial_estimate - expand_left).abs();
    let dist_above = (initial_estimate - expand_above).abs();
    let dist_upperleft = (initial_estimate - expand_upperleft).abs();

    if dist_left <= dist_above && dist_left <= dist_upperleft {
        left
    } else if dist_above <= dist_upperleft {
        above
    } else {
        upperleft
    }
}

pub fn decode_row(filter: FilterType, bpp: usize, previous: &[u8], current: &mut [u8]) {
    use self::FilterType::*;
    let len = current.len();
    let bpp = bpp.min(len);

    match filter {
        None => (),
        Sub => {
            for i in bpp..len {
                current[i] = current[i].wrapping_add(current[i - bpp]);
            }
        }
        Up => {
            for i in 0..len {
                current[i] = current[i].wrapping_add(previous[i]);
            }
        }
        Avg => {
            for i in 0..bpp {
                current[i] = current[i].wrapping_add(previous[i] / 2);
            }

            for i in bpp..len {
                current[i] = current[i].wrapping_add(
                    ((i16::from(current[i - bpp]) + i16::from(previous[i])) / 2) as u8,
                );
            }
        }
        Paeth => {
            for i in 0..bpp {
                current[i] = current[i].wrapping_add(paeth_predict(0, previous[i], 0));
            }

            for i in bpp..len {
                current[i] = current[i].wrapping_add(paeth_predict(
                    current[i - bpp],
                    previous[i],
                    previous[i - bpp],
                ));
            }
        }
    }
}

pub fn decode_frame(content: &[u8], bpp: usize, bytes_per_row: usize) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    if content.is_empty() {
        return Ok(decoded);
    }
    // Each row is one filter-type byte plus at most `bytes_per_row` payload
    // bytes.  The allocation is bounded by the input: `/DecodeParms` may
    // request an arbitrarily large row.
    let row_capacity = bytes_per_row.min(content.len() - 1);
    let mut previous = Vec::new();
    previous.try_reserve(row_capacity)?;
    previous.resize(row_capacity, 0_u8);
    let mut current = Vec::new();
    current.try_reserve(row_capacity)?;
    current.resize(row_capacity, 0_u8);
    let mut pos = 0;
    while pos < content.len() {
        let Ok(filter) = content[pos].try_into() else {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("invalid PNG filter type ({})", content[pos]),
            ));
        };
        pos += 1;
        // The final row may be shorter than a full row.  Decode the bytes
        // that are present instead of dropping the row: pdfium's
        // PngPredictor sizes its last output row from the remainder
        // (image_predictors.cpp, PngPredictor), so a cross-reference stream
        // whose last entry is cut short still yields that partial entry.
        let len = (content.len() - pos).min(bytes_per_row);
        current[..len].copy_from_slice(&content[pos..pos + len]);
        pos += len;

        decode_row(filter, bpp, &previous[..len], &mut current[..len]);
        decoded.extend_from_slice(&current[..len]);
        mem::swap(&mut previous, &mut current);
    }
    Ok(decoded)
}

pub fn encode_row(method: FilterType, bpp: usize, previous: &[u8], current: &mut [u8]) {
    use self::FilterType::*;
    let len = current.len();
    let bpp = bpp.min(len);

    match method {
        None => (),
        Sub => {
            for i in (bpp..len).rev() {
                current[i] = current[i].wrapping_sub(current[i - bpp]);
            }
        }
        Up => {
            for i in 0..len {
                current[i] = current[i].wrapping_sub(previous[i]);
            }
        }
        Avg => {
            for i in (bpp..len).rev() {
                current[i] = current[i].wrapping_sub(
                    ((u16::from(current[i - bpp]) + u16::from(previous[i])) / 2) as u8,
                );
            }

            for i in 0..bpp {
                current[i] = current[i].wrapping_sub(previous[i] / 2);
            }
        }
        Paeth => {
            for i in (bpp..len).rev() {
                current[i] = current[i].wrapping_sub(paeth_predict(
                    current[i - bpp],
                    previous[i],
                    previous[i - bpp],
                ));
            }

            for i in 0..bpp {
                current[i] = current[i].wrapping_sub(paeth_predict(0, previous[i], 0));
            }
        }
    }
}
