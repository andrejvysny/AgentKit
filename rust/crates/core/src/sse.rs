//! Strict UTF-8 SSE framing with bounded unterminated line and frame state.
use crate::CoreError;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
    pub done: bool,
}
pub const MAX_SSE_BYTES: usize = 1024 * 1024;
#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<String>,
    event: Option<String>,
    frame_bytes: usize,
    first_line: bool,
}
impl SseDecoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, CoreError> {
        let mut frames = vec![];
        for byte in bytes {
            if *byte == b'\n' {
                if let Some(frame) = self.consume_line()? {
                    frames.push(frame);
                }
            } else {
                if self.line.len() >= MAX_SSE_BYTES {
                    return Err(error("no frame boundary"));
                }
                self.line.push(*byte);
            }
        }
        Ok(frames)
    }
    pub fn finish(&mut self) -> Result<Vec<SseFrame>, CoreError> {
        let mut frames = vec![];
        if !self.line.is_empty() {
            if let Some(frame) = self.consume_line()? {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.flush() {
            frames.push(frame);
        }
        Ok(frames)
    }
    fn consume_line(&mut self) -> Result<Option<SseFrame>, CoreError> {
        let bytes = std::mem::take(&mut self.line);
        let raw = std::str::from_utf8(&bytes)
            .map_err(|_| CoreError::new("sse_parse", "sse_parse: invalid UTF-8"))?;
        let mut line = raw.strip_suffix('\r').unwrap_or(raw);
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            return Ok(self.flush());
        }
        self.frame_bytes += line.len();
        if self.frame_bytes > MAX_SSE_BYTES {
            return Err(error("frame exceeds limit"));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).into());
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event = Some(event.trim().into());
        }
        Ok(None)
    }
    fn flush(&mut self) -> Option<SseFrame> {
        self.frame_bytes = 0;
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        let done = data == "[DONE]";
        Some(SseFrame {
            event: if done { None } else { event },
            data: if done { String::new() } else { data },
            done,
        })
    }
}
fn error(message: &str) -> CoreError {
    CoreError::new(
        "sse_parse",
        &format!("sse_parse: {message}; maximum {MAX_SSE_BYTES} bytes"),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_utf8_crlf_multiline_and_done() {
        let input = "event: token\r\ndata: é\r\ndata: x\r\n\r\ndata: [DONE]\n\n";
        let mut parser = SseDecoder::new();
        let mut frames = vec![];
        for byte in input.bytes() {
            frames.extend(parser.push(&[byte]).unwrap());
        }
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("token".into()),
                    data: "é\nx".into(),
                    done: false
                },
                SseFrame {
                    event: None,
                    data: String::new(),
                    done: true
                }
            ]
        );
    }
    #[test]
    fn caps_frame_and_line_but_not_many_complete_frames() {
        let mut parser = SseDecoder::new();
        assert!(parser.push(&vec![b'x'; MAX_SSE_BYTES + 1]).is_err());
        let frames = parser_for(&"data: a\n\n".repeat(200000));
        assert_eq!(frames.len(), 200000);
        let mut parser = SseDecoder::new();
        assert!(parser.push(&[255, b'\n']).is_err());
    }
    fn parser_for(text: &str) -> Vec<SseFrame> {
        SseDecoder::new().push(text.as_bytes()).unwrap()
    }
}
