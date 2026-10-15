//! String encodings are part of Photoshop's text-engine data, including opaque byte strings.

use photocraft_text::engine_data::{self as ed, Value};

fn literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![b'('];
    for &byte in bytes {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b')');
    out
}

fn has_bytes(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|part| part == needle)
}

fn source_with_payload(payload: &[u8]) -> Vec<u8> {
    let mut source = b"<< /Opaque ".to_vec();
    source.extend(literal(payload));
    source.extend_from_slice(b" /Count 1 >>");
    source
}

fn assert_payload_survives_edit(payload: &[u8]) {
    let mut tree = ed::parse(&source_with_payload(payload)).unwrap();
    assert_eq!(tree.get("Opaque").and_then(Value::as_raw_bytes), Some(payload));
    tree.set("Count", Value::Int(2));
    let written = ed::write(&tree);
    assert!(has_bytes(&written, &literal(payload)), "an unchanged byte string must retain its bytes and encoding");
    assert_eq!(ed::parse(&written).unwrap(), tree);
    assert_eq!(ed::write(&ed::parse(&written).unwrap()), written);
}

#[test]
fn ascii_bitset_keeps_its_byte_encoding_when_another_field_changes() {
    assert_payload_survives_edit(b"000000000000000011111111111111110101010101010101");
}

#[test]
fn binary_bytes_and_escaped_delimiters_survive_an_edit() {
    let payload: Vec<u8> = (0..=u8::MAX).collect();
    assert_payload_survives_edit(&payload);
}

#[test]
fn an_empty_byte_string_does_not_become_an_empty_unicode_string() {
    assert_payload_survives_edit(b"");
}

#[test]
fn eight_bit_text_access_retains_its_existing_latin1_meaning() {
    let tree = ed::parse(&source_with_payload(&[b'A', 0x80, 0xff])).unwrap();
    assert_eq!(tree.get("Opaque").and_then(Value::as_str), Some("A\u{80}\u{ff}"));
}

#[test]
fn malformed_utf16_is_retained_instead_of_replaced_or_truncated() {
    for payload in [&[0xfe, 0xff, 0x00, b'A', 0xff][..], &[0xfe, 0xff, 0xd8, 0x00][..], &[0xfe, 0xff, 0xdc, 0x00][..]] {
        assert_payload_survives_edit(payload);
    }
}

#[test]
fn unicode_text_stays_utf16_and_keeps_non_bmp_and_escaped_characters() {
    let text = "\u{d55c}\u{ae00} \u{1f680} (label) \\ \r";
    let value = Value::Dict(vec![("Text".into(), Value::String(text.into()))]);
    let encoded = ed::write(&value);
    let mut payload = vec![0xfe, 0xff];
    for unit in text.encode_utf16() {
        payload.extend_from_slice(&unit.to_be_bytes());
    }
    assert!(has_bytes(&encoded, &literal(&payload)));
    assert_eq!(ed::parse(&encoded).unwrap(), value);
    assert_eq!(ed::parse(&encoded).unwrap().get("Text").and_then(Value::as_raw_bytes), None);
}

#[test]
fn bare_document_text_data_preserves_opaque_strings_during_serialization() {
    let payload = b"0011110000\\()\x80\xff";
    let mut tree = ed::parse(&source_with_payload(payload)).unwrap();
    tree.set("Count", Value::Int(3));
    tree.set("Text", Value::String("current\r".into()));
    let Value::Dict(items) = tree else { panic!("expected dictionary") };
    let encoded = ed::write_bare(&items);
    assert!(has_bytes(&encoded, &literal(payload)));
    let mut wrapped = b"<<".to_vec();
    wrapped.extend_from_slice(&encoded);
    wrapped.extend_from_slice(b">>");
    let back = ed::parse(&wrapped).unwrap();
    assert_eq!(back.get("Text").and_then(Value::as_str), Some("current\r"));
    assert_eq!(back, Value::Dict(items));
}
