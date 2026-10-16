use serde::{Deserialize, Serialize};

use acad_bridge::{FeedPoll, PipeClient};
use std::sync::{Arc, Mutex};

pub use acad_bridge::FeedHandle;

pub const DEFAULT_PIPE_NAME: &str = "acad_layer_standardizer";
pub const IPC_PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawingSnapshot {
    #[serde(default)]
    pub drawing_id: String,
    pub drawing_name: String,
    #[serde(default = "default_heuristic_threshold")]
    pub heuristic_threshold: f64,
    #[serde(default)]
    pub template_name: String,
    #[serde(default)]
    pub template_path: String,
    pub source_layers: Vec<String>,
    pub standard_layers: Vec<String>,
    pub empty_layers: Vec<String>,
    pub memory_mappings: std::collections::HashMap<String, String>,
    pub target_filters: Vec<TargetFilter>,
    #[serde(default)]
    pub always_hidden_targets: Vec<String>,
}

fn default_heuristic_threshold() -> f64 {
    0.6
}

/// Standard (target) layer names read from a template drawing by the connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandardLayersInfo {
    pub template_name: String,
    pub template_path: String,
    pub layers: Vec<String>,
}

/// Lightweight identity of the drawing AutoCAD currently has active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveDrawingInfo {
    pub drawing_id: String,
    pub display_name: String,
    pub layer_fingerprint: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetFilter {
    pub name: String,
    pub sort_group: String,
    pub layers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum IpcRequest {
    Ping,
    GetDrawingSnapshot,
    GetStandardLayers {
        protocol_version: u32,
        path: String,
    },
    GetActiveDrawing {
        known_revision: Option<u64>,
    },
    ApplyPlan {
        protocol_version: u32,
        drawing_name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        drawing_id: String,
        mappings: Vec<LayerMapping>,
        remember: bool,
        properties: PropertyMatchSettings,
    },
    PurgeEmptyLayers {
        protocol_version: u32,
        drawing_name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        drawing_id: String,
        layers: Vec<String>,
    },
    PollEvents {
        protocol_version: u32,
        since: Option<u64>,
        pending: Vec<PendingEntry>,
    },
    GetLayersForDrawing {
        protocol_version: u32,
        drawing_id: String,
    },
    /// Re-runs a close the connector blocked. `kind` is "drawing" or "quit";
    /// `pending` replaces the last report so the replayed close is allowed.
    ReplayClose {
        protocol_version: u32,
        kind: String,
        drawing_id: String,
        pending: Vec<PendingEntry>,
    },
}

/// A drawing's layer names, read by id (not necessarily the one LSTDR captured).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawingLayersInfo {
    pub drawing_id: String,
    pub drawing_name: String,
    pub source_layers: Vec<String>,
    pub empty_layers: Vec<String>,
}

