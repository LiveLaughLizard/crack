//! A small server-sent events reader over a reqwest byte stream.

use futures::StreamExt;

pub struct SseEvent {
    #[allow(dead_code)]
    pub event: Option<String>,
    pub data: String,
}

pub struct SseReader {
    stream: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    buffer: Vec<u8>,
}

impl SseReader {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            stream: response.bytes_stream().boxed(),
            buffer: Vec::new(),
        }
    }

    /// The next event, or `None` at the end of the stream.
    pub async fn next(&mut self) -> anyhow::Result<Option<SseEvent>> {
        loop {
            if let Some(event) = self.take_event() {
                return Ok(Some(event));
            }
            match self.stream.next().await {
                Some(chunk) => self.buffer.extend_from_slice(&chunk?),
                None => {
                    // A final event without the trailing blank line.
                    if self.buffer.iter().any(|b| !b.is_ascii_whitespace()) {
                        self.buffer.extend_from_slice(b"\n\n");
                        return Ok(self.take_event());
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        loop {
            let (end, separator) = find_separator(&self.buffer)?;
            let raw: Vec<u8> = self.buffer.drain(..end + separator).collect();
            let text = String::from_utf8_lossy(&raw[..end]);
            let mut event = None;
            let mut data = Vec::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                } else if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                }
            }
            if !data.is_empty() || event.is_some() {
                return Some(SseEvent {
                    event,
                    data: data.join("\n"),
                });
            }
        }
    }
}

fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}
