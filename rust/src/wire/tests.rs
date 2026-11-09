// Ported from internal/wire/wire_test.go.

use super::*;

#[test]
fn round_trip_primitives() {
    let mut w = Writer::new();
    w.u8(0x42);
    w.bool(true);
    w.i8(-5);
    w.u16(0xBEEF);
    w.i16(-1234);
    w.u32(0xDEAD_BEEF);
    w.i32(-99999);
    w.u64(0x1122_3344_5566_7788);
    w.f32(2.71);
    w.vec3(Vec3::new(1.5, -2.5, 3.5));

    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.u8(), 0x42);
    assert!(r.bool());
    assert_eq!(r.i8(), -5);
    assert_eq!(r.u16(), 0xBEEF);
    assert_eq!(r.i16(), -1234);
    assert_eq!(r.u32(), 0xDEAD_BEEF);
    assert_eq!(r.i32(), -99999);
    assert_eq!(r.u64(), 0x1122_3344_5566_7788);
    assert!((r.f32() - 2.71).abs() < 1e-5);
    assert_eq!(r.vec3(), Vec3::new(1.5, -2.5, 3.5));
    assert!(!r.overflowed(), "unexpected overflow");
}

#[test]
fn string_encodings() {
    let mut w = Writer::new();
    w.string8("hello");
    w.string16("world!");
    w.wstring8("Grüße"); // multibyte
    w.wstring16("日本語"); // CJK
    w.cstr("abc", 6);

    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.string8(), "hello");
    assert_eq!(r.string16(), "world!");
    assert_eq!(r.wstring8(), "Grüße");
    assert_eq!(r.wstring16(), "日本語");
    assert_eq!(r.cstr(6), "abc");
}

#[test]
fn utf32_width_counts_characters() {
    // wstring8 must count characters, each 4 bytes on the wire.
    let mut w = Writer::new();
    w.wstring8("日本");
    assert_eq!(w.len(), 1 + 2 * 4);
}

#[test]
fn string8_truncates_bytes() {
    let mut w = Writer::new();
    w.string8(&"x".repeat(300));
    assert_eq!(w.bytes()[0], 255);
    assert_eq!(w.len(), 256);
}

#[test]
fn narrow8_keeps_code_page_bytes() {
    // "Équipe" as the game writes it: one Latin-1 byte, not UTF-8.
    let bytes = [6, 0xC9, b'q', b'u', b'i', b'p', b'e'];
    let mut r = Reader::new(&bytes);
    let team = r.narrow8();
    assert_eq!(team.to_string(), "\u{FFFD}quipe");
    let mut w = Writer::new();
    w.narrow8(&team);
    assert_eq!(w.bytes(), bytes);
}

#[test]
fn invalid_utf32_becomes_replacement() {
    let mut w = Writer::new();
    w.u8(1);
    w.u32(0xD800); // a lone surrogate
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.wstring8(), "\u{FFFD}");
}

#[test]
fn reader_overflow() {
    let mut r = Reader::new(&[0x01]);
    assert_eq!(r.u32(), 0);
    assert!(r.overflowed(), "expected overflow flag");
}

#[test]
fn tcp_framing() {
    let payload = [0x3E, 0x01, 0x02];
    let framed = frame_tcp(&payload);
    assert_eq!(framed.len(), payload.len() + 2);
    assert_eq!(&framed[..2], &[0x03, 0x00], "length prefix");
}

#[test]
fn half_float() {
    let values = [
        0.0f32,
        1.0,
        -1.0,
        0.5,
        3.5,
        100.0,
        -273.15,
        65504.0,
        f32::NAN,
    ];
    let mut w = Writer::new();
    for v in values {
        w.f16(v);
    }
    let mut r = Reader::new(w.bytes());
    for v in values {
        let got = r.f16();
        // Half precision has a short mantissa, so allow a relative tolerance.
        let tolerance = (v.abs() * 0.01).max(0.01);
        assert!(
            (got - v).abs() <= tolerance || (v.is_nan() && got.is_nan()),
            "half round trip {v}: got {got}"
        );
    }
}

#[test]
fn temperature_byte() {
    let mut w = Writer::new();
    for t in [26.9, 0.0, -0.5, -5.0, -40.2] {
        w.temperature(t);
    }
    assert_eq!(w.bytes(), [26, 0, 0, 251, 216]);
}