/// One entry in the connector's change feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEvent {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Count of unsaved mapping rows the window holds for a drawing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    pub drawing_id: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LayerMapping {
    pub source_layer: String,
    pub target_layer: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PropertyMatchSettings {
    pub match_color: bool,
    pub match_linetype: bool,
    pub match_lineweight: bool,
    pub make_by_layer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum IpcResponse {
    Pong,
    DrawingSnapshot(DrawingSnapshot),
    ActiveDrawing(ActiveDrawingInfo),
    StandardLayers(StandardLayersInfo),
    ActiveDrawingUnchanged,
    NoActiveDrawing,
    Applied {
        protocol_version: u32,
        count: usize,
        remembered: bool,
        warning: Option<String>,
    },
    Purged {
        protocol_version: u32,
        layers: Vec<String>,
    },
    DrawingLayers(DrawingLayersInfo),
    Events {
        head: u64,
        reset: bool,
        events: Vec<FeedEvent>,
    },
    Replayed,
    Error(String),
}

#[cfg(windows)]
pub fn request_drawing_snapshot() -> Result<IpcResponse, String> {
    request(IpcRequest::GetDrawingSnapshot)
}

#[cfg(windows)]
pub fn get_standard_layers(path: String) -> Result<IpcResponse, String> {
    request(IpcRequest::GetStandardLayers {
        protocol_version: IPC_PROTOCOL_VERSION,
        path,
    })
}

#[cfg(windows)]
pub fn get_layers_for_drawing(drawing_id: String) -> Result<IpcResponse, String> {
    request(IpcRequest::GetLayersForDrawing {
        protocol_version: IPC_PROTOCOL_VERSION,
        drawing_id,
    })
}

#[cfg(windows)]
pub fn get_active_drawing(known_revision: Option<u64>) -> Result<IpcResponse, String> {
    request(IpcRequest::GetActiveDrawing { known_revision })
}

#[cfg(windows)]
pub fn apply_plan(
    drawing_name: String,
    drawing_id: String,
    mappings: Vec<LayerMapping>,
    remember: bool,
    properties: PropertyMatchSettings,
) -> Result<IpcResponse, String> {
    request(IpcRequest::ApplyPlan {
        protocol_version: IPC_PROTOCOL_VERSION,
        drawing_name,
        drawing_id,
        mappings,
        remember,
        properties,
    })
}

#[cfg(windows)]
pub fn purge_empty_layers(
    drawing_name: String,
    drawing_id: String,
    layers: Vec<String>,
) -> Result<IpcResponse, String> {
    request(IpcRequest::PurgeEmptyLayers {
        protocol_version: IPC_PROTOCOL_VERSION,
        drawing_name,
        drawing_id,
        layers,
    })
}

/// Polls the connector's change feed. Uses 3 connection attempts so a
/// once-a-second poll fails fast when AutoCAD is gone.
#[cfg(windows)]
pub fn poll_events(since: Option<u64>, pending: Vec<PendingEntry>) -> Result<IpcResponse, String> {
    send(
        IpcRequest::PollEvents {
            protocol_version: IPC_PROTOCOL_VERSION,
            since,
            pending,
        },
        3,
    )
}

/// Asks the connector to re-run a blocked drawing close or quit.
#[cfg(windows)]
pub fn replay_close(
    kind: String,
    drawing_id: String,
    pending: Vec<PendingEntry>,
) -> Result<IpcResponse, String> {
    request(IpcRequest::ReplayClose {
        protocol_version: IPC_PROTOCOL_VERSION,
        kind,
        drawing_id,
        pending,
    })
}

/// The kit's pipe client, aimed at this product's pipe and trace file.
#[cfg(windows)]
fn client() -> PipeClient {
    PipeClient::new(DEFAULT_PIPE_NAME, "AcLayerStandardizer-rust-ipc.log")
}

#[cfg(windows)]
fn request(request: IpcRequest) -> Result<IpcResponse, String> {
    send(request, 50)
}

#[cfg(windows)]
fn send(request: IpcRequest, attempts: u32) -> Result<IpcResponse, String> {
    let trace = should_trace(&request);
    client().send(&request, attempts, trace)
}

/// Polls fire about once a second; tracing them would grow the log without bound.
#[cfg_attr(not(windows), allow(dead_code))]
fn should_trace(request: &IpcRequest) -> bool {
    !matches!(request, IpcRequest::PollEvents { .. })
}

/// One change-feed message, as the window receives it.
pub type FeedMessage = acad_bridge::FeedMessage<FeedEvent>;

/// Starts the change-feed poll loop (about one poll per second). The kit runs the
/// loop; this maps the connector's answers onto it.
pub fn spawn_feed<F: Fn(FeedMessage) + Send + 'static>(
    pending: Arc<Mutex<Vec<PendingEntry>>>,
    deliver: F,
) -> FeedHandle {
    acad_bridge::spawn_feed(
        pending,
        |since, report| match poll_events(since, report) {
            Ok(IpcResponse::Events { head, reset, events }) => Ok(FeedPoll::Events { head, reset, events }),
            Ok(IpcResponse::Error(error)) => Ok(FeedPoll::Error(error)),
            Ok(_) => Ok(FeedPoll::Other),
            Err(error) => Err(error),
        },
        deliver,
    )
}


