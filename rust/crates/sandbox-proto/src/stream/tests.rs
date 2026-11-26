use super::*;

fn receive(id: u64, expected: u64, frames: Vec<DataFrame>) -> Result<Vec<u8>, StreamError> {
    let mut receiver = DataReceiver::new(id, expected);
    for frame in frames {
        if let Some(bytes) = receiver.accept(frame)? {
            return Ok(bytes);
        }
    }
    panic!("stream had no terminator");
}

#[test]
fn split_streams_reassemble() {
    for len in [0, 1, DATA_MAX_BYTES, DATA_MAX_BYTES + 1, 2 * DATA_MAX_BYTES] {
        let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let frames: Vec<DataFrame> = data_frames(9, &bytes).collect();
        assert_eq!(frames.len(), len.div_ceil(DATA_MAX_BYTES) + 1, "len {len}");
        assert!(frames.last().unwrap().bytes.is_empty());
        assert!(
            frames
                .iter()
                .all(|f| f.id == 9 && f.bytes.len() <= DATA_MAX_BYTES)
        );
        assert_eq!(receive(9, len as u64, frames).unwrap(), bytes);
    }
}

fn frame(id: u64, seq: u64, bytes: &[u8]) -> DataFrame {
    DataFrame {
        id,
        seq,
        bytes: bytes.to_vec(),
    }
}

#[test]
fn refuses_another_request_or_sequence() {
    assert_eq!(
        receive(1, 1, vec![frame(2, 0, b"a")]),
        Err(StreamError::WrongRequest {
            expected: 1,
            got: 2
        })
    );
    assert_eq!(
        receive(1, 2, vec![frame(1, 0, b"a"), frame(1, 2, b"b")]),
        Err(StreamError::OutOfOrder {
            expected: 1,
            got: 2
        })
    );
}

#[test]
fn refuses_streams_of_another_length() {
    assert_eq!(
        receive(1, 1, vec![frame(1, 0, b"ab")]),
        Err(StreamError::TooLong { expected: 1 })
    );
    assert_eq!(
        receive(1, 3, vec![frame(1, 0, b"ab"), frame(1, 1, b"")]),
        Err(StreamError::TooShort {
            expected: 3,
            got: 2
        })
    );
}

#[test]
fn refuses_frames_after_the_end_or_an_error() {
    let mut receiver = DataReceiver::new(1, 0);
    assert_eq!(receiver.accept(frame(1, 0, b"")), Ok(Some(Vec::new())));
    assert_eq!(
        receiver.accept(frame(1, 1, b"")),
        Err(StreamError::Finished)
    );
    let mut receiver = DataReceiver::new(1, 0);
    assert!(receiver.accept(frame(1, 0, b"x")).is_err());
    assert_eq!(
        receiver.accept(frame(1, 0, b"")),
        Err(StreamError::Finished)
    );
}
