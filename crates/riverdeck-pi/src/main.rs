use base64::Engine as _;

use serde::{Deserialize, Serialize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::http::Request;
use wry::{WebView, WebViewBuilder};

#[derive(Debug, Clone)]
enum PiEvent {
    Eval(String),
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse(std::env::args().skip(1).collect())?;

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

    let webview = match &args {
        Args::Pi(a) => {
            let html = pi_host_html(a);
            build_pi_webview(&window, html, proxy, a.ipc_port)?
        }
        Args::Url(a) => build_url_webview(&window, &a.url, a.ipc_port)?,
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
                let _ = webview.evaluate_script(&js);
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
) -> anyhow::Result<WebView> {
    let proxy_for_ipc = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_html(html)
        .with_navigation_handler(move |url: String| {
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
        .with_ipc_handler(move |req: Request<String>| handle_ipc(req, proxy_for_ipc.clone()));

    Ok(builder.build(window)?)
}

fn build_url_webview(
    window: &tao::window::Window,
    url: &str,
    ipc_port: Option<u16>,
) -> anyhow::Result<WebView> {
    let builder =
        WebViewBuilder::new()
            .with_url(url)
            .with_navigation_handler(move |url: String| {
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
    Ok(builder.build(window)?)
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
    x: Option<i32>,
    y: Option<i32>,
    w: Option<i32>,
    h: Option<i32>,
    decorations: Option<bool>,
    always_on_top: Option<bool>,
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

fn handle_ipc(req: Request<String>, proxy: EventLoopProxy<PiEvent>) {
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
    }
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