#[cfg(not(windows))]
pub fn request_drawing_snapshot() -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn get_standard_layers(_path: String) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn get_layers_for_drawing(_drawing_id: String) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn get_active_drawing(_known_revision: Option<u64>) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn apply_plan(
    _drawing_name: String,
    _drawing_id: String,
    _mappings: Vec<LayerMapping>,
    _remember: bool,
    _properties: PropertyMatchSettings,
) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn purge_empty_layers(
    _drawing_name: String,
    _drawing_id: String,
    _layers: Vec<String>,
) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn poll_events(
    _since: Option<u64>,
    _pending: Vec<PendingEntry>,
) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(not(windows))]
pub fn replay_close(
    _kind: String,
    _drawing_id: String,
    _pending: Vec<PendingEntry>,
) -> Result<IpcResponse, String> {
    Err("AutoCAD named-pipe IPC is available only on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_without_drawing_id_still_parses() {
        let json = r#"{"drawing_name":"A.dwg","source_layers":["A"],"standard_layers":["B"],"empty_layers":[],"memory_mappings":{},"target_filters":[]}"#;
        let snapshot: DrawingSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.drawing_id, "");
    }

    #[test]
    fn get_active_drawing_serializes_known_revision() {
        let json = serde_json::to_string(&IpcRequest::GetActiveDrawing {
            known_revision: Some(7),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"GetActiveDrawing","payload":{"known_revision":7}}"#
        );
    }

    #[test]
    fn active_drawing_response_round_trips() {
        let json = r#"{"type":"ActiveDrawing","payload":{"drawing_id":"doc-1","display_name":"A.dwg","layer_fingerprint":"ab12","revision":3}}"#;
        let IpcResponse::ActiveDrawing(info) = serde_json::from_str(json).unwrap() else {
            panic!("expected ActiveDrawing");
        };
        assert_eq!(info.drawing_id, "doc-1");
        assert_eq!(info.display_name, "A.dwg");
        assert_eq!(info.layer_fingerprint, "ab12");
        assert_eq!(info.revision, 3);
        assert!(matches!(
            serde_json::from_str::<IpcResponse>(r#"{"type":"ActiveDrawingUnchanged"}"#).unwrap(),
            IpcResponse::ActiveDrawingUnchanged
        ));
        assert!(matches!(
            serde_json::from_str::<IpcResponse>(r#"{"type":"NoActiveDrawing"}"#).unwrap(),
            IpcResponse::NoActiveDrawing
        ));
    }

    #[test]
    fn apply_plan_omits_empty_drawing_id() {
        let request = |drawing_id: &str| {
            serde_json::to_string(&IpcRequest::ApplyPlan {
                protocol_version: IPC_PROTOCOL_VERSION,
                drawing_name: "A.dwg".into(),
                drawing_id: drawing_id.into(),
                mappings: vec![],
                remember: false,
                properties: PropertyMatchSettings {
                    match_color: true,
                    match_linetype: true,
                    match_lineweight: true,
                    make_by_layer: false,
                },
            })
            .unwrap()
        };
        assert!(!request("").contains("drawing_id"));
        assert!(request("doc-2").contains(r#""drawing_id":"doc-2""#));
    }

    #[test]
    fn poll_events_request_shape() {
        let json = serde_json::to_value(&IpcRequest::PollEvents {
            protocol_version: IPC_PROTOCOL_VERSION,
            since: Some(3),
            pending: vec![PendingEntry {
                drawing_id: "d1".into(),
                count: 2,
            }],
        })
        .unwrap();
        assert_eq!(json["type"], "PollEvents");
        assert_eq!(json["payload"]["since"], 3);
        assert_eq!(json["payload"]["pending"][0]["drawing_id"], "d1");
        assert_eq!(json["payload"]["pending"][0]["count"], 2);
    }

    #[test]
    fn polls_are_not_traced_but_other_requests_are() {
        let poll = IpcRequest::PollEvents {
            protocol_version: IPC_PROTOCOL_VERSION,
            since: None,
            pending: vec![],
        };
        assert!(!should_trace(&poll));
        assert!(should_trace(&IpcRequest::Ping));
        assert!(should_trace(&IpcRequest::GetDrawingSnapshot));
    }

    #[test]
    fn events_response_deserializes() {
        let json = r#"{"type":"Events","payload":{"head":4,"reset":false,"events":[{"seq":4,"type":"DrawingActivated","payload":{"drawing_id":"d1"}}]}}"#;
        let IpcResponse::Events {
            head,
            reset,
            events,
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected Events");
        };
        assert_eq!(head, 4);
        assert!(!reset);
        assert_eq!(events[0].seq, 4);
        assert_eq!(events[0].kind, "DrawingActivated");
        assert_eq!(events[0].payload["drawing_id"], "d1");
    }

    #[test]
    fn get_layers_for_drawing_request_shape() {
        let json = serde_json::to_string(&IpcRequest::GetLayersForDrawing {
            protocol_version: IPC_PROTOCOL_VERSION,
            drawing_id: "d1".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"GetLayersForDrawing","payload":{"protocol_version":4,"drawing_id":"d1"}}"#
        );
    }

    #[test]
    fn drawing_layers_response_deserializes() {
        let json = r#"{"type":"DrawingLayers","payload":{"drawing_id":"d1","drawing_name":"A.dwg","source_layers":["0","WALL"],"empty_layers":["WALL"]}}"#;
        let IpcResponse::DrawingLayers(info) = serde_json::from_str(json).unwrap() else {
            panic!("expected DrawingLayers");
        };
        assert_eq!(info.drawing_id, "d1");
        assert_eq!(info.drawing_name, "A.dwg");
        assert_eq!(info.source_layers, vec!["0", "WALL"]);
        assert_eq!(info.empty_layers, vec!["WALL"]);
    }

    #[test]
    fn get_standard_layers_request_shape() {
        let json = serde_json::to_string(&IpcRequest::GetStandardLayers {
            protocol_version: IPC_PROTOCOL_VERSION,
            path: "X.dwg".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"GetStandardLayers","payload":{"protocol_version":4,"path":"X.dwg"}}"#
        );
    }

    #[test]
    fn standard_layers_response_round_trips() {
        let json = r#"{"type":"StandardLayers","payload":{"template_name":"T.dws","template_path":"C:/T.dws","layers":["0","A-WALL"]}}"#;
        let IpcResponse::StandardLayers(info) = serde_json::from_str(json).unwrap() else {
            panic!("expected StandardLayers");
        };
        assert_eq!(info.template_name, "T.dws");
        assert_eq!(info.template_path, "C:/T.dws");
        assert_eq!(info.layers, vec!["0", "A-WALL"]);
    }

    #[test]
    fn replay_close_request_shape() {
        let json = serde_json::to_string(&IpcRequest::ReplayClose {
            protocol_version: IPC_PROTOCOL_VERSION,
            kind: "drawing".into(),
            drawing_id: "d1".into(),
            pending: vec![PendingEntry {
                drawing_id: "d2".into(),
                count: 1,
            }],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"ReplayClose","payload":{"protocol_version":4,"kind":"drawing","drawing_id":"d1","pending":[{"drawing_id":"d2","count":1}]}}"#
        );
    }

    #[test]
    fn replayed_response_deserializes() {
        assert!(matches!(
            serde_json::from_str::<IpcResponse>(r#"{"type":"Replayed"}"#).unwrap(),
            IpcResponse::Replayed
        ));
    }
}
