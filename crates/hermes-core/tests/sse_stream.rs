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
