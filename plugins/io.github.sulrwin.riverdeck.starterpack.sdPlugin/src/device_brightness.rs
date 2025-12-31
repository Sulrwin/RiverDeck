use openaction::*;

// Non-spec RiverDeck-specific protocols are used in this file.

#[derive(serde::Serialize)]
struct DeviceBrightnessEvent {
	event: &'static str,
	action: String,
	value: u8,
}

fn get_str<'a>(obj: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> Option<&'a str> {
	obj.get(key).and_then(|v| v.as_str()).map(|s| s.trim())
}

fn get_u8(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u8> {
	obj.get(key).and_then(|v| v.as_u64()).map(|v| v as u8)
}

fn normalize_action(s: &str) -> Option<&'static str> {
	match s.trim() {
		"" => None,
		"none" => None,
		"set" => Some("set"),
		"increase" => Some("increase"),
		"decrease" => Some("decrease"),
		_ => None,
	}
}

pub async fn up(
	event: impl crate::ActionEvent,
	outbound: &mut OutboundEventManager,
) -> EventHandlerResult {
	let Some(settings) = event.settings().as_object() else {
		return Ok(());
	};

	// New mapping (Action Editor schema). Fall back to legacy `action`/`value`.
	let press_action = get_str(settings, "pressAction")
		.and_then(normalize_action)
		.or_else(|| get_str(settings, "action").and_then(normalize_action))
		.unwrap_or("set");

	let set_value = get_u8(settings, "setValue")
		.or_else(|| get_u8(settings, "value"))
		.unwrap_or(50)
		.clamp(0, 100);

	let step = get_u8(settings, "step").unwrap_or(1).clamp(1, 100);
	let value = match press_action {
		"set" => set_value,
		"increase" | "decrease" => step,
		_ => return Ok(()),
	};

	outbound
		.send_event(DeviceBrightnessEvent {
			event: "deviceBrightness",
			action: press_action.to_owned(),
			value,
		})
		.await?;

	Ok(())
}

pub async fn rotate(
	event: DialRotateEvent,
	outbound: &mut OutboundEventManager,
) -> EventHandlerResult {
	let Some(settings) = event.payload.settings.as_object() else {
		// Back-compat: old encoder behavior even without settings.
		let action = if event.payload.ticks < 0 {
			"decrease".to_owned()
		} else {
			"increase".to_owned()
		};
		outbound
			.send_event(DeviceBrightnessEvent {
				event: "deviceBrightness",
				action,
				value: event.payload.ticks.unsigned_abs().min(100) as u8,
			})
			.await?;
		return Ok(());
	};

	let ticks = event.payload.ticks;
	if ticks == 0 {
		return Ok(());
	}

	let set_value = get_u8(settings, "setValue")
		.or_else(|| get_u8(settings, "value"))
		.unwrap_or(50)
		.clamp(0, 100);

	let step = get_u8(settings, "step").unwrap_or(1).clamp(1, 100) as u16;

	let cw_action = get_str(settings, "clockwiseAction")
		.and_then(normalize_action)
		.unwrap_or("increase");
	let ccw_action = get_str(settings, "anticlockwiseAction")
		.and_then(normalize_action)
		.unwrap_or("decrease");

	let action = if ticks < 0 { ccw_action } else { cw_action };
	let action = match action {
		"set" | "increase" | "decrease" => action,
		_ => return Ok(()),
	};

	let value = match action {
		"set" => set_value,
		"increase" | "decrease" => {
			let magnitude = ticks.unsigned_abs().saturating_mul(step);
			magnitude.min(100) as u8
		}
		_ => return Ok(()),
	};

	outbound
		.send_event(DeviceBrightnessEvent {
			event: "deviceBrightness",
			action: action.to_owned(),
			value,
		})
		.await?;

	Ok(())
}
