use base64::Engine as _;
use directories::BaseDirs;

use serde::{Deserialize, Serialize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
))]
use wry::WebViewBuilderExtUnix;
use wry::http::Request;
use wry::{WebContext, WebView, WebViewBuilder};

#[derive(Debug, Clone)]
enum PiEvent {
    Eval(String),
}

fn main() -> anyhow::Result<()> {
    // Make helper logs show up even without an explicit `RUST_LOG`.
    // (The parent process may pass a restrictive filter that hides our logs.)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse(std::env::args().skip(1).collect())?;
    log::info!(
        "riverdeck-pi starting (pid={}, args={args:?})",
        std::process::id()
    );

    let storage_dir = args.storage_dir().or_else(default_storage_dir);
    if let Some(dir) = &storage_dir {
        let _ = std::fs::create_dir_all(dir);
        log::info!("webview storage dir: {}", dir.display());
    }

    let event_loop = EventLoopBuilder::<PiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (title, x, y, w, h, decorations, always_on_top) = match &args {
        Args::Pi(a) => (
            a.label.clone(),
            a.x,
            a.y,
            a.w,
            a.h,
            a.decorations,
            a.always_on_top,
        ),
        Args::Url(a) => (
            a.label.clone(),
            a.x,
            a.y,
            a.w,
            a.h,
            a.decorations,
            a.always_on_top,
        ),
    };

    let mut wb = WindowBuilder::new().with_title(&title);
    if let Some(decorations) = decorations {
        wb = wb.with_decorations(decorations);
    }
    if let Some(always_on_top) = always_on_top {
        wb = wb.with_always_on_top(always_on_top);
    }
    if let (Some(w), Some(h)) = (w, h) {
        wb = wb.with_inner_size(tao::dpi::LogicalSize::new(w as f64, h as f64));
    }
    if let (Some(x), Some(y)) = (x, y) {
        wb = wb.with_position(tao::dpi::LogicalPosition::new(x as f64, y as f64));
    }
    let window = wb.build(&event_loop)?;

    let mut web_context = storage_dir.map(|dir| WebContext::new(Some(dir)));

    let webview = match &args {
        Args::Pi(a) => {
            log::info!(
                "PI mode: label={:?} pi_src={:?} origin={:?} ws_port={} context={:?} ipc_port={:?}",
                a.label,
                a.pi_src,
                a.origin,
                a.ws_port,
                a.context,
                a.ipc_port
            );
            let html = pi_host_html(a);
            build_pi_webview(&window, html, proxy, a.ipc_port, web_context.as_mut())?
        }
        Args::Url(a) => {
            log::info!(
                "URL mode: label={:?} url={:?} ipc_port={:?}",
                a.label,
                a.url,
                a.ipc_port
            );
            let wv = build_url_webview(
                &window,
                &a.url,
                proxy.clone(),
                a.ipc_port,
                web_context.as_mut(),
            )?;

            // Best-effort: force a ping immediately so we can verify IPC works even if the
            // page throttles timers or our initialization script doesn't run.
            if a.ipc_port.is_some() {
                let proxy = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    let js = "try{const href=String(location.href||'');if(window.ipc&&window.ipc.postMessage){window.ipc.postMessage(JSON.stringify({type:'marketplacePing',href}));}else{location.href='riverdeck-ipc://marketplacePing?href='+encodeURIComponent(href);}}catch(_){ }";
                    let _ = proxy.send_event(PiEvent::Eval(js.to_owned()));
                });
            }

            wv
        }
    };

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(PiEvent::Eval(js)) => {
                if let Err(err) = webview.evaluate_script(&js) {
                    log::warn!("evaluate_script failed: {err:?}");
                }
            }
            _ => {}
        }
    });
}

