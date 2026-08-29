use open_search_core::SearchCapability;
use serde_json::Value;
use thiserror::Error;

const MAX_FRAME_SIZE: usize = 256 * 1024;
const MAX_STREAM_SIZE: usize = 8 * 1024 * 1024;

pub(crate) struct GrokSseCollector {
    buffer: Vec<u8>,
    received: usize,
    results: Vec<Value>,
    response_completed: bool,
    capability: SearchCapability,
    saw_x_search_call: bool,
}

pub(crate) struct GrokSseOutput {
    pub(crate) results: Vec<Value>,
}

impl GrokSseCollector {
    pub(crate) fn new(capability: SearchCapability) -> Self {
        Self {
            buffer: Vec::new(),
            received: 0,
            results: Vec::new(),
            response_completed: false,
            capability,
            saw_x_search_call: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), GrokSseError> {
        self.received = self
            .received
            .checked_add(chunk.len())
            .ok_or(GrokSseError::StreamTooLarge)?;
        if self.received > MAX_STREAM_SIZE {
            return Err(GrokSseError::StreamTooLarge);
        }
        self.buffer.extend_from_slice(chunk);

        while let Some(end) = find_frame_end(&self.buffer) {
            let frame = self.buffer.drain(..end).collect::<Vec<_>>();
            self.handle_frame(&frame)?;
        }
        if self.buffer.len() > MAX_FRAME_SIZE {
            return Err(GrokSseError::FrameTooLarge);
        }
        Ok(())
    }

    pub(crate) fn take_output(&mut self) -> Option<GrokSseOutput> {
        let ready = match self.capability {
            SearchCapability::Web => !self.results.is_empty(),
            SearchCapability::X => {
                self.response_completed && self.saw_x_search_call && !self.results.is_empty()
            }
        };
        ready.then(|| GrokSseOutput {
            results: std::mem::take(&mut self.results),
        })
    }

    pub(crate) fn finish(mut self) -> Result<GrokSseOutput, GrokSseError> {
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            self.handle_frame(&frame)?;
        }
        if let Some(output) = self.take_output() {
            return Ok(output);
        }
        if self.response_completed {
            Err(GrokSseError::MissingToolResult)
        } else {
            Err(GrokSseError::Incomplete)
        }
    }

    fn handle_frame(&mut self, frame: &[u8]) -> Result<(), GrokSseError> {
        if frame.len() > MAX_FRAME_SIZE {
            return Err(GrokSseError::FrameTooLarge);
        }
        let Some(data) = data_payload(frame) else {
            return Ok(());
        };
        if data == b"[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_slice(&data).map_err(|_| GrokSseError::InvalidJson)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    self.handle_output_item(item);
                }
            }
            Some("response.completed") => {
                if let Some(output) = event
                    .get("response")
                    .and_then(|response| response.get("output"))
                    .and_then(Value::as_array)
                {
                    for item in output {
                        self.handle_output_item(item);
                    }
                }
                self.response_completed = true;
            }
            Some("response.error")
            | Some("response.failed")
            | Some("response.incomplete")
            | Some("error") => {
                return Err(GrokSseError::Upstream(event_message(&event)));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_output_item(&mut self, item: &Value) {
        match self.capability {
            SearchCapability::Web if is_web_search_result(item) => self.push_result(item),
            SearchCapability::X if is_x_search_call(item) => self.saw_x_search_call = true,
            SearchCapability::X if is_completed_assistant_message(item) => self.push_result(item),
            SearchCapability::X | SearchCapability::Web => {}
        }
    }

    fn push_result(&mut self, item: &Value) {
        let id = item.get("id").and_then(Value::as_str);
        if self.results.iter().any(|existing| {
            (id.is_some() && existing.get("id").and_then(Value::as_str) == id) || existing == item
        }) {
            return;
        }
        self.results.push(item.clone());
    }
}

fn is_web_search_result(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("web_search_call")
}

fn is_x_search_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("custom_tool_call")
}

fn is_completed_assistant_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("assistant")
        && item.get("status").and_then(Value::as_str) == Some("completed")
}

fn event_message(event: &Value) -> String {
    event
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| event.get("error")?.get("message")?.as_str())
        .unwrap_or("Grok returned a streaming error")
        .to_owned()
}

fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    let short = buffer
        .windows(2)
        .position(|window| matches!(window, b"\n\n" | b"\r\r"))
        .map(|index| index + 2);
    match (crlf, short) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    for line in frame.split(|byte| matches!(byte, b'\r' | b'\n')) {
        let data = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"));
        if let Some(data) = data {
            if !payload.is_empty() {
                payload.push(b'\n');
            }
            payload.extend_from_slice(data);
        }
    }
    (!payload.is_empty()).then_some(payload)
}

