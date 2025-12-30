use std::path::{Path, PathBuf};

use log::error;
use tiny_http::{Header, Response, Server};

const RIVERDECK_PROPERTY_INSPECTOR_SUFFIX: &str = "|riverdeck_property_inspector";
const RIVERDECK_PROPERTY_INSPECTOR_CHILD_SUFFIX: &str = "|riverdeck_property_inspector_child";

fn is_allowed_origin(origin: &str) -> bool {
    // Keep this intentionally conservative: these servers are local-only, but we still don't want
    // arbitrary websites to read local plugin assets via permissive CORS.
    let o = origin.trim();
    o.starts_with("http://localhost:")
        || o.starts_with("http://127.0.0.1:")
        || o.starts_with("https://localhost:")
        || o.starts_with("https://127.0.0.1:")
        || o == "tauri://localhost"
        || o == "https://tauri.localhost"
}

fn access_control_allow_origin_for(request: &tiny_http::Request) -> Option<Header> {
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str())?;
    if is_allowed_origin(origin) {
        Some(Header {
            field: "Access-Control-Allow-Origin".parse().unwrap(),
            value: origin.parse().unwrap(),
        })
    } else {
        None
    }
}

fn normalize_url_path(mut url: String) -> String {
    // The server receives request paths like `//home/user/...` because callers concatenate
    // `http://localhost:PORT/` + `/abs/path`. Normalize to a single leading slash.
    while url.starts_with("//") {
        url.remove(0);
    }
    url
}

fn mime(extension: &str) -> String {
    match extension {
        "htm" | "html" | "xhtml" => "text/html".to_owned(),
        "js" | "cjs" | "mjs" => "text/javascript".to_owned(),
        "css" => "text/css".to_owned(),
        "png" | "jpeg" | "gif" | "webp" => format!("image/{}", extension),
        "jpg" => "image/jpeg".to_owned(),
        "svg" => "image/svg+xml".to_owned(),
        _ => "application/octet-stream".to_owned(),
    }
}

