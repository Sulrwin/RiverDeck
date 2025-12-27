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

    let html = pi_host_html(&args);

    let event_loop = EventLoopBuilder::<PiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title(&args.label)
        .build(&event_loop)?;

    let webview = build_webview(&window, html, proxy)?;

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

fn build_webview(
    window: &tao::window::Window,
    html: String,
    proxy: EventLoopProxy<PiEvent>,
) -> anyhow::Result<WebView> {
    let proxy_for_ipc = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(move |req: Request<String>| handle_ipc(req, proxy_for_ipc.clone()));

    Ok(builder.build(window)?)
}

#[derive(Debug)]
struct Args {
    label: String,
    pi_src: String,
    origin: String,
    ws_port: u16,
    context: String,
    info_json: String,
    connect_json: String,
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

        let label = take(&mut argv, "--label")?;
        let pi_src = take(&mut argv, "--pi-src")?;
        let origin = take(&mut argv, "--origin")?;
        let ws_port: u16 = take(&mut argv, "--ws-port")?.parse()?;
        let context = take(&mut argv, "--context")?;
        let info_json = take(&mut argv, "--info-json")?;
        let connect_json = take(&mut argv, "--connect-json")?;

        Ok(Self {
            label,
            pi_src,
            origin,
            ws_port,
            context,
            info_json,
            connect_json,
        })
    }
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
            let _ = open::that_detached(url);
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

fn pi_host_html(args: &Args) -> String {
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
    html, body {{ margin:0; padding:0; width:100%; height:100%; overflow:hidden; }}
    #container {{ position:relative; width:100%; height:100%; }}
    #close {{ position:absolute; top:8px; right:8px; z-index:50; display:none; }}
    iframe {{ width:100%; height:100%; border:0; }}
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
