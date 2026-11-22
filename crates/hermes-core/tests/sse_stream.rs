use hermes_core::sse::{SseDecoder, SseError};

#[test]
fn decodes_utf8_split_across_network_chunks_and_multiple_events() {
    let wire = "data: Привет\r\n\r\ndata: [DONE]\n\n".as_bytes();
    let split = wire.iter().position(|byte| *byte == 0xd0).unwrap() + 1;
    let mut decoder = SseDecoder::new();
    assert!(decoder.push(&wire[..split]).unwrap().is_empty());
    assert_eq!(decoder.push(&wire[split..]).unwrap(), ["Привет", "[DONE]"]);
    assert_eq!(decoder.finish().unwrap(), Vec::<String>::new());
}

#[test]
fn joins_data_lines_and_ignores_comments_and_other_fields() {
    let mut decoder = SseDecoder::new();
    let input = b": keepalive\nevent: message\ndata: first\ndata: second\n\n";
    assert_eq!(decoder.push(input).unwrap(), ["first\nsecond"]);
}

#[test]
fn rejects_incomplete_event_at_end_of_stream() {
    let mut decoder = SseDecoder::new();
    assert!(decoder.push(b"data: partial").unwrap().is_empty());
    assert_eq!(decoder.finish(), Err(SseError::IncompleteEvent));
}

#[test]
fn rejects_invalid_utf8_in_complete_event() {
    let mut decoder = SseDecoder::new();
    assert_eq!(decoder.push(b"data: \xff\n\n"), Err(SseError::InvalidUtf8));
}

#[test]
fn handles_crlf_split_across_chunks_and_cr_only_lines() {
    let mut decoder = SseDecoder::new();
    assert!(decoder.push(b"data: one\r").unwrap().is_empty());
    assert_eq!(
        decoder.push(b"\n\r\ndata: two\r\r").unwrap(),
        ["one", "two"]
    );
    assert!(decoder.finish().unwrap().is_empty());
}

#[test]
fn emits_empty_data_event_and_ignores_keepalive() {
    let mut decoder = SseDecoder::new();
    assert_eq!(decoder.push(b": ping\n\ndata:\n\n").unwrap(), [""]);
}

#[test]
fn tracks_the_last_event_id_and_retry_across_events_and_chunks() {
    let mut decoder = SseDecoder::new();
    assert_eq!(decoder.last_event_id(), None);
    // A priming event has an id and empty data; the id counts once the event ends.
    assert!(decoder.push(b"id: r0\nretry: 25\n").unwrap().is_empty());
    assert_eq!(decoder.last_event_id(), None);
    assert_eq!(decoder.push(b"data:\n\n").unwrap(), [""]);
    assert_eq!(decoder.last_event_id(), Some("r0"));
    assert_eq!(decoder.retry_ms(), Some(25));
    // An event without an id keeps the previous one; an id with a NUL or a bad retry is ignored.
    assert_eq!(decoder.push(b"data: a\n\n").unwrap(), ["a"]);
    assert_eq!(decoder.last_event_id(), Some("r0"));
    assert_eq!(
        decoder
            .push(b"id: bad\x00id\nretry: 1x\ndata: b\n\n")
            .unwrap(),
        ["b"]
    );
    assert_eq!(decoder.last_event_id(), Some("r0"));
    assert_eq!(decoder.retry_ms(), Some(25));
    assert_eq!(decoder.push(b"id: r7\r\ndata: c\r\n\r\n").unwrap(), ["c"]);
    assert_eq!(decoder.last_event_id(), Some("r7"));
}
