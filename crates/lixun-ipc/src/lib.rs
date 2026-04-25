//! Lixun IPC — unix-socket protocol and codec.
//!
//! Transport: unix-socket at `$XDG_RUNTIME_DIR/lixun.sock`, fallback `/tmp/lixun-$UID.sock`.
//! Framing: `u32` big-endian length prefix + JSON payload.
//! Socket permissions: `0600`.

use bytes::{Buf, BufMut, BytesMut};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio_util::codec::{Decoder, Encoder};

use lixun_core::Hit;

pub mod gui;

pub const PROTOCOL_VERSION: u16 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WatcherStats {
    pub directories: u64,
    pub excluded: u64,
    pub errors: u64,
    pub overflow_events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WriterStats {
    pub commits: u64,
    pub last_commit_latency_ms: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStats {
    pub rss_bytes: u64,
    pub vm_peak_bytes: u64,
    pub vm_size_bytes: u64,
    pub vm_swap_bytes: u64,
}

/// OCR queue + worker observability snapshot exposed to CLI/GUI.
///
/// The daemon populates this on `Request::Status` when OCR is enabled.
/// All counts are best-effort point-in-time reads against the persistent
/// queue (`queue_*`) or in-memory worker counters (`drained_total`,
/// `last_drain_at`). `last_drain_at` uses Unix seconds with sentinel
/// `None` for "never drained since startup".
///
/// Contract: `queue_total == queue_pending + queue_failed`. Sliced in
/// the daemon via a single SQL round-trip (see
/// `OcrQueue::stats(max_attempts)`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OcrStats {
    pub queue_total: u64,
    pub queue_pending: u64,
    pub queue_failed: u64,
    pub drained_total: u64,
    pub last_drain_at: Option<i64>,
}

/// The oldest protocol version this build can negotiate with.
pub const MIN_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Toggle,
    Show,
    Hide,
    Search {
        q: String,
        limit: u32,
        /// When `true`, daemon populates `Response::HitsWithExtras{,V3}.explanations`
        /// with one human-readable score-breakdown string per hit. Default
        /// `false` via `#[serde(default)]` so older clients that omit the
        /// field keep decoding as non-explain requests \u2014 no `PROTOCOL_VERSION`
        /// bump needed (plan DB-11).
        #[serde(default)]
        explain: bool,
    },
    Reindex { paths: Vec<PathBuf> },
    Status,
    RecordClick { doc_id: String },
    RecordQuery { q: String },
    RecordQueryClick { doc_id: String, query: String },
    SearchHistory { limit: u32 },
    /// Open a preview window for the given hit. The GUI embeds the
    /// full Hit rather than a DocId because app / calculator /
    /// recent-query hits never reach Tantivy and so cannot be
    /// resolved from an id alone. The daemon writes the Hit to a
    /// tempfile and spawns `lixun-preview` with `--hit-json <path>`.
    /// Boxed to keep the `Request` enum compact.
    ///
    /// `monitor` carries the connector name (e.g. `"eDP-1"`,
    /// `"DP-2"`) of the display the launcher is currently on, so
    /// the preview binary can open on the same monitor without
    /// having to guess from a pointer position that may not even
    /// intersect its own (not-yet-mapped) surface. Optional for
    /// backward compat and because a GUI that fails to resolve its
    /// monitor still prefers to spawn a preview than to refuse
    /// one — the binary falls back to pointer-based selection.
    Preview {
        hit: Box<Hit>,
        monitor: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Hits(Vec<Hit>),
    HitsWithExtras {
        hits: Vec<Hit>,
        calculation: Option<lixun_core::Calculation>,
        /// Per-hit human-readable score breakdown, one entry per `hits`
        /// item. Empty (via `#[serde(default)]`) unless the caller sent
        /// `Request::Search { explain: true, .. }`. Old clients that
        /// omit the field on deserialize see an empty vec and stay on
        /// the non-explain code path \u2014 no `PROTOCOL_VERSION` bump.
        #[serde(default)]
        explanations: Vec<String>,
    },
    HitsWithExtrasV3 {
        hits: Vec<Hit>,
        calculation: Option<lixun_core::Calculation>,
        top_hit: Option<lixun_core::DocId>,
        #[serde(default)]
        explanations: Vec<String>,
    },
    Status {
        indexed_docs: u64,
        last_reindex: Option<DateTime<Utc>>,
        errors: u32,
        #[serde(default)]
        watcher: Option<WatcherStats>,
        #[serde(default)]
        writer: Option<WriterStats>,
        #[serde(default)]
        memory: Option<MemoryStats>,
        #[serde(default)]
        reindex_in_progress: bool,
        #[serde(default)]
        reindex_started: Option<DateTime<Utc>>,
        #[serde(default)]
        ocr: Option<OcrStats>,
    },
    Visibility {
        visible: bool,
    },
    Queries(Vec<String>),
    Error(String),
}

/// Framing codec: u32 BE length + u16 version + JSON payload.
pub struct FrameCodec {
    state: DecodeState,
}

enum DecodeState {
    Header,
    Version(usize),
    Payload(usize),
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self {
            state: DecodeState::Header,
        }
    }
}

