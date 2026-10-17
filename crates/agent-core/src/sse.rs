//! Minimal Server-Sent Events decoder for the Messages API stream.

/// One SSE event: the `event:` name and the joined `data:` payload.
#[derive(Debug, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    buf: String,
}

impl SseDecoder {
    /// Feeds raw bytes and returns every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        // Normalize CRLF so the blank-line split works either way.
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n");
        }
        let mut out = Vec::new();
        while let Some(end) = self.buf.find("\n\n") {
            let raw: String = self.buf.drain(..end + 2).collect();
            let mut event = String::new();
            let mut data = Vec::new();
            for line in raw.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            if !event.is_empty() || !data.is_empty() {
                out.push(SseEvent { event, data: data.join("\n") });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"event: ping\ndata: {\"a\"").is_empty());
        let ev = d.push(b":1}\n\nevent: message_stop\ndata: {}\n\n");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], SseEvent { event: "ping".into(), data: "{\"a\":1}".into() });
        assert_eq!(ev[1].event, "message_stop");
    }
}
