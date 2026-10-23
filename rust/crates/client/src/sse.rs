use crate::ClientError;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SseFrame {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: Option<String>,
    pub retry: Option<u64>,
}

/// Incremental byte parser. CRLF remains one terminator across chunk boundaries.
#[derive(Debug)]
pub struct SseDecoder {
    limit: usize,
    line: Vec<u8>,
    frame: SseFrame,
    data: Vec<String>,
    bytes: usize,
    skip_lf: bool,
    saw_field: bool,
    first_line: bool,
}

impl SseDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            limit: max_frame_bytes,
            line: Vec::new(),
            frame: SseFrame::default(),
            data: Vec::new(),
            bytes: 0,
            skip_lf: false,
            saw_field: false,
            first_line: true,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ClientError> {
        let mut ready = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.bytes += 1;
            if self.bytes > self.limit {
                return Err(protocol("SSE frame exceeds max_frame_bytes"));
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                if let Some(frame) = self.line()? {
                    ready.push(frame);
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(ready)
    }

    pub fn has_partial_frame(&self) -> bool {
        !self.line.is_empty() || self.saw_field || !self.data.is_empty()
    }

    /// Partial frames are intentionally discarded; only blank lines commit frames.
    pub fn finish(&mut self) {
        self.line.clear();
        self.frame = SseFrame::default();
        self.data.clear();
    }

    fn line(&mut self) -> Result<Option<SseFrame>, ClientError> {
        let bytes = std::mem::take(&mut self.line);
        let line =
            std::str::from_utf8(&bytes).map_err(|_| protocol("SSE contains invalid UTF-8"))?;
        let line = if self.first_line {
            self.first_line = false;
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.is_empty() {
            let mut frame = std::mem::take(&mut self.frame);
            if !self.data.is_empty() {
                frame.data = Some(std::mem::take(&mut self.data).join("\n"));
            }
            self.bytes = 0;
            return Ok(std::mem::take(&mut self.saw_field).then_some(frame));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        self.saw_field = true;
        match field {
            "id" if !value.contains('\0') => self.frame.id = Some(value.into()),
            "event" => self.frame.event = Some(value.into()),
            "data" => self.data.push(value.into()),
            "retry" if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) => {
                self.frame.retry = value.parse().ok()
            }
            _ => {}
        }
        Ok(None)
    }
}

pub(crate) fn protocol(message: &str) -> ClientError {
    ClientError::Protocol(message.into())
}