fn build_pi_webview(
    window: &tao::window::Window,
    html: String,
    proxy: EventLoopProxy<PiEvent>,
    ipc_port: Option<u16>,
    web_context: Option<&mut WebContext>,
) -> anyhow::Result<WebView> {
    let proxy_for_ipc = proxy.clone();
    let mut builder = if let Some(ctx) = web_context {
        WebViewBuilder::new_with_web_context(ctx)
    } else {
        WebViewBuilder::new()
    };
    builder = builder
        .with_html(html)
        .with_navigation_handler(move |url: String| {
            log::debug!("navigate: {url}");
            // Allow regular browsing, but hand off deep-links to the OS.
            // This is important for things like Elgato Marketplace "Open in Stream Deck" links.
            if url.starts_with("openaction://")
                || url.starts_with("streamdeck://")
                || url.starts_with("riverdeck://")
            {
                if let Some(port) = ipc_port {
                    let _ = send_deep_link_ipc(port, &url);
                } else {
                    let _ = open::that_detached(url);
                }
                return false;
            }
            true
        })
        .with_ipc_handler(move |req: Request<String>| {
            handle_ipc(req, proxy_for_ipc.clone(), ipc_port)
        });

    // On Unix/GTK (including Wayland), prefer `build_gtk` for better compatibility.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    {
        use tao::platform::unix::WindowExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| anyhow::anyhow!("tao default_vbox unavailable for build_gtk"))?;
        Ok(builder.build_gtk(vbox)?)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    )))]
    {
        Ok(builder.build(window)?)
    }
}

fn build_url_webview(
    window: &tao::window::Window,
    url: &str,
    proxy: EventLoopProxy<PiEvent>,
    ipc_port: Option<u16>,
    web_context: Option<&mut WebContext>,
) -> anyhow::Result<WebView> {
    let mut builder = if let Some(ctx) = web_context {
        WebViewBuilder::new_with_web_context(ctx)
    } else {
        WebViewBuilder::new()
    };
    let ipc_port_for_nav = ipc_port;
    builder = builder
        .with_url(url)
        .with_ipc_handler({
            let proxy = proxy.clone();
            move |req: Request<String>| handle_ipc(req, proxy.clone(), ipc_port)
        })
        .with_initialization_script(marketplace_token_capture_script())
        .with_navigation_handler(move |url: String| {
            log::debug!("navigate: {url}");
            if url.starts_with("riverdeck-ipc://") {
                if let Some(port) = ipc_port_for_nav {
                    let _ = handle_riverdeck_ipc_url(port, &url);
                }
                return false;
            }
            if url.starts_with("openaction://")
                || url.starts_with("streamdeck://")
                || url.starts_with("riverdeck://")
            {
                if let Some(port) = ipc_port {
                    let _ = send_deep_link_ipc(port, &url);
                } else {
                    let _ = open::that_detached(url);
                }
                return false;
            }
            true
        });
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    {
        use tao::platform::unix::WindowExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| anyhow::anyhow!("tao default_vbox unavailable for build_gtk"))?;
        Ok(builder.build_gtk(vbox)?)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    )))]
    {
        Ok(builder.build(window)?)
    }
}

fn handle_riverdeck_ipc_url(ipc_port: u16, url: &str) -> std::io::Result<()> {
    let Ok(u) = reqwest::Url::parse(url) else {
        return Ok(());
    };
    let host = u.host_str().unwrap_or_default();
    match host {
        "marketplacePing" => {
            let href = u
                .query_pairs()
                .find(|(k, _)| k == "href")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            if !href.trim().is_empty() {
                log::info!("marketplace ping received via nav fallback (href={href})");
                let _ = send_marketplace_ping_ipc(ipc_port, &href);
            }
        }
        "marketplaceToken" => {
            let token = u
                .query_pairs()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            if !token.trim().is_empty() {
                log::info!(
                    "marketplace token received via nav fallback (len={})",
                    token.trim().len()
                );
                let _ = send_marketplace_token_ipc(ipc_port, &token);
            }
        }
        _ => {}
    }
    Ok(())
}

fn marketplace_token_capture_script() -> &'static str {
    r#"
