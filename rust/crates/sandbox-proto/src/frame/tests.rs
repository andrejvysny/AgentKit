use std::io::Cursor;

use super::*;

fn header(len: u32, kind: u8) -> Vec<u8> {
    let mut out = len.to_be_bytes().to_vec();
    out.push(kind);
    out
}

fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>, FrameError> {
    let mut decoder = FrameDecoder::new();
    decoder.push(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    Ok(frames)
}

fn decode_err(bytes: &[u8]) -> FrameError {
    let decoded = decode_all(bytes).expect_err("decoder accepted bad input");
    let read = read_frame(&mut Cursor::new(bytes)).expect_err("read_frame accepted bad input");
    assert_eq!(decoded.to_string(), read.to_string());
    decoded
}

fn sample_frames() -> Vec<Frame> {
    vec![
        Frame::Control(r#"{"id":1,"body":{"type":"usage"}}"#.into()),
        Frame::Data(DataFrame {
            id: 7,
            seq: 0,
            bytes: b"hello".to_vec(),
        }),
        Frame::Data(DataFrame {
            id: u64::MAX,
            seq: u64::MAX,
            bytes: Vec::new(),
        }),
        Frame::Control("[]".into()),
    ]
}

#[test]
fn control_frame_layout_is_length_kind_json() {
    let bytes = encode_control(&serde_json::json!({"id": 3})).unwrap();
    assert_eq!(&bytes[..5], &[0, 0, 0, 9, KIND_CONTROL]);
    assert_eq!(&bytes[5..], br#"{"id":3}"#);
}

#[test]
fn data_frame_layout_is_length_kind_id_seq_bytes() {
    let bytes = encode_data(1, 2, b"ab").unwrap();
    let mut want = header(19, KIND_DATA);
    want.extend_from_slice(&1u64.to_be_bytes());
    want.extend_from_slice(&2u64.to_be_bytes());
    want.extend_from_slice(b"ab");
    assert_eq!(bytes, want);
}

#[test]
fn frames_round_trip_through_the_decoder_and_read_frame() {
    let frames = sample_frames();
    let mut wire = Vec::new();
    for frame in &frames {
        write_frame(&mut wire, frame).unwrap();
    }
    assert_eq!(decode_all(&wire).unwrap(), frames);
    let mut cursor = Cursor::new(&wire);
    let mut read = Vec::new();
    while let Some(frame) = read_frame(&mut cursor).unwrap() {
        read.push(frame);
    }
    assert_eq!(read, frames);
}

#[test]
fn decoder_fed_byte_by_byte_yields_each_frame_once() {
    let frames = sample_frames();
    let wire: Vec<u8> = frames.iter().flat_map(|f| f.encode().unwrap()).collect();
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    for byte in &wire {
        decoder.push(std::slice::from_ref(byte));
        while let Some(frame) = decoder.next_frame().unwrap() {
            out.push(frame);
        }
    }
    assert_eq!(out, frames);
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn many_tiny_frames_in_one_push_decode_in_linear_time() {
    // Large enough that dropping each frame's bytes from the front of the
    // buffer (quadratic) takes many seconds; linear decoding takes
    // milliseconds.
    const COUNT: usize = 200_000;
    let frame = encode_data(9, 0, &[]).unwrap();
    let mut wire = frame.repeat(COUNT);
    let next = encode_data(9, 1, b"tail").unwrap();
    let (head, tail) = next.split_at(7);
    wire.extend_from_slice(head);
    let started = std::time::Instant::now();
    let mut decoder = FrameDecoder::new();
    decoder.push(&wire);
    let mut count = 0;
    while let Some(frame) = decoder.next_frame().unwrap() {
        assert!(matches!(
            frame,
            Frame::Data(DataFrame { id: 9, seq: 0, .. })
        ));
        count += 1;
    }
    assert_eq!(count, COUNT);
    assert_eq!(decoder.buffered(), head.len(), "the partial frame stays");
    decoder.push(tail);
    let last = decoder.next_frame().unwrap().expect("completed frame");
    assert_eq!(last.encode().unwrap(), next);
    assert_eq!(decoder.next_frame().unwrap(), None);
    assert_eq!(decoder.buffered(), 0);
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "{COUNT} frames took {elapsed:?}"
    );
}

#[test]
fn maxima_are_inclusive() {
    let json = format!("\"{}\"", "a".repeat(CONTROL_MAX_BYTES - 2));
    let control = Frame::Control(json);
    assert_eq!(decode_all(&control.encode().unwrap()).unwrap(), [control]);
    let data = Frame::Data(DataFrame {
        id: 1,
        seq: 0,
        bytes: vec![7; DATA_MAX_BYTES],
    });
    assert_eq!(decode_all(&data.encode().unwrap()).unwrap(), [data]);
}

#[test]
fn encoders_refuse_oversized_payloads() {
    let json = "a".repeat(CONTROL_MAX_BYTES + 1);
    assert!(matches!(
        encode_control(&json),
        Err(FrameError::TooLarge { max, .. }) if max == CONTROL_MAX_LEN
    ));
    assert!(matches!(
        encode_data(1, 0, &vec![0; DATA_MAX_BYTES + 1]),
        Err(FrameError::TooLarge { max, .. }) if max == DATA_MAX_LEN
    ));
}

#[test]
fn rejects_zero_length() {
    assert!(matches!(decode_err(&[0, 0, 0, 0]), FrameError::EmptyFrame));
}

#[test]
fn rejects_unknown_kinds() {
    for kind in [0u8, 3, 255] {
        let mut bytes = header(2, kind);
        bytes.push(0);
        assert!(matches!(decode_err(&bytes), FrameError::UnknownKind(k) if k == kind));
    }
}

#[test]
fn rejects_oversized_lengths_from_the_header_alone() {
    let control = (CONTROL_MAX_LEN + 1) as u32;
    let err = decode_err(&header(control, KIND_CONTROL));
    assert!(matches!(err, FrameError::TooLarge { len, max }
        if len == u64::from(control) && max == CONTROL_MAX_LEN));
    let data = (DATA_MAX_LEN + 1) as u32;
    assert!(matches!(
        decode_err(&header(data, KIND_DATA)),
        FrameError::TooLarge { .. }
    ));
    // No body follows: the decoder refuses on the prefix before buffering.
    assert!(matches!(
        decode_err(&u32::MAX.to_be_bytes()),
        FrameError::TooLarge { .. }
    ));
}

#[test]
fn rejects_data_frames_shorter_than_their_header() {
    for len in [1u32, 2, 16] {
        let mut bytes = header(len, KIND_DATA);
        bytes.resize(4 + len as usize, 0);
        assert!(
            matches!(decode_err(&bytes), FrameError::DataTooShort { len: l } if l == u64::from(len))
        );
    }
}

#[test]
fn rejects_invalid_utf8_and_invalid_json() {
    let mut bytes = header(3, KIND_CONTROL);
    bytes.extend_from_slice(&[0xff, 0xfe]);
    assert!(matches!(decode_err(&bytes), FrameError::InvalidUtf8));
    for text in ["{", "nope", "{} {}", ""] {
        let mut bytes = header(1 + text.len() as u32, KIND_CONTROL);
        bytes.extend_from_slice(text.as_bytes());
        assert!(
            matches!(decode_err(&bytes), FrameError::InvalidJson(_)),
            "{text:?}"
        );
    }
}

#[test]
fn decoder_stays_poisoned_after_an_error() {
    let mut decoder = FrameDecoder::new();
    decoder.push(&[0, 0, 0, 0]);
    assert!(decoder.next_frame().is_err());
    decoder.push(&Frame::Control("1".into()).encode().unwrap());
    assert!(matches!(decoder.next_frame(), Err(FrameError::Poisoned)));
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn read_frame_tells_clean_end_from_truncation() {
    assert!(
        read_frame(&mut Cursor::new(&[] as &[u8]))
            .unwrap()
            .is_none()
    );
    let wire = encode_data(1, 0, b"abc").unwrap();
    for cut in 1..wire.len() {
        let err = read_frame(&mut Cursor::new(&wire[..cut])).unwrap_err();
        assert!(matches!(err, FrameError::Truncated), "cut at {cut}: {err}");
    }
}

#[test]
fn parse_control_separates_shape_errors() {
    let value: Vec<u32> = parse_control("[1,2]").unwrap();
    assert_eq!(value, [1, 2]);
    assert!(matches!(
        parse_control::<Vec<u32>>("{}"),
        Err(FrameError::InvalidMessage(_))
    ));
}

/// Tiny xorshift64 so the fuzz loops need no extra dependency.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Feed `bytes` in random chunks to the decoder and to `read_frame`; both
/// must answer without panicking.
fn feed(rng: &mut XorShift, bytes: &[u8]) {
    let mut decoder = FrameDecoder::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(1 + rng.below(rest.len()));
        decoder.push(chunk);
        rest = tail;
        while let Ok(Some(_)) = decoder.next_frame() {}
    }
    let mut cursor = Cursor::new(bytes);
    while let Ok(Some(_)) = read_frame(&mut cursor) {}
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..2_000 {
        let len = rng.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        feed(&mut rng, &bytes);
    }
}

#[test]
fn mutated_and_truncated_valid_frames_never_panic() {
    let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
    let wire: Vec<u8> = sample_frames()
        .iter()
        .flat_map(|f| f.encode().unwrap())
        .collect();
    for _ in 0..5_000 {
        let mut bytes = wire.clone();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.next() as u8;
        }
        bytes.truncate(1 + rng.below(bytes.len()));
        feed(&mut rng, &bytes);
    }
}
