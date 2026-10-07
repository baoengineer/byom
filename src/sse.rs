//! Incremental SSE framing. Lines and accumulated events are bounded to 1 MiB.
use anyhow::{Result, bail};

const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    skip_lf: bool,
    started: bool,
    failed: bool,
}

impl SseDecoder {
    /// Accept arbitrary byte fragments, including fragments inside UTF-8 characters.
    /// After a decoding error this decoder must be replaced.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        self.check_state()?;
        let result = self.push_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.complete_line(&mut events)?;
                    self.skip_lf = byte == b'\r';
                }
                _ => {
                    if self.line.len() == MAX_LINE_BYTES {
                        bail!("SSE line exceeds the 1 MiB limit");
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    /// Flush a final unterminated line and any pending data event at EOF.
    /// Repeated calls without new input return no events.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>> {
        self.check_state()?;
        let mut events = Vec::new();
        if !self.line.is_empty()
            && let Err(error) = self.complete_line(&mut events)
        {
            self.failed = true;
            return Err(error);
        }
        self.dispatch(&mut events);
        self.skip_lf = false;
        Ok(events)
    }

    fn check_state(&self) -> Result<()> {
        if self.failed {
            bail!("SSE decoder cannot continue after a decoding error");
        }
        Ok(())
    }

    fn complete_line(&mut self, events: &mut Vec<SseEvent>) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let mut line = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("SSE stream contains invalid UTF-8"))?;
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => {
                if value.len() + self.data.len() > MAX_EVENT_BYTES {
                    bail!("SSE event exceeds the 1 MiB limit");
                }
                self.event = Some(value.to_owned());
            }
            "data" => {
                let separator = usize::from(self.has_data);
                if self.data.len()
                    + separator
                    + value.len()
                    + self.event.as_ref().map_or(0, String::len)
                    > MAX_EVENT_BYTES
                {
                    bail!("SSE event exceeds the 1 MiB limit");
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = self.event.take().filter(|name| !name.is_empty());
        let data = std::mem::take(&mut self.data);
        if self.has_data {
            events.push(SseEvent { event, data });
        }
        self.has_data = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_utf8_crlf_and_multiple_events() {
        let input = "\u{feff}: comment\r\nevent: delta\r\ndata: hé🙂\r\ndata: second\r\n\r\ndata: [DONE]\n\n";
        for chunk_size in 1..=input.len() {
            let mut decoder = SseDecoder::default();
            let mut events = Vec::new();
            for chunk in input.as_bytes().chunks(chunk_size) {
                events.extend(decoder.push(chunk).unwrap());
            }
            events.extend(decoder.finish().unwrap());
            assert_eq!(
                events,
                vec![
                    SseEvent {
                        event: Some("delta".into()),
                        data: "hé🙂\nsecond".into()
                    },
                    SseEvent {
                        event: None,
                        data: "[DONE]".into()
                    },
                ]
            );
        }
    }

    #[test]
    fn fields_comments_empty_data_and_bare_cr() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b"event: ignored\r: keepalive\r\revent: old\nevent:\ndata\ndata:  spaced\nid: 1\nretry: 100\nunknown: x\n\n").unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "\n spaced".into()
            }]
        );
    }

    #[test]
    fn finish_flushes_and_is_idempotent() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"event: delta\ndata: one\ndata: two")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            decoder.finish().unwrap(),
            vec![SseEvent {
                event: Some("delta".into()),
                data: "one\ntwo".into()
            }]
        );
        assert!(decoder.finish().unwrap().is_empty());
        assert!(decoder.push(b": comment").unwrap().is_empty());
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn invalid_and_incomplete_utf8_fail_without_leaking_input() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: \xff\n").is_err());
        assert!(decoder.push(b"data: ok\n\n").is_err());
        assert!(decoder.finish().is_err());
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: \xe2").unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn bounds_lines_and_multiline_events() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(&vec![b'x'; MAX_LINE_BYTES])
                .unwrap()
                .is_empty()
        );
        assert!(decoder.push(b"x").is_err());

        let mut decoder = SseDecoder::default();
        let line = format!("data: {}\n", "x".repeat(MAX_EVENT_BYTES / 2));
        decoder.push(line.as_bytes()).unwrap();
        assert!(decoder.push(line.as_bytes()).is_err());
    }
}
