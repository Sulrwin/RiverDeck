use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 request
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub id: Option<serde_json::Value>,
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// JSON-RPC 2.0 response
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub id: serde_json::Value,
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

/// JSON-RPC event (notification, no response expected)
#[derive(Debug, Serialize)]
pub struct JsonRpcEvent {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
}

// Request parameter types
#[derive(Debug, Deserialize)]
pub struct GetApplicationInstanceParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
}

#[derive(Debug, Deserialize)]
pub struct GetApplicationInstanceAtIndexParams {
    pub index: usize,
}

#[derive(Debug, Deserialize)]
pub struct SetApplicationVolumeParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    pub volume: f32,
}

#[derive(Debug, Deserialize)]
pub struct SetApplicationMuteParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    pub mute: bool,
}

#[derive(Debug, Deserialize)]
pub struct GetApplicationImageParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    #[serde(rename = "imageSize")]
    pub image_size: u32,
}

// Response types
#[derive(Debug, Serialize)]
pub struct ApplicationInstanceCountResponse {
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApplicationInfoResponse {
    #[serde(rename = "processID")]
    pub process_id: u32,
    #[serde(rename = "executableFile")]
    pub executable_file: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub volume: f32,
    pub mute: bool,
    pub activity: u32,
}

#[derive(Debug, Serialize)]
pub struct DeviceInfoResponse {
    pub id: String,
    #[serde(rename = "friendlyName")]
    pub friendly_name: String,
    pub volume: f32,
    pub mute: bool,
}

// Event parameter types
#[derive(Debug, Serialize)]
pub struct AppAddRemoveEventParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    #[serde(rename = "appAddedRemoved")]
    pub app_added_removed: String, // "Added" or "Removed"
}

#[derive(Debug, Serialize)]
pub struct AppVolumeChangedParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    pub volume: f32,
}

#[derive(Debug, Serialize)]
pub struct AppMuteChangedParams {
    #[serde(rename = "processID")]
    pub process_id: u32,
    pub mute: bool,
}
