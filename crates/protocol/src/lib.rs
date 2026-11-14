//! Wire protocol between Testudo adapters (JS reporters/preloads, future CDP and
//! native-runner bridges) and the Testudo core.
//!
//! Framing, all integers big-endian:
//!
//! ```text
//! message := u32 len | u8 kind | body[len - 1]
//! kind 1  := Event   body = UTF-8 JSON `Event`
//! kind 2  := Frame   body = u32 header_len | JSON `FrameHeader` | JPEG bytes
//! ```
//!
//! JSON uses camelCase field names and a `type` tag. The JS side lives in
//! `adapters/js/transport.mjs` and must stay in sync with this file.

use bytes::{Buf, BufMut, BytesMut};
pub use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub const KIND_EVENT: u8 = 1;
pub const KIND_FRAME: u8 = 2;

/// Upper bound on a single message, guards against garbage length prefixes.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// Env vars the core sets on spawned runner processes.
pub mod env {
    pub const SOCKET: &str = "TESTUDO_SOCKET";
    pub const RUN_ID: &str = "TESTUDO_RUN_ID";
    pub const PROJECT_ROOT: &str = "TESTUDO_PROJECT_ROOT";
    /// Recorded frames fit within this many pixels on each side.
    pub const FRAME_MAX: &str = "TESTUDO_FRAME_MAX";
    /// At most this many recorded frames per second.
    pub const FRAME_RATE: &str = "TESTUDO_FRAME_RATE";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    /// Runner reporter in the runner's main process: test lifecycle.
    Reporter,
    /// Node preload that instruments browser pages: frames, network, console.
    Preload,
    /// Direct CDP attachment from the core (future).
    Cdp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestStatus {
    Passed,
    Failed,
    TimedOut,
    Skipped,
    Interrupted,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkPhase {
    Request,
    Response,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StdStream {
    Stdout,
    Stderr,
}

/// Everything that is not a video frame.
///
/// Page ids are opaque strings minted by whoever opened the page; they are unique
/// within a run. Test ids come from the runner (Playwright `TestCase.id`, Vitest
/// `TestCase.id`) so reporter and preload events can be joined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Event {
    Hello {
        role: Role,
        runner: String,
        pid: u32,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        protocol: Option<u32>,
    },

    // --- test lifecycle (reporter) ---
    RunBegin {
        #[serde(default)]
        total_tests: Option<u32>,
    },
    TestBegin {
        test_id: String,
        title: String,
        #[serde(default)]
        title_path: Vec<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        worker_index: Option<i32>,
    },
    StepBegin {
        test_id: String,
        step_id: String,
        title: String,
        #[serde(default)]
        category: Option<String>,
    },
    StepEnd {
        test_id: String,
        step_id: String,
        #[serde(default)]
        duration_ms: Option<f64>,
        #[serde(default)]
        error: Option<String>,
    },
    TestEnd {
        test_id: String,
        status: TestStatus,
        #[serde(default)]
        duration_ms: Option<f64>,
        #[serde(default)]
        error: Option<String>,
        /// Why a test was skipped (Playwright skip annotation, Vitest note).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    RunEnd {
        status: String,
    },

    // --- browser instrumentation (preload / cdp) ---
    PageOpened {
        page_id: String,
        #[serde(default)]
        test_id: Option<String>,
        #[serde(default)]
        worker_index: Option<i32>,
        #[serde(default)]
        browser: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    PageClosed {
        page_id: String,
    },
    Navigated {
        page_id: String,
        url: String,
    },
    Network {
        page_id: String,
        request_id: String,
        phase: NetworkPhase,
        ts: f64,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        status: Option<u16>,
        #[serde(default)]
        resource_type: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    Console {
        page_id: String,
        level: String,
        text: String,
        ts: f64,
    },
    AdapterLog {
        level: String,
        message: String,
    },

    // --- emitted by the core itself, never by adapters ---
    RunStarted {
        label: String,
        runner: String,
        cwd: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Stdio {
        stream: StdStream,
        line: String,
    },
    ProcessExited {
        #[serde(default)]
        code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameHeader {
    pub page_id: String,
    /// Milliseconds since the Unix epoch, as reported by the browser.
    pub ts: f64,
    #[serde(default)]
    pub viewport_width: Option<u32>,
    #[serde(default)]
    pub viewport_height: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Event(Event),
    Frame { header: FrameHeader, jpeg: Bytes },
}

#[derive(Debug)]
pub enum DecodeError {
    TooLarge(usize),
    UnknownKind(u8),
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "message too large: {n} bytes"),
            Self::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            Self::Malformed(m) => write!(f, "malformed message: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Try to decode one message from the front of `buf`. Returns `Ok(None)` when more
/// bytes are needed. On success the consumed bytes are removed from `buf`.
///
/// A message whose JSON fails to parse (e.g. an event type from a newer adapter) is
/// consumed and reported as `Malformed`, so the stream stays in sync.
pub fn decode(buf: &mut BytesMut) -> Result<Option<Message>, DecodeError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_MESSAGE_LEN {
        return Err(DecodeError::TooLarge(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    buf.advance(4);
    let mut body = buf.split_to(len).freeze();
    let kind = body.get_u8();
    match kind {
        KIND_EVENT => serde_json::from_slice(&body)
            .map(|e| Some(Message::Event(e)))
            .map_err(|e| DecodeError::Malformed(e.to_string())),
        KIND_FRAME => {
            if body.len() < 4 {
                return Err(DecodeError::Malformed("frame body too short".into()));
            }
            let hlen = body.get_u32() as usize;
            if body.len() < hlen {
                return Err(DecodeError::Malformed("frame header overruns body".into()));
            }
            let header_bytes = body.split_to(hlen);
            let header = serde_json::from_slice(&header_bytes)
                .map_err(|e| DecodeError::Malformed(e.to_string()))?;
            Ok(Some(Message::Frame { header, jpeg: body }))
        }
        other => Err(DecodeError::UnknownKind(other)),
    }
}

pub fn encode_event(event: &Event, out: &mut BytesMut) {
    let json = serde_json::to_vec(event).expect("Event serializes");
    out.put_u32((json.len() + 1) as u32);
    out.put_u8(KIND_EVENT);
    out.put_slice(&json);
}

pub fn encode_frame(header: &FrameHeader, jpeg: &[u8], out: &mut BytesMut) {
    let h = serde_json::to_vec(header).expect("FrameHeader serializes");
    out.put_u32((1 + 4 + h.len() + jpeg.len()) as u32);
    out.put_u8(KIND_FRAME);
    out.put_u32(h.len() as u32);
    out.put_slice(&h);
    out.put_slice(jpeg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip_and_partial_reads() {
        let ev = Event::TestBegin {
            test_id: "abc".into(),
            title: "works".into(),
            title_path: vec!["file.spec.ts".into(), "works".into()],
            file: Some("file.spec.ts".into()),
            line: Some(3),
            project: None,
            worker_index: Some(0),
        };
        let mut wire = BytesMut::new();
        encode_event(&ev, &mut wire);
        let full = wire.clone();

        let mut partial = BytesMut::from(&full[..full.len() - 1]);
        assert!(decode(&mut partial).unwrap().is_none());

        match decode(&mut wire).unwrap() {
            Some(Message::Event(got)) => assert_eq!(got, ev),
            other => panic!("unexpected {other:?}"),
        }
        assert!(wire.is_empty());
    }

    #[test]
    fn frame_roundtrip() {
        let header = FrameHeader {
            page_id: "p1".into(),
            ts: 1.5,
            viewport_width: Some(800),
            viewport_height: Some(600),
        };
        let mut wire = BytesMut::new();
        encode_frame(&header, b"\xff\xd8jpeg", &mut wire);
        match decode(&mut wire).unwrap() {
            Some(Message::Frame { header: h, jpeg }) => {
                assert_eq!(h, header);
                assert_eq!(&jpeg[..], b"\xff\xd8jpeg");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn js_shaped_json_parses() {
        let json = br#"{"type":"pageOpened","pageId":"1-0","testId":"t1","browser":"chromium"}"#;
        let ev: Event = serde_json::from_slice(json).unwrap();
        assert!(matches!(ev, Event::PageOpened { ref page_id, .. } if page_id == "1-0"));
    }

    #[test]
    fn unknown_event_type_is_consumed() {
        let json = br#"{"type":"somethingNew"}"#;
        let mut wire = BytesMut::new();
        wire.put_u32((json.len() + 1) as u32);
        wire.put_u8(KIND_EVENT);
        wire.put_slice(json);
        encode_event(&Event::RunEnd { status: "passed".into() }, &mut wire);

        assert!(matches!(decode(&mut wire), Err(DecodeError::Malformed(_))));
        assert!(matches!(decode(&mut wire), Ok(Some(Message::Event(Event::RunEnd { .. })))));
    }
}