/// Start a simple webserver to serve files of plugins that run in a browser environment.
pub fn init_webserver(prefix: PathBuf) {
    let server = {
        let listener = match std::net::TcpListener::bind(("127.0.0.1", *super::PORT_BASE + 2)) {
            Ok(l) => l,
            Err(err) => {
                error!("Failed to bind plugin webserver socket: {}", err);
                return;
            }
        };

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

            unsafe { SetHandleInformation(listener.as_raw_socket() as _, HANDLE_FLAG_INHERIT, 0) };
        }

        match Server::from_listener(listener, None) {
            Ok(s) => s,
            Err(err) => {
                error!("Failed to start plugin webserver: {}", err);
                return;
            }
        }
    };

    for request in server.incoming_requests() {
        // Handle CORS preflight early.
        if request.method().as_str() == "OPTIONS" {
            let mut response = Response::empty(204);
            if let Some(acao) = access_control_allow_origin_for(&request) {
                response.add_header(acao);
                response.add_header(Header {
                    field: "Access-Control-Allow-Methods".parse().unwrap(),
                    value: "GET, OPTIONS".parse().unwrap(),
                });
                response.add_header(Header {
                    field: "Access-Control-Allow-Headers".parse().unwrap(),
                    value: "Content-Type".parse().unwrap(),
                });
            }
            let _ = request.respond(response);
            continue;
        }

        let mut url = match urlencoding::decode(request.url()) {
            Ok(u) => normalize_url_path(u.into_owned()),
            Err(_) => {
                let _ = request.respond(Response::empty(400));
                continue;
            }
        };
        if url.contains('?') {
            url = url.split_once('?').unwrap().0.to_owned();
        }
        #[cfg(target_os = "windows")]
        let url = url[1..].replace('/', "\\");

        // Ensure the requested path is within the config directory to prevent unrestricted access to the filesystem.
        let requested = PathBuf::from(&url);
        // Canonicalize *both* sides to avoid `..` tricks and to prevent symlink escapes.
        let prefix_canon = match std::fs::canonicalize(&prefix) {
            Ok(p) => p,
            Err(_) => {
                let _ = request.respond(Response::empty(500));
                continue;
            }
        };
        let requested_canon = match std::fs::canonicalize(&requested) {
            Ok(p) => p,
            Err(_) => {
                // Not found / invalid path.
                let mut resp = Response::empty(404);
                if let Some(acao) = access_control_allow_origin_for(&request) {
                    resp.add_header(acao);
                }
                let _ = request.respond(resp);
                continue;
            }
        };

        if !requested_canon.starts_with(&prefix_canon) {
            let mut resp = Response::empty(403);
            if let Some(acao) = access_control_allow_origin_for(&request) {
                resp.add_header(acao);
            }
            let _ = request.respond(resp);
            continue;
        }

        let access_control_allow_origin = access_control_allow_origin_for(&request);

        // The Svelte frontend cannot call the connectElgatoStreamDeckSocket function on property inspector frames
        // because they are served from a different origin (this webserver on port 57118).
        // Instead, we have to inject a script onto all property inspector frames that receives a message
        // from the Svelte frontend over window.postMessage.

        // Additionally, Tauri cannot support window.open as seperate Tauri windows have seperate JavaScript contexts.
        // However, plugin property inspectors expect access to this function.
        // Instead, we have to inject a replacement window.open implementation that creates an IFrame element
        // and requests the Svelte frontend to maximise the property inspector.

        if let Some(path) = url.strip_suffix(RIVERDECK_PROPERTY_INSPECTOR_SUFFIX) {
            if !Path::new(path).exists() {
                let mut resp = Response::empty(404);
                if let Some(acao) = access_control_allow_origin.clone() {
                    resp.add_header(acao);
                }
                let _ = request.respond(resp);
                continue;
            }

            let mut content = std::fs::read_to_string(path).unwrap_or_default();
            content += r#"
				<div id="riverdeck_iframe_container" style="position: absolute; z-index: 100; top: 0; left: 0; width: 100%; height: 100%; display: none;"></div>
				<script>
					const riverdeck_window_open = window.open;
					const riverdeck_iframe_container = document.getElementById("riverdeck_iframe_container");
					const riverdeck_allowed_origin = (() => { try { return document.referrer ? new URL(document.referrer).origin : null; } catch (_) { return null; } })();
					const riverdeck_token = (() => { try { return new URLSearchParams(window.location.search).get("riverdeck_token"); } catch (_) { return null; } })();

					// RiverDeck extension: best-effort auth for property inspector sockets.
					// We can't change third-party PI code, so we monkey-patch `WebSocket.send` to send a
					// `riverdeckAuth` message immediately after the PI's first send (which is the register event).
					(() => {
						if (window.__riverdeck_ws_patched) return;
						window.__riverdeck_ws_patched = true;
						const NativeWebSocket = window.WebSocket;
						window.WebSocket = function(url, protocols) {
							const ws = protocols ? new NativeWebSocket(url, protocols) : new NativeWebSocket(url);
							const origSend = ws.send.bind(ws);
							let sentAuth = false;
							ws.send = (data) => {
								const res = origSend(data);
								if (!sentAuth && window.__riverdeck_pi_uuid && window.__riverdeck_pi_token) {
									sentAuth = true;
									try {
										origSend(JSON.stringify({
											event: "riverdeckAuth",
											uuid: window.__riverdeck_pi_uuid,
											token: window.__riverdeck_pi_token
										}));
									} catch (_) {}
								}
								return res;
							};
							return ws;
						};
						window.WebSocket.prototype = NativeWebSocket.prototype;
					})();

					window.addEventListener("message", (event) => {
						const { data } = event;
						if (riverdeck_allowed_origin && event.origin !== riverdeck_allowed_origin) return;
						if (data.event == "connect") {
							event.stopImmediatePropagation();
							// Save PI UUID/token for the WebSocket wrapper above.
							try {
								window.__riverdeck_pi_uuid = data.payload && data.payload.length ? data.payload[1] : null;
								window.__riverdeck_pi_token = riverdeck_token;
							} catch (_) {}
							if (typeof connectOpenActionSocket === "function") connectOpenActionSocket(...data.payload);
							else connectElgatoStreamDeckSocket(...data.payload);
						} else if (data.event == "windowClosed") {
							event.stopImmediatePropagation();
							if (riverdeck_iframe_container.firstElementChild) riverdeck_iframe_container.firstElementChild.remove();
							riverdeck_iframe_container.style.display = "none";
						}
					});

					window.open = (url, target) => {
						if (target && !(target == "_self" || target == "_top")) {
							top.postMessage({ event: "openUrl", payload: url.startsWith("http") ? url : new URL(url, window.location.href).href }, riverdeck_allowed_origin ?? "*");
							return;
						}
						let iframe = document.createElement("iframe");
						iframe.style.flexGrow = "1";
						iframe.onload = () => {
							iframe.contentWindow.opener = window;
							iframe.contentWindow.onbeforeunload = () => top.postMessage({ event: "windowClosed", payload: window.name }, riverdeck_allowed_origin ?? "*");
							iframe.contentWindow.close = () => { iframe.contentWindow.onbeforeunload(); iframe.remove(); };
							iframe.contentWindow.document.body.style.overflowY = "auto";
						};
						iframe.src = url.startsWith("http") ? url : url + "|riverdeck_property_inspector_child";
						if (riverdeck_iframe_container.firstElementChild) riverdeck_iframe_container.firstElementChild.remove();
						riverdeck_iframe_container.appendChild(iframe);
						riverdeck_iframe_container.style.display = "flex";
						top.postMessage({ event: "windowOpened", payload: window.name }, riverdeck_allowed_origin ?? "*");
						return iframe.contentWindow;
					};

					const riverdeck_window_fetch = window.fetch;
					let riverdeck_fetch_count = 0;
					let riverdeck_fetch_promises = {};
					window.addEventListener("message", (event) => {
						const { data } = event;
						if (riverdeck_allowed_origin && event.origin !== riverdeck_allowed_origin) return;
						if (data.event == "fetchResponse") {
							event.stopImmediatePropagation();
							const response = new Response(data.payload.response.body, data.payload.response);
							Object.defineProperty(response, "url", { value: data.payload.response.url });
							riverdeck_fetch_promises[data.payload.id].resolve(response);
							delete riverdeck_fetch_promises[data.payload.id];
						} else if (data.event == "fetchError") {
							event.stopImmediatePropagation();
							riverdeck_fetch_promises[data.payload.id].reject(data.payload.error);
							delete riverdeck_fetch_promises[data.payload.id];
						}
					});
					window.fetch = (...args) => {
						if (args.length) args[0] = new URL(args[0], window.location.href).href;
						top.postMessage({ event: "fetch", payload: { args, context: window.name, id: ++riverdeck_fetch_count }}, riverdeck_allowed_origin ?? "*");
						return new Promise((resolve, reject) => { riverdeck_fetch_promises[riverdeck_fetch_count] = { resolve, reject }; });
					};
				</script>
			"#;

            let mut response = Response::from_string(content);
            if let Some(acao) = access_control_allow_origin.clone() {
                response.add_header(acao);
            }
            response.add_header(Header {
                field: "Content-Type".parse().unwrap(),
                value: "text/html".parse().unwrap(),
            });
            let _ = request.respond(response);
        } else if let Some(path) = url.strip_suffix(RIVERDECK_PROPERTY_INSPECTOR_CHILD_SUFFIX) {
            if !Path::new(path).exists() {
                let mut resp = Response::empty(404);
                if let Some(acao) = access_control_allow_origin.clone() {
                    resp.add_header(acao);
                }
                let _ = request.respond(resp);
                continue;
            }

            let mut content = std::fs::read_to_string(path).unwrap_or_default();
            content = format!("<script>window.opener ??= window.parent;</script>{content}");

            let mut response = Response::from_string(content);
            if let Some(acao) = access_control_allow_origin.clone() {
                response.add_header(acao);
            }
            response.add_header(Header {
                field: "Content-Type".parse().unwrap(),
                value: "text/html".parse().unwrap(),
            });
            let _ = request.respond(response);
        } else {
            if !Path::new(&url).exists() {
                let mut resp = Response::empty(404);
                if let Some(acao) = access_control_allow_origin.clone() {
                    resp.add_header(acao);
                }
                let _ = request.respond(resp);
                continue;
            }

            let mime_type = mime(&match Path::new(&url).extension() {
                Some(extension) => extension.to_string_lossy().into_owned(),
                None => "html".to_owned(),
            });

            let content_type = Header {
                field: "Content-Type".parse().unwrap(),
                value: mime_type.parse().unwrap(),
            };

            if mime_type.starts_with("text/") || mime_type == "image/svg+xml" {
                let mut response =
                    Response::from_string(std::fs::read_to_string(url).unwrap_or_default());
                if let Some(acao) = access_control_allow_origin.clone() {
                    response.add_header(acao);
                }
                response.add_header(content_type);
                let _ = request.respond(response);
            } else {
                let mut response = Response::from_file(match std::fs::File::open(url) {
                    Ok(file) => file,
                    Err(_) => continue,
                });
                if let Some(acao) = access_control_allow_origin.clone() {
                    response.add_header(acao);
                }
                response.add_header(content_type);
                let _ = request.respond(response);
            }
        }
    }
}
