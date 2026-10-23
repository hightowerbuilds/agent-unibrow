//! Minimal Server-Sent Events decoder for the Messages API stream.

/// One SSE event: the `event:` name and the joined `data:` payload.
#[derive(Debug, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    // Raw bytes: a chunk can end inside a multi-byte character, so text is
    // only decoded once a whole event (which ends on ASCII "\n\n") is in.
    buf: Vec<u8>,
}

impl SseDecoder {
    /// Feeds raw bytes and returns every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        // Normalize CRLF so the blank-line split works either way. A "\r"
        // at the very end may still get its "\n" in the next chunk.
        if self.buf.contains(&b'\r') {
            let keep_cr = self.buf.last() == Some(&b'\r');
            let end = self.buf.len() - keep_cr as usize;
            let mut out = Vec::with_capacity(self.buf.len());
            let mut i = 0;
            while i < end {
                if self.buf[i] == b'\r' && self.buf.get(i + 1) == Some(&b'\n') {
                    i += 1;
                    continue;
                }
                out.push(self.buf[i]);
                i += 1;
            }
            if keep_cr {
                out.push(b'\r');
            }
            self.buf = out;
        }
        let mut out = Vec::new();
        while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let raw_bytes: Vec<u8> = self.buf.drain(..end + 2).collect();
            let raw = String::from_utf8_lossy(&raw_bytes);
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

    #[test]
    fn multibyte_split_across_chunks() {
        let msg = "event: content_block_delta\ndata: {\"text\":\"caf\u{e9} \u{2014} \u{1f600}\"}\n\n".as_bytes();
        for cut in 1..msg.len() {
            let mut d = SseDecoder::default();
            let mut ev = d.push(&msg[..cut]);
            ev.extend(d.push(&msg[cut..]));
            assert_eq!(ev.len(), 1, "cut at {cut}");
            assert_eq!(ev[0].data, "{\"text\":\"caf\u{e9} \u{2014} \u{1f600}\"}", "cut at {cut}");
        }
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"event: ping\r\ndata: {}\r").is_empty());
        let ev = d.push(b"\n\r\n");
        assert_eq!(ev, vec![SseEvent { event: "ping".into(), data: "{}".into() }]);
    }
}
