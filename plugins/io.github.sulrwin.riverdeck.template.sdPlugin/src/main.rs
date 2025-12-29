use openaction::*;

struct GlobalHandler;
impl GlobalEventHandler for GlobalHandler {}

struct ActionHandler;
impl ActionEventHandler for ActionHandler {
    async fn will_appear(
        &self,
        event: AppearanceEvent,
        outbound: &mut OutboundEventManager,
    ) -> EventHandlerResult {
        // A simple “hello” title so you can verify the plugin runs.
        outbound
            .set_title(&event.context, Some("Template"), None)
            .await?;
        Ok(())
    }

    async fn key_up(
        &self,
        event: KeyEvent,
        outbound: &mut OutboundEventManager,
    ) -> EventHandlerResult {
        // Toggle the title each press as a proof-of-life.
        let title = if event.payload.state == 0 {
            "Pressed"
        } else {
            "Template"
        };
        outbound
            .set_title(&event.context, Some(title), None)
            .await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Minimal logger to stdout/stderr (captured by RiverDeck plugin logs).
    simplelog::TermLogger::init(
        log::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .ok();

    run_plugin(GlobalHandler, ActionHandler).await?;
    Ok(())
}


