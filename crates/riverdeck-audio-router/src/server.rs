use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::audio_backend::AudioBackend;
use crate::protocol::*;

pub async fn start_audio_router_server(backend: Arc<dyn AudioBackend>) -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:1844").await?;
    log::info!("Audio Router WebSocket server listening on 127.0.0.1:1844");

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                log::info!("Audio Router: New client connected from {}", addr);
                let backend = backend.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, backend).await {
                        log::error!("Audio Router: Client error: {:#}", e);
                    }
                });
            }
            Err(e) => {
                log::error!("Audio Router: Failed to accept connection: {:#}", e);
            }
        }
    }
}

async fn handle_client(stream: TcpStream, backend: Arc<dyn AudioBackend>) -> anyhow::Result<()> {
    let ws_stream = accept_async(stream).await?;
    let (mut write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let msg = msg?;

        if let Message::Text(text) = msg {
            log::debug!("Audio Router: Received message: {}", text);

            match serde_json::from_str::<JsonRpcRequest>(&text) {
                Ok(request) => {
                    let response = handle_request(&request, &backend).await;
                    let response_text = serde_json::to_string(&response)?;
                    log::debug!("Audio Router: Sending response: {}", response_text);
                    write.send(Message::Text(response_text.into())).await?;
                }
                Err(e) => {
                    log::error!("Audio Router: Failed to parse request: {:#}", e);
                }
            }
        }
    }

    Ok(())
}

async fn handle_request(
    request: &JsonRpcRequest,
    backend: &Arc<dyn AudioBackend>,
) -> JsonRpcResponse {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);

    match request.method.as_str() {
        "getApplicationInstanceCount" => {
            let apps = backend.list_applications();
            JsonRpcResponse {
                id,
                jsonrpc: "2.0".to_string(),
                result: Some(
                    serde_json::to_value(ApplicationInstanceCountResponse { count: apps.len() })
                        .unwrap(),
                ),
                error: None,
            }
        }

        "getApplicationInstance" => {
            match serde_json::from_value::<GetApplicationInstanceParams>(request.params.clone()) {
                Ok(params) => {
                    let apps = backend.list_applications();
                    if let Some(app) = apps.iter().find(|a| a.process_id == params.process_id) {
                        JsonRpcResponse {
                            id,
                            jsonrpc: "2.0".to_string(),
                            result: Some(
                                serde_json::to_value(ApplicationInfoResponse {
                                    process_id: app.process_id,
                                    executable_file: app.executable_file.clone(),
                                    display_name: app.display_name.clone(),
                                    volume: app.volume,
                                    mute: app.mute,
                                    activity: app.activity as u32,
                                })
                                .unwrap(),
                            ),
                            error: None,
                        }
                    } else {
                        JsonRpcResponse {
                            id,
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(JsonRpcError {
                                code: -32000,
                                message: "Application not found".to_string(),
                            }),
                        }
                    }
                }
                Err(e) => JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: format!("Invalid params: {:#}", e),
                    }),
                },
            }
        }

        "getApplicationInstanceAtIndex" => {
            match serde_json::from_value::<GetApplicationInstanceAtIndexParams>(
                request.params.clone(),
            ) {
                Ok(params) => {
                    let apps = backend.list_applications();
                    if let Some(app) = apps.get(params.index) {
                        JsonRpcResponse {
                            id,
                            jsonrpc: "2.0".to_string(),
                            result: Some(
                                serde_json::to_value(ApplicationInfoResponse {
                                    process_id: app.process_id,
                                    executable_file: app.executable_file.clone(),
                                    display_name: app.display_name.clone(),
                                    volume: app.volume,
                                    mute: app.mute,
                                    activity: app.activity as u32,
                                })
                                .unwrap(),
                            ),
                            error: None,
                        }
                    } else {
                        JsonRpcResponse {
                            id,
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(JsonRpcError {
                                code: -32000,
                                message: "Index out of bounds".to_string(),
                            }),
                        }
                    }
                }
                Err(e) => JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: format!("Invalid params: {:#}", e),
                    }),
                },
            }
        }

        "setApplicationInstanceVolume" => {
            match serde_json::from_value::<SetApplicationVolumeParams>(request.params.clone()) {
                Ok(params) => match backend.set_app_volume(params.process_id, params.volume) {
                    Ok(_) => JsonRpcResponse {
                        id,
                        jsonrpc: "2.0".to_string(),
                        result: Some(serde_json::json!({})),
                        error: None,
                    },
                    Err(e) => JsonRpcResponse {
                        id,
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32000,
                            message: format!("Failed to set volume: {:#}", e),
                        }),
                    },
                },
                Err(e) => JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: format!("Invalid params: {:#}", e),
                    }),
                },
            }
        }

        "setApplicationInstanceMute" => {
            match serde_json::from_value::<SetApplicationMuteParams>(request.params.clone()) {
                Ok(params) => match backend.set_app_mute(params.process_id, params.mute) {
                    Ok(_) => JsonRpcResponse {
                        id,
                        jsonrpc: "2.0".to_string(),
                        result: Some(serde_json::json!({})),
                        error: None,
                    },
                    Err(e) => JsonRpcResponse {
                        id,
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32000,
                            message: format!("Failed to set mute: {:#}", e),
                        }),
                    },
                },
                Err(e) => JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: format!("Invalid params: {:#}", e),
                    }),
                },
            }
        }

        "getSystemDefaultDevice" => {
            if let Some(device) = backend.get_system_default_device() {
                JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: Some(
                        serde_json::to_value(DeviceInfoResponse {
                            id: device.id,
                            friendly_name: device.friendly_name,
                            volume: device.volume,
                            mute: device.mute,
                        })
                        .unwrap(),
                    ),
                    error: None,
                }
            } else {
                JsonRpcResponse {
                    id,
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32000,
                        message: "No default device found".to_string(),
                    }),
                }
            }
        }

        _ => {
            log::warn!("Audio Router: Unknown method: {}", request.method);
            JsonRpcResponse {
                id,
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                }),
            }
        }
    }
}
