use std::path::{Path, PathBuf};

use log::error;
use tiny_http::{Header, Response, Server};

const RIVERDECK_PROPERTY_INSPECTOR_SUFFIX: &str = "|riverdeck_property_inspector";
const RIVERDECK_PROPERTY_INSPECTOR_CHILD_SUFFIX: &str = "|riverdeck_property_inspector_child";

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
pub async fn init_webserver(prefix: PathBuf) {
    let server = {
        let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{}", *super::PORT_BASE + 2))
        {
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
        let mut url = urlencoding::decode(request.url()).unwrap().into_owned();
        if url.contains('?') {
            url = url.split_once('?').unwrap().0.to_owned();
        }
        #[cfg(target_os = "windows")]
        let url = url[1..].replace('/', "\\");

        // Ensure the requested path is within the config directory to prevent unrestricted access to the filesystem.
        let developer = match crate::store::Store::new(
            "settings",
            &prefix,
            crate::store::Settings::default(),
        ) {
            Ok(store) => store.value.developer,
            Err(_) => false,
        };
        if !developer && !Path::new(&url).starts_with(&prefix) {
            let _ = request.respond(Response::empty(403));
            continue;
        }

        let access_control_allow_origin = Header {
            field: "Access-Control-Allow-Origin".parse().unwrap(),
            value: "*".parse().unwrap(),
        };

        // The Svelte frontend cannot call the connectElgatoStreamDeckSocket function on property inspector frames
        // because they are served from a different origin (this webserver on port 57118).
        // Instead, we have to inject a script onto all property inspector frames that receives a message
        // from the Svelte frontend over window.postMessage.

        // Additionally, Tauri cannot support window.open as seperate Tauri windows have seperate JavaScript contexts.
        // However, plugin property inspectors expect access to this function.
        // Instead, we have to inject a replacement window.open implementation that creates an IFrame element
        // and requests the Svelte frontend to maximise the property inspector.

        if let Some(path) = url.strip_suffix(RIVERDECK_PROPERTY_INSPECTOR_SUFFIX) {
            if !matches!(tokio::fs::try_exists(path).await, Ok(true)) {
                let _ =
                    request.respond(Response::empty(404).with_header(access_control_allow_origin));
                continue;
            }

            let mut content = tokio::fs::read_to_string(path).await.unwrap_or_default();
            content += r#"
				<div id="riverdeck_iframe_container" style="position: absolute; z-index: 100; top: 0; left: 0; width: 100%; height: 100%; display: none;"></div>
				<script>
					const riverdeck_window_open = window.open;
					const riverdeck_iframe_container = document.getElementById("riverdeck_iframe_container");

					window.addEventListener("message", ({ data }) => {
						if (data.event == "connect") {
							event.stopImmediatePropagation();
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
							top.postMessage({ event: "openUrl", payload: url.startsWith("http") ? url : new URL(url, window.location.href).href }, "*");
							return;
						}
						let iframe = document.createElement("iframe");
						iframe.style.flexGrow = "1";
						iframe.onload = () => {
							iframe.contentWindow.opener = window;
							iframe.contentWindow.onbeforeunload = () => top.postMessage({ event: "windowClosed", payload: window.name }, "*");
							iframe.contentWindow.close = () => { iframe.contentWindow.onbeforeunload(); iframe.remove(); };
							iframe.contentWindow.document.body.style.overflowY = "auto";
						};
						iframe.src = url.startsWith("http") ? url : url + "|riverdeck_property_inspector_child";
						if (riverdeck_iframe_container.firstElementChild) riverdeck_iframe_container.firstElementChild.remove();
						riverdeck_iframe_container.appendChild(iframe);
						riverdeck_iframe_container.style.display = "flex";
						top.postMessage({ event: "windowOpened", payload: window.name }, "*");
						return iframe.contentWindow;
					};

					const riverdeck_window_fetch = window.fetch;
					let riverdeck_fetch_count = 0;
					let riverdeck_fetch_promises = {};
					window.addEventListener("message", ({ data }) => {
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
						top.postMessage({ event: "fetch", payload: { args, context: window.name, id: ++riverdeck_fetch_count }}, "*");
						return new Promise((resolve, reject) => { riverdeck_fetch_promises[riverdeck_fetch_count] = { resolve, reject }; });
					};
				</script>
			"#;

            let mut response = Response::from_string(content);
            response.add_header(access_control_allow_origin);
            response.add_header(Header {
                field: "Content-Type".parse().unwrap(),
                value: "text/html".parse().unwrap(),
            });
            let _ = request.respond(response);
        } else if let Some(path) = url.strip_suffix(RIVERDECK_PROPERTY_INSPECTOR_CHILD_SUFFIX) {
            if !matches!(tokio::fs::try_exists(path).await, Ok(true)) {
                let _ =
                    request.respond(Response::empty(404).with_header(access_control_allow_origin));
                continue;
            }

            let mut content = tokio::fs::read_to_string(path).await.unwrap_or_default();
            content = format!("<script>window.opener ??= window.parent;</script>{content}");

            let mut response = Response::from_string(content);
            response.add_header(access_control_allow_origin);
            response.add_header(Header {
                field: "Content-Type".parse().unwrap(),
                value: "text/html".parse().unwrap(),
            });
            let _ = request.respond(response);
        } else {
            if !matches!(tokio::fs::try_exists(&url).await, Ok(true)) {
                let _ =
                    request.respond(Response::empty(404).with_header(access_control_allow_origin));
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
                    Response::from_string(tokio::fs::read_to_string(url).await.unwrap_or_default());
                response.add_header(access_control_allow_origin);
                response.add_header(content_type);
                let _ = request.respond(response);
            } else {
                let mut response = Response::from_file(match tokio::fs::File::open(url).await {
                    Ok(file) => file.into_std().await,
                    Err(_) => continue,
                });
                response.add_header(access_control_allow_origin);
                response.add_header(content_type);
                let _ = request.respond(response);
            }
        }
    }
}