impl Encoder<Request> for FrameCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Request, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let json = serde_json::to_vec(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let total_len = 2 + json.len();
        dst.put_u32(total_len as u32);
        dst.put_u16(PROTOCOL_VERSION);
        dst.put_slice(&json);
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = Request;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                DecodeState::Header => {
                    if src.len() < 4 {
                        return Ok(None);
                    }
                    let len = src.get_u32() as usize;
                    if len < 2 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "frame too short for version",
                        ));
                    }
                    self.state = DecodeState::Version(len);
                }
                DecodeState::Version(len) => {
                    if src.len() < 2 {
                        return Ok(None);
                    }
                    let version = src.get_u16();
                    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "version {} outside supported {}..={}",
                                version, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION
                            ),
                        ));
                    }
                    self.state = DecodeState::Payload(len - 2);
                }
                DecodeState::Payload(len) => {
                    if src.len() < len {
                        return Ok(None);
                    }
                    let data = src.split_to(len);
                    let req: Request = serde_json::from_slice(&data)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    self.state = DecodeState::Header;
                    return Ok(Some(req));
                }
            }
        }
    }
}

/// Determine the socket path.
pub fn socket_path() -> PathBuf {
    let uid = get_uid_fallback();
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(runtime_dir).join("lixun.sock");
        return path;
    }
    PathBuf::from(format!("/tmp/lixun-{}.sock", uid))
}

