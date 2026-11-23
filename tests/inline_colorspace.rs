//! Inline-image colour spaces resolved through the resource dictionary.
//!
//! An inline image names a colour space with `/CS`; only the resource dictionary can
//! say how many components a non-device name has, and that count decides where the
//! image data ends.  These tests exercise the resolver API Torcular uses.

use lopdf::content::Content;
use lopdf::{Object, PdfName};
use std::cell::RefCell;

#[test]
fn decoder_resolves_named_colorspace() {
    // `/Cs1` is not a device space, so without the resolver the image cannot be sized.
    let input = b"q BI /W 1 /H 1 /CS /Cs1 /BPC 8 ID \x01\x02\x03\x04 EI Q";
    let resolver = |colorspace: &Object| match colorspace {
        Object::Name(name) if name.as_slice() == b"Cs1" => Some(4),
        _ => None,
    };
    let content = Content::decode_with_inline_colorspaces(input, &resolver).unwrap();
    let ops: Vec<&str> = content.operations.iter().map(|op| op.operator.as_ref()).collect();
    assert_eq!(ops, vec!["q", "BI", "Q"]);
    let image = content.operations[1].operands[0].as_stream().unwrap();
    assert_eq!(image.content, b"\x01\x02\x03\x04");
    assert_eq!(image.dict.get(b"ColorSpace").unwrap().as_name().unwrap(), b"Cs1");
}

#[test]
fn unresolved_colorspace_keeps_legacy_skip() {
    let input = b"q BI /W 1 /H 1 /CS /Cs1 /BPC 8 ID \x01\x02\x03\x04 EI Q";
    // Without a resolver the inline image is skipped, as before.
    let content = Content::decode(input).unwrap();
    let ops: Vec<&str> = content.operations.iter().map(|op| op.operator.as_ref()).collect();
    assert_eq!(ops, vec!["q", "BI", "Q"]);
    assert!(content.operations[1].operands.is_empty());

    // Returning None keeps exactly that behaviour.
    let resolver = |_: &Object| None;
    let content = Content::decode_with_inline_colorspaces(input, &resolver).unwrap();
    assert!(content.operations[1].operands.is_empty());
}

#[test]
fn iterator_resolver_called_once_per_image_in_order() {
    let input = b"BI /W 1 /H 1 /CS /First /BPC 8 ID \x00 EI \
                  BI /W 1 /H 1 /CS /Second /BPC 8 ID \x00\x00\x00 EI";
    let seen = RefCell::new(Vec::new());
    let resolver = |colorspace: &Object| {
        seen.borrow_mut().push(colorspace.clone());
        match colorspace.as_name().ok()? {
            b"First" => Some(1),
            b"Second" => Some(3),
            _ => None,
        }
    };
    let operations = Content::decode_iter_with_inline_colorspaces(input, &resolver)
        .collect::<lopdf::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(operations.len(), 2);
    assert_eq!(
        seen.into_inner(),
        vec![
            Object::Name(PdfName::new(b"First")),
            Object::Name(PdfName::new(b"Second"))
        ]
    );
}

#[test]
fn image_mask_resolver_sees_null() {
    // An image mask has no colour space of its own, so the resolver sees Null.
    let input = b"BI /W 8 /H 1 /IM true /BPC 1 /CS /Ignored ID \xff EI";
    let seen = RefCell::new(None);
    let resolver = |colorspace: &Object| {
        *seen.borrow_mut() = Some(colorspace.clone());
        None
    };
    let content = Content::decode_with_inline_colorspaces(input, &resolver).unwrap();
    assert_eq!(seen.into_inner(), Some(Object::Null));
    let image = content.operations[0].operands[0].as_stream().unwrap();
    assert_eq!(image.content, b"\xff");
}