(() => {
  // Best-effort: capture Marketplace access token when running on marketplace.elgato.com.
  // The token is used by the host to resolve `.../install/<variant_id>` deep links into signed
  // download URLs. We intentionally keep this minimal and defensive.
  let last = null;

  function send(tok) {
    try {
      if (!tok || typeof tok !== "string") return;
      if (tok.length <= 10 || tok.length >= 16000) return;
      if (tok === last) return;
      last = tok;
      if (window.ipc && window.ipc.postMessage) {
        window.ipc.postMessage(JSON.stringify({ type: "marketplaceToken", token: tok }));
      } else {
        // Fallback: trigger a navigation to a custom scheme that the host intercepts.
        location.href = "riverdeck-ipc://marketplaceToken?token=" + encodeURIComponent(tok);
      }
    } catch (_) {}
  }

  // Strategy A: sniff bearer tokens from Marketplace's own fetch/XHR calls (most reliable).
  try {
    const _fetch = window.fetch;
    window.fetch = async (...args) => {
      try {
        // Only accept tokens used against mp-gateway, otherwise we may capture unrelated OAuth tokens.
        let urlStr = "";
        try {
          const u = args[0];
          if (typeof u === "string") urlStr = u;
          else if (u && typeof u.url === "string") urlStr = u.url;
        } catch (_) {}
        const isMpGateway = urlStr.includes("mp-gateway.elgato.com");

        const init = args[1] || {};
        const h = init.headers;
        const getAuth = (headers) => {
          try {
            if (!headers) return null;
            if (headers instanceof Headers) return headers.get("authorization") || headers.get("Authorization");
            if (Array.isArray(headers)) {
              for (const [k, v] of headers) if (String(k).toLowerCase() === "authorization") return v;
              return null;
            }
            if (typeof headers === "object") {
              for (const k of Object.keys(headers)) if (k.toLowerCase() === "authorization") return headers[k];
            }
          } catch (_) {}
          return null;
        };
        const auth = getAuth(h);
        if (isMpGateway && auth && typeof auth === "string" && auth.toLowerCase().startsWith("bearer ")) {
          send(auth.slice(7).trim());
        }
      } catch (_) {}
      return _fetch(...args);
    };

    const _open = XMLHttpRequest.prototype.open;
    const _set = XMLHttpRequest.prototype.setRequestHeader;
    XMLHttpRequest.prototype.open = function(...args) {
      // args: method, url, ...
      try { this.__riverdeck_url = String(args[1] || ""); } catch (_) {}
      return _open.apply(this, args);
    };
    XMLHttpRequest.prototype.setRequestHeader = function(k, v) {
      try {
        const isMpGateway = (this && this.__riverdeck_url && String(this.__riverdeck_url).includes("mp-gateway.elgato.com"));
        if (isMpGateway && String(k).toLowerCase() === "authorization" && typeof v === "string" && v.toLowerCase().startsWith("bearer ")) {
          send(v.slice(7).trim());
        }
      } catch (_) {}
      return _set.apply(this, arguments);
    };
  } catch (_) {}

  // Strategy B: poll the NextAuth session endpoint (works on some deployments).
  async function poll() {
    try {
      const host = (location && location.host) ? String(location.host) : "";
      if (!host.endsWith("marketplace.elgato.com")) {
        setTimeout(poll, 5000);
        return;
      }

      // Debug: prove IPC works even before we have a token.
      try {
        const href = String(location.href || "");
        if (window.ipc && window.ipc.postMessage) {
          window.ipc.postMessage(JSON.stringify({ type: "marketplacePing", href }));
        } else {
          location.href = "riverdeck-ipc://marketplacePing?href=" + encodeURIComponent(href);
        }
      } catch (_) {}

      // NextAuth session endpoint (used by the Marketplace frontend).
      const resp = await fetch("/api/auth/session", { credentials: "include" });
      if (resp && resp.ok) {
        const j = await resp.json();
        const tok =
          (j && (j.accessToken || j.access_token)) ||
          (j && j.user && (j.user.accessToken || j.user.access_token)) ||
          (j && j.token) ||
          null;
        send(tok);
      }
    } catch (_) {}
    setTimeout(poll, 5000);
  }
  poll();
})();
"#
}

fn send_deep_link_ipc(port: u16, url: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid ipc port"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250))?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));

    // Ask the existing window to show for UX, then forward the deep link.
    let _ = writeln!(stream, "{{\"type\":\"show\"}}");
    writeln!(
        stream,
        "{}",
        serde_json::json!({ "type": "deep_link", "url": url })
    )?;
    Ok(())
}

#[derive(Debug)]
enum Args {
    Pi(PiArgs),
    Url(UrlArgs),
}

#[derive(Debug)]
struct PiArgs {
    label: String,
    pi_src: String,
    origin: String,
    ws_port: u16,
    context: String,
    info_json: String,
    connect_json: String,
    ipc_port: Option<u16>,
    storage_dir: Option<std::path::PathBuf>,
    x: Option<i32>,
    y: Option<i32>,
    w: Option<i32>,
    h: Option<i32>,
    decorations: Option<bool>,
    always_on_top: Option<bool>,
}

