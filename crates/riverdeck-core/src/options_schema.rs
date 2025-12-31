//! Host-side options schema for rendering native Action Editor settings UIs.
//!
//! This is intentionally host-owned (not plugin-owned) so we can progressively add coverage for
//! third-party plugins without requiring manifest changes.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct OptionsSchema {
    pub title: &'static str,
    pub fields: &'static [Field],
}

#[derive(Debug, Clone, Copy)]
pub enum DefaultValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(&'static str),
}

impl DefaultValue {
    pub fn to_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(v) => Value::Bool(v),
            Self::Int(v) => Value::Number(v.into()),
            Self::Float(v) => serde_json::Number::from_f64(v)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Self::Text(v) => Value::String(v.to_owned()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum FieldKind {
    Bool,
    Int,
    Float,
    Text,
    Enum(&'static [EnumOption]),
    PathFile,
    PathDir,
}

#[derive(Debug, Clone)]
pub struct EnumOption {
    pub label: &'static str,
    pub value: &'static str,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub label: &'static str,
    /// Dot-separated path into the instance `settings` object.
    /// Example: `"runOn"` or `"device.id"`.
    pub key_path: &'static str,
    pub help: Option<&'static str>,
    pub kind: FieldKind,
    pub default: DefaultValue,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Best-effort lookup of a host-defined schema for an action UUID.
pub fn get_schema(action_uuid: &str) -> Option<&'static OptionsSchema> {
    let u = action_uuid.trim();

    // OpenAction Counter plugin (me.amankhanna.oacounter)
    // PI only exposes "step" while preserving "value" (managed by the plugin). We mirror that:
    // allow editing step, but leave value untouched.
    static OA_COUNTER_FIELDS: &[Field] = &[Field {
        label: "Step",
        key_path: "step",
        help: Some("Amount to add/subtract per press/turn."),
        kind: FieldKind::Int,
        default: DefaultValue::Int(1),
        min: Some(1.0),
        max: None,
    }];
    static OA_COUNTER_TEMP_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Temporary Counter",
        fields: OA_COUNTER_FIELDS,
    };
    static OA_COUNTER_PERSIST_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Persisted Counter",
        fields: OA_COUNTER_FIELDS,
    };

    // Starterpack: Run Command
    static RUN_COMMAND_FIELDS: &[Field] = &[
        Field {
            label: "Key down",
            key_path: "down",
            help: None,
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Key up",
            key_path: "up",
            help: None,
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Dial rotate (encoder)",
            key_path: "rotate",
            help: Some("Use %d for ticks; negative is counterclockwise, positive is clockwise."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Write to path",
            key_path: "file",
            help: None,
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Show on key",
            key_path: "show",
            help: None,
            kind: FieldKind::Bool,
            default: DefaultValue::Bool(false),
            min: None,
            max: None,
        },
    ];
    static RUN_COMMAND_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Run Command",
        fields: RUN_COMMAND_FIELDS,
    };

    // Starterpack: Input Simulation
    static INPUT_SIM_FIELDS: &[Field] = &[
        Field {
            label: "Key down",
            key_path: "down",
            help: Some("Input to be executed on key down."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Key up",
            key_path: "up",
            help: Some("Input to be executed on key up."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Dial rotate anticlockwise (encoder)",
            key_path: "anticlockwise",
            help: Some("Input to be executed on dial rotate anticlockwise."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Dial rotate clockwise (encoder)",
            key_path: "clockwise",
            help: Some("Input to be executed on dial rotate clockwise."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
    ];
    static INPUT_SIM_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Input Simulation",
        fields: INPUT_SIM_FIELDS,
    };

    // Starterpack: Switch Profile
    static SWITCH_PROFILE_FIELDS: &[Field] = &[
        Field {
            label: "Device ID",
            key_path: "device",
            help: Some("Defaults to the current device if empty."),
            kind: FieldKind::Text,
            default: DefaultValue::Text(""),
            min: None,
            max: None,
        },
        Field {
            label: "Profile",
            key_path: "profile",
            help: None,
            kind: FieldKind::Text,
            default: DefaultValue::Text("Default"),
            min: None,
            max: None,
        },
    ];
    static SWITCH_PROFILE_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Switch Profile",
        fields: SWITCH_PROFILE_FIELDS,
    };

    // Starterpack: Device Brightness
    static BRIGHTNESS_ACTION_OPTS: &[EnumOption] = &[
        EnumOption {
            label: "None",
            value: "none",
        },
        EnumOption {
            label: "Set",
            value: "set",
        },
        EnumOption {
            label: "Increase",
            value: "increase",
        },
        EnumOption {
            label: "Decrease",
            value: "decrease",
        },
    ];
    static DEVICE_BRIGHTNESS_FIELDS: &[Field] = &[
        Field {
            label: "Press action",
            key_path: "pressAction",
            help: Some("What happens when you press the key / dial."),
            kind: FieldKind::Enum(BRIGHTNESS_ACTION_OPTS),
            default: DefaultValue::Text("set"),
            min: None,
            max: None,
        },
        Field {
            label: "Dial clockwise action",
            key_path: "clockwiseAction",
            help: Some("What happens when you rotate the dial clockwise (right)."),
            kind: FieldKind::Enum(BRIGHTNESS_ACTION_OPTS),
            default: DefaultValue::Text("increase"),
            min: None,
            max: None,
        },
        Field {
            label: "Dial anticlockwise action",
            key_path: "anticlockwiseAction",
            help: Some("What happens when you rotate the dial anticlockwise (left)."),
            kind: FieldKind::Enum(BRIGHTNESS_ACTION_OPTS),
            default: DefaultValue::Text("decrease"),
            min: None,
            max: None,
        },
        Field {
            label: "Set value (%)",
            key_path: "setValue",
            help: Some("Only used when the selected action is Set."),
            kind: FieldKind::Int,
            default: DefaultValue::Int(50),
            min: Some(0.0),
            max: Some(100.0),
        },
        Field {
            label: "Step (%)",
            key_path: "step",
            help: Some("Only used when the selected action is Increase/Decrease."),
            kind: FieldKind::Int,
            default: DefaultValue::Int(1),
            min: Some(1.0),
            max: Some(100.0),
        },
    ];
    static DEVICE_BRIGHTNESS_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Device Brightness",
        fields: DEVICE_BRIGHTNESS_FIELDS,
    };

    // Template plugin: placeholder (no settings defined yet).
    static TEMPLATE_FIELDS: &[Field] = &[];
    static TEMPLATE_SCHEMA: OptionsSchema = OptionsSchema {
        title: "Template Action",
        fields: TEMPLATE_FIELDS,
    };

    match u {
        "me.amankhanna.oacounter.temporary" => Some(&OA_COUNTER_TEMP_SCHEMA),
        "me.amankhanna.oacounter.persisted" => Some(&OA_COUNTER_PERSIST_SCHEMA),
        "io.github.sulrwin.riverdeck.starterpack.runcommand" => Some(&RUN_COMMAND_SCHEMA),
        "io.github.sulrwin.riverdeck.starterpack.inputsimulation" => Some(&INPUT_SIM_SCHEMA),
        "io.github.sulrwin.riverdeck.starterpack.switchprofile" => Some(&SWITCH_PROFILE_SCHEMA),
        "io.github.sulrwin.riverdeck.starterpack.devicebrightness" => {
            Some(&DEVICE_BRIGHTNESS_SCHEMA)
        }
        "io.github.sulrwin.riverdeck.template.action" => Some(&TEMPLATE_SCHEMA),
        _ => None,
    }
}

/// Get a setting by dot-path. Returns `None` if the path doesn't exist.
pub fn get_by_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    if path.is_empty() {
        return Some(root);
    }
    let mut cur = root;
    for seg in path.split('.') {
        let seg = seg.trim();
        if seg.is_empty() {
            return None;
        }
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Set a setting by dot-path, creating intermediate objects as needed.
///
/// Returns `Err` if an intermediate path component exists but isn't an object.
pub fn set_by_path(root: &mut Value, path: &str, value: Value) -> Result<(), &'static str> {
    let path = path.trim();
    if path.is_empty() {
        *root = value;
        return Ok(());
    }
    let segments: Vec<&str> = path
        .split('.')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        *root = value;
        return Ok(());
    }
    if !root.is_object() {
        *root = Value::Object(serde_json::Map::new());
    }
    let mut cur = root;
    for seg in &segments[..segments.len() - 1] {
        let obj = cur.as_object_mut().ok_or("path parent is not an object")?;
        if !obj.contains_key(*seg) {
            obj.insert((*seg).to_owned(), Value::Object(serde_json::Map::new()));
        }
        let next = obj.get_mut(*seg).ok_or("missing inserted object")?;
        if !next.is_object() {
            return Err("intermediate path component is not an object");
        }
        cur = next;
    }
    let last = segments[segments.len() - 1];
    let obj = cur.as_object_mut().ok_or("path parent is not an object")?;
    obj.insert(last.to_owned(), value);
    Ok(())
}
