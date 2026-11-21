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

    fn consume_line(&mut self, events: &mut Vec<String>) -> Result<(), SseError> {
        let line = std::str::from_utf8(&self.line).map_err(|_| SseError::InvalidUtf8)?;
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            }
        }
        self.line.clear();
        Ok(())
    }
}
