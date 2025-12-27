use once_cell::sync::OnceCell;

use std::sync::Arc;

/// Host-provided implementation for creating webviews (HTML plugins, PI host window, etc.).
///
/// Core avoids hard-coding a specific windowing framework (Tauri, wry, etc.).
pub trait WebviewHost: Send + Sync + 'static {
    /// Create (or show) a webview for a plugin whose `CodePath` is an HTML entrypoint.
    ///
    /// Implementations are expected to support hidden/background windows.
    fn spawn_plugin_webview(&self, label: &str, url: &str, init_js: &str) -> anyhow::Result<()>;

    /// Best-effort close/destroy a previously created webview.
    fn close_webview(&self, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Best-effort show a previously created webview (useful for developer mode).
    fn show_webview(&self, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Best-effort open devtools for a webview (if supported).
    fn open_devtools(&self, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

static WEBVIEW_HOST: OnceCell<Arc<dyn WebviewHost>> = OnceCell::new();

pub fn init_webview_host(host: Arc<dyn WebviewHost>) {
    let _ = WEBVIEW_HOST.set(host);
}

pub fn webview_host() -> Option<&'static Arc<dyn WebviewHost>> {
    WEBVIEW_HOST.get()
}