impl Args {
    fn parse(mut argv: Vec<String>) -> anyhow::Result<Self> {
        fn take(argv: &mut Vec<String>, key: &str) -> anyhow::Result<String> {
            let idx = argv
                .iter()
                .position(|v| v == key)
                .ok_or_else(|| anyhow::anyhow!("missing arg {key}"))?;
            if idx + 1 >= argv.len() {
                return Err(anyhow::anyhow!("missing value for {key}"));
            }
            let value = argv.remove(idx + 1);
            argv.remove(idx);
            Ok(value)
        }

        // Optional docking geometry.
        fn take_opt(argv: &mut Vec<String>, key: &str) -> Option<String> {
            let idx = argv.iter().position(|v| v == key)?;
            if idx + 1 >= argv.len() {
                return None;
            }
            let value = argv.remove(idx + 1);
            argv.remove(idx);
            Some(value)
        }
        let x = take_opt(&mut argv, "--x").and_then(|v| v.parse::<i32>().ok());
        let y = take_opt(&mut argv, "--y").and_then(|v| v.parse::<i32>().ok());
        let w = take_opt(&mut argv, "--w").and_then(|v| v.parse::<i32>().ok());
        let h = take_opt(&mut argv, "--h").and_then(|v| v.parse::<i32>().ok());
        let ipc_port = take_opt(&mut argv, "--ipc-port").and_then(|v| v.parse::<u16>().ok());
        let storage_dir = take_opt(&mut argv, "--storage-dir").map(std::path::PathBuf::from);
        let decorations = take_opt(&mut argv, "--decorations").and_then(|v| match v.as_str() {
            "1" | "true" | "True" | "TRUE" => Some(true),
            "0" | "false" | "False" | "FALSE" => Some(false),
            _ => None,
        });
        let always_on_top = take_opt(&mut argv, "--always-on-top").and_then(|v| match v.as_str() {
            "1" | "true" | "True" | "TRUE" => Some(true),
            "0" | "false" | "False" | "FALSE" => Some(false),
            _ => None,
        });

        // URL/web mode (used for marketplace, docs, etc.).
        if argv.iter().any(|v| v == "--url") {
            let label = take(&mut argv, "--label")?;
            let url = take(&mut argv, "--url")?;
            return Ok(Args::Url(UrlArgs {
                label,
                url,
                ipc_port,
                storage_dir,
                x,
                y,
                w,
                h,
                decorations,
                always_on_top,
            }));
        }

        // PI mode (existing behavior).
        let label = take(&mut argv, "--label")?;
        let pi_src = take(&mut argv, "--pi-src")?;
        let origin = take(&mut argv, "--origin")?;
        let ws_port: u16 = take(&mut argv, "--ws-port")?.parse()?;
        let context = take(&mut argv, "--context")?;
        let info_json = take(&mut argv, "--info-json")?;
        let connect_json = take(&mut argv, "--connect-json")?;

        Ok(Args::Pi(PiArgs {
            label,
            pi_src,
            origin,
            ws_port,
            context,
            info_json,
            connect_json,
            ipc_port,
            storage_dir,
            x,
            y,
            w,
            h,
            decorations,
            always_on_top,
        }))
    }
}

#[derive(Debug)]
struct UrlArgs {
    label: String,
    url: String,
    ipc_port: Option<u16>,
    storage_dir: Option<std::path::PathBuf>,
    x: Option<i32>,
    y: Option<i32>,
    w: Option<i32>,
    h: Option<i32>,
    decorations: Option<bool>,
    always_on_top: Option<bool>,
}

impl Args {
    fn storage_dir(&self) -> Option<std::path::PathBuf> {
        match self {
            Args::Pi(a) => a.storage_dir.clone(),
            Args::Url(a) => a.storage_dir.clone(),
        }
    }
}

fn default_storage_dir() -> Option<std::path::PathBuf> {
    // Stable on-disk location so cookies/session state survive restarts.
    let base = BaseDirs::new()?;
    let app_id = "io.github.sulrwin.riverdeck";
    Some(base.data_dir().join(app_id).join("webview").join("default"))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum IpcRequest {
    #[serde(rename = "openUrl")]
    OpenUrl { url: String },
    #[serde(rename = "fetch")]
    Fetch {
        id: u64,
        context: String,
        url: String,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
        body_base64: Option<String>,
    },
    #[serde(rename = "marketplaceToken")]
    MarketplaceToken { token: String },
    #[serde(rename = "marketplacePing")]
    MarketplacePing { href: String },
}

#[derive(Debug, Serialize)]
struct FetchResponsePayload<'a> {
    id: u64,
    context: &'a str,
    url: &'a str,
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body_base64: String,
}