#[derive(Debug, Error)]
pub(crate) enum GrokSseError {
    #[error("Grok SSE frame exceeded the size limit")]
    FrameTooLarge,
    #[error("Grok SSE stream exceeded the size limit")]
    StreamTooLarge,
    #[error("Grok SSE event contained invalid JSON")]
    InvalidJson,
    #[error("Grok SSE stream ended before a usable search result was returned")]
    Incomplete,
    #[error("Grok response completed without a usable search result")]
    MissingToolResult,
    #[error("{0}")]
    Upstream(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_x_message_and_response_completion() {
        let mut collector = GrokSseCollector::new(SearchCapability::X);
        collector
            .push(b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"id\":\"x-1\",\"name\":\"x_semantic_search\",\"input\":\"{\\\"query\\\":\\\"rust\\\"}\",\"status\":\"completed\"}}\n\n")
            .expect("tool event");
        assert!(collector.take_output().is_none());

        collector
            .push(br#"data: {"type":"response.output_item.done","item":{"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"result","annotations":[{"type":"url_citation","url":"https://x.com/i/status/1"}]}]}}

"#)
            .expect("message event");
        assert!(collector.take_output().is_none());

        collector
            .push(
                br#"data: {"type":"response.completed","response":{"output":[]}}

"#,
            )
            .expect("completed event");

        let output = collector.take_output().expect("completed X result");
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0]["type"], "message");
        assert_eq!(output.results[0]["content"][0]["text"], "result");
        assert_eq!(
            output.results[0]["content"][0]["annotations"][0]["url"],
            "https://x.com/i/status/1"
        );
    }

    #[test]
    fn recovers_x_result_from_completed_response_output() {
        let mut collector = GrokSseCollector::new(SearchCapability::X);
        collector
            .push(br#"data: {"type":"response.completed","response":{"output":[{"type":"custom_tool_call","id":"x-1","name":"x_keyword_search","input":"{\"query\":\"rust\"}","status":"completed"},{"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"result","annotations":[]}] }]}}

"#)
            .expect("completed response");

        let output = collector.finish().expect("completed X result");
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0]["type"], "message");
    }

    #[test]
    fn returns_web_search_tool_result_unchanged() {
        let mut collector = GrokSseCollector::new(SearchCapability::Web);
        collector
            .push(br#"data: {"type":"response.output_item.done","item":{"type":"web_search_call","id":"web-1","status":"completed","action":{"type":"search","query":"rust","sources":[{"url":"https://www.rust-lang.org"}]}}}

"#)
            .expect("tool event");

        let output = collector.finish().expect("hosted tool result");
        assert_eq!(
            output.results[0],
            serde_json::json!({
                "type": "web_search_call",
                "id": "web-1",
                "status": "completed",
                "action": {
                    "type": "search",
                    "query": "rust",
                    "sources": [{"url": "https://www.rust-lang.org"}]
                }
            })
        );
    }

    #[test]
    fn rejects_completed_response_without_a_search_tool_result() {
        let mut collector = GrokSseCollector::new(SearchCapability::Web);
        collector
            .push(
                br#"data: {"type":"response.completed","response":{"output":[]}}

"#,
            )
            .expect("completed event");
        assert!(matches!(
            collector.finish(),
            Err(GrokSseError::MissingToolResult)
        ));
    }

    #[test]
    fn rejects_x_message_without_an_x_search_call() {
        let mut collector = GrokSseCollector::new(SearchCapability::X);
        collector
            .push(br#"data: {"type":"response.completed","response":{"output":[{"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"result","annotations":[]}]}]}}

"#)
            .expect("completed response");

        assert!(matches!(
            collector.finish(),
            Err(GrokSseError::MissingToolResult)
        ));
    }

    #[test]
    fn x_requires_response_completed_not_only_done_sentinel() {
        let mut collector = GrokSseCollector::new(SearchCapability::X);
        collector
            .push(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"custom_tool_call\",\"id\":\"x-1\",\"name\":\"x_keyword_search\",\"input\":\"{}\",\"status\":\"completed\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"message-1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[]}}\n\ndata: [DONE]\n\n")
            .expect("stream events");

        assert!(collector.take_output().is_none());
        assert!(matches!(collector.finish(), Err(GrokSseError::Incomplete)));
    }
}