pub(crate) fn get_uid_fallback() -> u32 {
    if let Ok(uid) = std::env::var("UID")
        && let Ok(uid) = uid.parse::<u32>()
    {
        return uid;
    }
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Ok(uid) = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .parse::<u32>()
            {
                return uid;
            }
        }
    }
    1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_toggle() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Request::Toggle, &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(decoded, Request::Toggle));
    }

    #[test]
    fn test_encode_decode_search() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let req = Request::Search {
            q: "hello world".to_string(),
            limit: 10,
            explain: false,
        };
        codec.encode(req.clone(), &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Request::Search { q, limit, explain } => {
                assert_eq!(q, "hello world");
                assert_eq!(limit, 10);
                assert!(!explain);
            }
            _ => panic!("Expected Search variant"),
        }
    }

    #[test]
    fn test_search_explain_defaults_false_on_missing_field() {
        // Back-compat guard for `Request::Search.explain` (plan DB-11):
        // payloads from pre-T6 clients omit the field entirely; serde
        // must synthesize `explain: false` rather than rejecting the
        // frame. Exercises the `#[serde(default)]` path directly.
        let legacy_json = br#"{"Search":{"q":"hi","limit":5}}"#;
        let req: Request = serde_json::from_slice(legacy_json).unwrap();
        match req {
            Request::Search { q, limit, explain } => {
                assert_eq!(q, "hi");
                assert_eq!(limit, 5);
                assert!(!explain, "legacy payload must default to explain=false");
            }
            _ => panic!("Expected Search variant"),
        }
    }

    #[test]
    fn test_hits_with_extras_v3_explanations_defaults_empty_on_missing_field() {
        // Back-compat guard for `Response::HitsWithExtrasV3.explanations`
        // (plan DB-11). A v3 payload shipped by a pre-T6 daemon omits
        // `explanations`; serde must synthesize an empty vec so the
        // CLI/GUI keep treating the response as non-explain. Mirrors
        // the Request-side guard above.
        let legacy_json = br#"{"HitsWithExtrasV3":{"hits":[],"calculation":null,"top_hit":null}}"#;
        let resp: Response = serde_json::from_slice(legacy_json).unwrap();
        match resp {
            Response::HitsWithExtrasV3 {
                hits,
                calculation,
                top_hit,
                explanations,
            } => {
                assert!(hits.is_empty());
                assert!(calculation.is_none());
                assert!(top_hit.is_none());
                assert!(explanations.is_empty());
            }
            _ => panic!("Expected HitsWithExtrasV3 variant"),
        }
    }

    #[test]
    fn test_encode_decode_reindex() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let req = Request::Reindex {
            paths: vec![PathBuf::from("/tmp/test")],
        };
        codec.encode(req, &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Request::Reindex { paths } => {
                assert_eq!(paths.len(), 1);
                assert_eq!(paths[0], PathBuf::from("/tmp/test"));
            }
            _ => panic!("Expected Reindex variant"),
        }
    }

    #[test]
    fn test_encode_decode_status() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Request::Status, &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(decoded, Request::Status));
    }

    #[test]
    fn test_version_mismatch() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let json = serde_json::to_vec(&Request::Toggle).unwrap();
        buf.put_u32((2 + json.len()) as u32);
        buf.put_u16(99);
        buf.put_slice(&json);

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("outside supported"),
            "expected 'outside supported' in error, got: {}",
            err
        );
    }

    #[test]
    fn test_protocol_v3_record_query_click_roundtrip() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let req = Request::RecordQueryClick {
            doc_id: "fs:/a".into(),
            query: "foo".into(),
        };
        codec.encode(req, &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Request::RecordQueryClick { doc_id, query } => {
                assert_eq!(doc_id, "fs:/a");
                assert_eq!(query, "foo");
            }
            other => panic!("Expected RecordQueryClick, got {:?}", other),
        }
    }

    #[test]
    fn test_codec_accepts_protocol_v2_frame() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let json = serde_json::to_vec(&Request::Toggle).unwrap();
        buf.put_u32((2 + json.len()) as u32);
        buf.put_u16(2);
        buf.put_slice(&json);

        let decoded = codec.decode(&mut buf).unwrap();
        assert!(
            matches!(decoded, Some(Request::Toggle)),
            "expected Some(Request::Toggle), got {:?}",
            decoded
        );
    }

    #[test]
    fn test_incomplete_frame_returns_none() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(10);
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_buffer_returns_none() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_multiple_requests_in_buffer() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Request::Toggle, &mut buf).unwrap();
        codec.encode(Request::Status, &mut buf).unwrap();

        let first = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(first, Request::Toggle));

        let second = codec.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(second, Request::Status));
    }

    #[test]
    fn test_socket_path_format() {
        let path = socket_path();
        assert!(path.to_string_lossy().contains("lixun.sock"));
    }

    #[test]
    fn test_hits_with_extras_roundtrip_empty() {
        let resp = Response::HitsWithExtras {
            hits: vec![],
            calculation: None,
            explanations: vec![],
        };
        let json = serde_json::to_vec(&resp).unwrap();
        let decoded: Response = serde_json::from_slice(&json).unwrap();
        match decoded {
            Response::HitsWithExtras {
                hits,
                calculation,
                explanations,
            } => {
                assert!(hits.is_empty());
                assert!(calculation.is_none());
                assert!(explanations.is_empty());
            }
            _ => panic!("expected HitsWithExtras"),
        }
    }

    #[test]
    fn test_hits_with_extras_roundtrip_with_calculation() {
        let resp = Response::HitsWithExtras {
            hits: vec![],
            calculation: Some(lixun_core::Calculation {
                expr: "2+2".into(),
                result: "4".into(),
            }),
            explanations: vec![],
        };
        let json = serde_json::to_vec(&resp).unwrap();
        let decoded: Response = serde_json::from_slice(&json).unwrap();
        match decoded {
            Response::HitsWithExtras { calculation, .. } => {
                let c = calculation.expect("calculation present");
                assert_eq!(c.expr, "2+2");
                assert_eq!(c.result, "4");
            }
            _ => panic!("expected HitsWithExtras"),
        }
    }

    #[test]
    fn test_hits_variant_still_parses_v1() {
        let resp = Response::Hits(vec![]);
        let json = serde_json::to_vec(&resp).unwrap();
        let decoded: Response = serde_json::from_slice(&json).unwrap();
        assert!(matches!(decoded, Response::Hits(h) if h.is_empty()));
    }

    fn fake_hit() -> Hit {
        use lixun_core::{Action, Category, DocId};
        Hit {
            id: DocId("fs:/tmp/demo.txt".into()),
            category: Category::File,
            title: "demo.txt".into(),
            subtitle: "/tmp".into(),
            icon_name: None,
            kind_label: Some("Plain text".into()),
            score: 1.0,
            action: Action::OpenFile {
                path: "/tmp/demo.txt".into(),
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    #[test]
    fn test_encode_decode_preview_roundtrips_full_hit() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let req = Request::Preview {
            hit: Box::new(fake_hit()),
            monitor: Some("eDP-1".into()),
        };
        codec.encode(req, &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Request::Preview { hit, monitor } => {
                assert_eq!(hit.id.0, "fs:/tmp/demo.txt");
                assert_eq!(hit.title, "demo.txt");
                assert!(matches!(hit.action, lixun_core::Action::OpenFile { .. }));
                assert_eq!(monitor.as_deref(), Some("eDP-1"));
            }
            other => panic!("Expected Preview variant, got {:?}", other),
        }
    }

    #[test]
    fn test_preview_monitor_none_roundtrips() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        let req = Request::Preview {
            hit: Box::new(fake_hit()),
            monitor: None,
        };
        codec.encode(req, &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Request::Preview { monitor, .. } => assert_eq!(monitor, None),
            other => panic!("Expected Preview variant, got {:?}", other),
        }
    }

    #[test]
    fn test_preview_interleaved_with_other_variants() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Request::Toggle, &mut buf).unwrap();
        codec
            .encode(
                Request::Preview {
                    hit: Box::new(fake_hit()),
                    monitor: None,
                },
                &mut buf,
            )
            .unwrap();
        codec.encode(Request::Status, &mut buf).unwrap();

        assert!(matches!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Request::Toggle
        ));
        assert!(matches!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Request::Preview { .. }
        ));
        assert!(matches!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Request::Status
        ));
    }
}