fn handle_ipc(req: Request<String>, proxy: EventLoopProxy<PiEvent>, ipc_port: Option<u16>) {
    let msg = req.body().clone();
    let Ok(req) = serde_json::from_str::<IpcRequest>(&msg) else {
        return;
    };
    match req {
        IpcRequest::OpenUrl { url } => {
            let u = url.trim();
            if u.len() <= 2048 && (u.starts_with("http://") || u.starts_with("https://")) {
                let _ = open::that_detached(u);
            }
        }
        IpcRequest::Fetch {
            id,
            context,
            url,
            method,
            headers,
            body_base64,
        } => {
            std::thread::spawn(move || {
                let url_trim = url.trim();
                if !(url_trim.starts_with("http://") || url_trim.starts_with("https://")) {
                    let js = format!(
                        "window.postMessage({{event:'riverdeckFetchResponse',payload:{}}}, '*');",
                        serde_json::to_string(&FetchResponsePayload {
                            id,
                            context: &context,
                            url: &url,
                            status: 400,
                            status_text: "blocked url scheme".to_owned(),
                            headers: vec![],
                            body_base64: String::new(),
                        })
                        .unwrap_or_else(|_| "null".to_owned())
                    );
                    let _ = proxy.send_event(PiEvent::Eval(js));
                    return;
                }

                let client = reqwest::blocking::Client::new();
                let mut rb = client.request(
                    method
                        .unwrap_or_else(|| "GET".to_owned())
                        .parse()
                        .unwrap_or(reqwest::Method::GET),
                    &url,
                );
                if let Some(headers) = headers {
                    for (k, v) in headers {
                        rb = rb.header(k, v);
                    }
                }
                if let Some(body) = body_base64
                    .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                {
                    // Avoid huge request bodies from untrusted PIs.
                    if body.len() > (2 * 1024 * 1024) {
                        return;
                    }
                    rb = rb.body(body);
                }

                let payload = match rb.send() {
                    Ok(resp) => {
                        let status = resp.status();
                        let status_text = status.canonical_reason().unwrap_or("").to_owned();
                        let headers = resp
                            .headers()
                            .iter()
                            .map(|(k, v)| {
                                (k.to_string(), v.to_str().unwrap_or_default().to_owned())
                            })
                            .collect::<Vec<_>>();
                        let bytes = resp.bytes().unwrap_or_default();
                        // Cap response size to avoid base64-bombing the host process.
                        let bytes = if bytes.len() > (5 * 1024 * 1024) {
                            bytes.slice(..(5 * 1024 * 1024))
                        } else {
                            bytes
                        };
                        FetchResponsePayload {
                            id,
                            context: &context,
                            url: &url,
                            status: status.as_u16(),
                            status_text,
                            headers,
                            body_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        }
                    }
                    Err(err) => FetchResponsePayload {
                        id,
                        context: &context,
                        url: &url,
                        status: 599,
                        status_text: err.to_string(),
                        headers: vec![],
                        body_base64: String::new(),
                    },
                };

                let js = format!(
                    "window.postMessage({{event:'riverdeckFetchResponse',payload:{}}}, '*');",
                    serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_owned())
                );
                let _ = proxy.send_event(PiEvent::Eval(js));
            });
        }
        IpcRequest::MarketplaceToken { token } => {
            log::info!(
                "marketplace token received in webview helper (len={}, ipc_port={:?})",
                token.trim().len(),
                ipc_port
            );
            if let Some(port) = ipc_port {
                let _ = send_marketplace_token_ipc(port, &token);
            }
        }
        IpcRequest::MarketplacePing { href } => {
            log::info!(
                "marketplace ping received in webview helper (href={}, ipc_port={:?})",
                href,
                ipc_port
            );
            if let Some(port) = ipc_port {
                let _ = send_marketplace_ping_ipc(port, &href);
            }
        }
    }
}

fn send_marketplace_token_ipc(port: u16, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let tok = token.trim();
    if tok.is_empty() || tok.len() > 16 * 1024 {
        return Ok(());
    }

    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid ipc port"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250))?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    writeln!(
        stream,
        "{}",
        serde_json::json!({ "type": "marketplace_token", "token": tok })
    )?;
    Ok(())
}

fn send_marketplace_ping_ipc(port: u16, href: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let href = href.trim();
    if href.is_empty() || href.len() > 4096 {
        return Ok(());
    }

    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid ipc port"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250))?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    writeln!(
        stream,
        "{}",
        serde_json::json!({ "type": "marketplace_ping", "href": href })
    )?;
    Ok(())
}

