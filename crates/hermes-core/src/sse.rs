//! Incremental decoding of the data fields in a server-sent event stream.

#[derive(Debug, PartialEq, Eq)]
pub enum SseError {
    InvalidUtf8,
    IncompleteEvent,
}

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
    skip_lf: bool,
    /// The `id` field of the event being read; it takes effect when the event ends.
    pending_id: Option<String>,
    pending_retry: Option<u64>,
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.consume_line(&mut events)?;
                    self.skip_lf = true;
                }
                b'\n' => self.consume_line(&mut events)?,
                _ => self.line.push(byte),
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, SseError> {
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err(SseError::IncompleteEvent);
        }
        Ok(Vec::new())
    }

    /// The last non-empty `id` of an event that has ended, even one without data (a priming
    /// event). A client that loses the stream resumes after it with `Last-Event-ID`.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// The last `retry` field: how long a client should wait before it reconnects.
    pub fn retry_ms(&self) -> Option<u64> {
        self.retry_ms
    }

    fn consume_line(&mut self, events: &mut Vec<String>) -> Result<(), SseError> {
        let line = std::str::from_utf8(&self.line).map_err(|_| SseError::InvalidUtf8)?;
        if line.is_empty() {
            if let Some(id) = self.pending_id.take().filter(|id| !id.is_empty()) {
                self.last_event_id = Some(id);
            }
            if let Some(retry) = self.pending_retry.take() {
                self.retry_ms = Some(retry);
            }
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => self.data.push(value.to_owned()),
                "id" if !value.contains('\0') => self.pending_id = Some(value.to_owned()),
                "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                    self.pending_retry = value.parse().ok();
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(())
    }
}