fn pi_host_html(args: &PiArgs) -> String {
    let origin_json = serde_json::to_string(&args.origin).unwrap_or_else(|_| "\"*\"".to_owned());
    let context_json = serde_json::to_string(&args.context).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Property Inspector</title>
  <style>
    html, body {{
      margin:0;
      padding:0;
      width:100%;
      height:100%;
      overflow:hidden;
      background:#1f2022;
      color:#e6e6e6;
      font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    }}
    #container {{
      position:relative;
      width:100%;
      height:100%;
      padding:10px;
      box-sizing:border-box;
    }}
    #close {{
      position:absolute;
      top:14px;
      right:14px;
      z-index:50;
      display:none;
      width:30px;
      height:30px;
      border-radius:10px;
      border:1px solid rgba(255,255,255,0.14);
      background: rgba(255,255,255,0.06);
      color:#f0f0f0;
      cursor:pointer;
      font-size:16px;
      line-height:28px;
      text-align:center;
    }}
    #close:hover {{
      background: rgba(255,255,255,0.10);
      border-color: rgba(255,255,255,0.20);
    }}
    #close:active {{
      background: rgba(255,255,255,0.14);
    }}
    iframe {{
      width:100%;
      height:100%;
      border:0;
      border-radius:12px;
      background:#2b2c2f;
    }}
  </style>
</head>
<body>
  <div id="container">
    <button id="close">✕</button>
    <iframe id="pi" name="{context}" src="{pi_src}"></iframe>
  </div>
  <script>
    const origin = {origin_json};
    const wsPort = {ws_port};
    const context = {context_json};
    const info = {info_json};
    const connectPayload = {connect_json};

    const iframe = document.getElementById('pi');
    const closeBtn = document.getElementById('close');

    iframe.addEventListener('load', () => {{
      iframe.contentWindow?.postMessage({{
        event: 'connect',
        payload: [wsPort, context, 'registerPropertyInspector', info, JSON.stringify(connectPayload)]
      }}, origin);
    }});

    closeBtn.addEventListener('click', () => {{
      iframe.contentWindow?.postMessage({{ event: 'windowClosed' }}, origin);
      closeBtn.style.display = 'none';
    }});

    window.addEventListener('message', (ev) => {{
      const data = ev.data;
      if (!data || typeof data !== 'object') return;

      if (data.event === 'windowOpened') {{
        closeBtn.style.display = 'block';
      }} else if (data.event === 'windowClosed') {{
        closeBtn.style.display = 'none';
      }} else if (data.event === 'openUrl') {{
        window.ipc?.postMessage(JSON.stringify({{ type: 'openUrl', url: data.payload }}));
      }} else if (data.event === 'fetch') {{
        const args = data.payload.args || [];
        const url = args[0];
        const opts = args[1] || {{}};
        const headersArr = [];
        if (opts.headers) {{
          try {{
            for (const [k, v] of new Headers(opts.headers).entries()) headersArr.push([k, v]);
          }} catch {{}}
        }}
        let bodyBase64 = null;
        if (opts.body instanceof Uint8Array) {{
          bodyBase64 = btoa(String.fromCharCode(...opts.body));
        }} else if (typeof opts.body === 'string') {{
          bodyBase64 = btoa(unescape(encodeURIComponent(opts.body)));
        }}
        window.ipc?.postMessage(JSON.stringify({{
          type: 'fetch',
          id: data.payload.id,
          context: data.payload.context,
          url,
          method: opts.method,
          headers: headersArr,
          body_base64: bodyBase64
        }}));
      }} else if (data.event === 'riverdeckFetchResponse') {{
        const p = data.payload;
        const bin = p.body_base64 ? Uint8Array.from(atob(p.body_base64), c => c.charCodeAt(0)) : new Uint8Array();
        iframe.contentWindow?.postMessage({{
          event: 'fetchResponse',
          payload: {{
            id: p.id,
            response: {{
              url: p.url,
              body: bin,
              headers: p.headers,
              status: p.status,
              statusText: p.status_text
            }}
          }}
        }}, origin);
      }}
    }});
  </script>
</body>
</html>
"#,
        context = args.context,
        pi_src = args.pi_src,
        ws_port = args.ws_port,
        origin_json = origin_json,
        context_json = context_json,
        info_json = args.info_json,
        connect_json = args.connect_json
    )
}
