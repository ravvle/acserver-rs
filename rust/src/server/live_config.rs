// The configuration tree, its Lua form and the in-game window follow
// AssettoServer. Copyright (C) 2023 Niewiarowski, compujuckel.
// AGPL-3.0-or-later.

//! Live configuration (`EnableLiveConfiguration`): settings an admin can
//! change while the server runs, with `/set <key> <value>`, over HTTP at
//! `/api/configuration`, or in CSP's in-game window. Keys are AssettoServer's
//! paths ("Extra.AiParams.MaxSpeedKph"), so its window works unchanged.
//! Settings read only at start are shown read-only.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Server;
use super::scripts::ServerScript;
use crate::config::{AiParams, Config, LiveExtra};

/// `Server.*` settings that may change at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LiveServer {
    /// TIME_OF_DAY_MULT.
    pub time_of_day_multiplier: f32,
}

/// Every live setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Live {
    pub server: LiveServer,
    pub extra: LiveExtra,
}

impl Live {
    pub fn from_config(cfg: &Config) -> Live {
        Live {
            server: LiveServer {
                time_of_day_multiplier: cfg.server.time_of_day_mult,
            },
            extra: cfg.extra.live,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let e = &self.extra;
        if e.max_afk_time_minutes <= 0 {
            return Err("MaxAfkTimeMinutes must be above 0".to_string());
        }
        if e.network_bubble_distance < 0.0 || self.server.time_of_day_multiplier < 0.0 {
            return Err("the value cannot be negative".to_string());
        }
        Ok(())
    }
}

/// AiParams fields read only when the server starts.
const AI_READ_ONLY: [&str; 4] = [
    "LaneWidthMeters",
    "SmoothCamber",
    "NamePrefix",
    "AiBehaviorUpdateIntervalHz",
];

/// Enum-valued settings and their values, for the window's drop-downs.
fn enum_values(name: &str) -> Option<Vec<&'static str>> {
    match name {
        "AfkKickBehavior" => Some(vec!["PlayerInput", "MinimumSpeed"]),
        _ => None,
    }
}

/// One setting as AssettoServer's ConfigurationProperty.
fn property(name: &str, value: &Value, read_only: bool, dicts: &[&str]) -> Option<Value> {
    let mut p = Map::new();
    p.insert("Name".into(), name.into());
    p.insert("ReadOnly".into(), read_only.into());
    let (kind, shown) = match value {
        Value::Null => return None,
        Value::Bool(_) => ("System.Boolean".to_string(), value.clone()),
        Value::Number(n) if n.is_f64() => ("System.Single".to_string(), value.clone()),
        Value::Number(_) => ("System.Int32".to_string(), value.clone()),
        Value::String(_) => match enum_values(name) {
            Some(valid) => {
                p.insert("ValidValues".into(), valid.into());
                ("enum".to_string(), value.clone())
            }
            None => ("System.String".to_string(), value.clone()),
        },
        Value::Array(items) => {
            let objects = items.iter().any(Value::is_object);
            p.insert(
                "EntryType".into(),
                if objects { "object" } else { "System.String" }.into(),
            );
            let shown: Vec<Value> = items
                .iter()
                .map(|v| {
                    if v.is_object() {
                        object(v, true, &[], &[])
                    } else {
                        v.clone()
                    }
                })
                .collect();
            ("list".to_string(), shown.into())
        }
        Value::Object(map) if dicts.contains(&name) => {
            p.insert("EntryType".into(), "object".into());
            let shown: Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), object(v, true, &[], &[])))
                .collect();
            ("dict".to_string(), Value::Object(shown))
        }
        Value::Object(_) => ("object".to_string(), object(value, true, &[], &[])),
    };
    p.insert("Type".into(), kind.into());
    p.insert("Value".into(), shown);
    Some(Value::Object(p))
}

/// A struct's fields as a ConfigurationObject (`{Properties = {…}}`).
fn object(value: &Value, read_only: bool, read_only_fields: &[&str], dicts: &[&str]) -> Value {
    let mut properties = Vec::new();
    if let Value::Object(map) = value {
        for (name, v) in map {
            let ro = read_only || read_only_fields.contains(&name.as_str());
            if let Some(p) = property(name, v, ro, dicts) {
                properties.push(p);
            }
        }
    }
    let mut o = Map::new();
    o.insert("Properties".into(), properties.into());
    Value::Object(o)
}

/// Writes JSON as a Lua table constructor, which CSP's `stringify.parse`
/// reads: objects with `name = value` fields, arrays as lists.
pub fn to_lua(value: &Value) -> String {
    let mut out = String::new();
    write_lua(value, &mut out);
    out
}

fn write_lua(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("nil"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_lua_string(s, out),
        Value::Array(items) => {
            out.push('{');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_lua(item, out);
            }
            out.push('}');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let identifier = key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if identifier {
                    out.push_str(key);
                } else {
                    out.push('[');
                    write_lua_string(key, out);
                    out.push(']');
                }
                out.push('=');
                write_lua(item, out);
            }
            out.push('}');
        }
    }
}

fn write_lua_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Replaces field `name` of `current` (a serialized struct) with `text`
/// parsed as the field's type.
fn set_field(current: &mut Value, name: &str, text: &str) -> Result<(), String> {
    let Some(field) = current.as_object_mut().and_then(|m| m.get_mut(name)) else {
        return Err(format!("Could not find property {name}"));
    };
    let text = text.trim();
    let parsed = match field {
        Value::Bool(_) => match text.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Value::Bool(true),
            "false" | "0" | "off" => Value::Bool(false),
            _ => return Err(format!("{name} is true or false")),
        },
        Value::Number(n) if n.is_f64() => {
            let v: f64 = text.parse().map_err(|_| format!("{name} is a number"))?;
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| format!("{name} is a number"))?
        }
        Value::Number(_) => Value::from(
            text.parse::<i64>()
                .map_err(|_| format!("{name} is a whole number"))?,
        ),
        Value::String(_) => {
            if let Some(valid) = enum_values(name)
                && !valid.contains(&text)
            {
                return Err(format!("{name} is one of {}", valid.join(", ")));
            }
            Value::String(text.to_string())
        }
        _ => return Err(format!("Property {name} cannot be set")),
    };
    *field = parsed;
    Ok(())
}

impl Server {
    /// The current live settings.
    pub(crate) fn live(&self) -> Live {
        *self.live.read()
    }

    /// The whole configuration tree, as AssettoServer's ConfigurationObject.
    pub fn configuration_tree(&self) -> Value {
        let live = self.live();
        let mut server = object(
            &serde_json::to_value(live.server).unwrap_or_default(),
            false,
            &[],
            &[],
        );
        // Shown for orientation; set in server_cfg.ini.
        if let Some(props) = server["Properties"].as_array_mut() {
            for (name, value) in [
                ("Name", Value::from(self.cfg.server.name.clone())),
                ("Track", Value::from(self.cfg.server.track.clone())),
                ("MaxClients", Value::from(self.cfg.server.max_clients)),
            ] {
                props.extend(property(name, &value, true, &[]));
            }
        }
        let mut extra = object(
            &serde_json::to_value(live.extra).unwrap_or_default(),
            false,
            &[],
            &[],
        );
        if let Some(traffic) = &self.traffic
            && let Some(props) = extra["Properties"].as_array_mut()
        {
            let ai = serde_json::to_value(&*traffic.params()).unwrap_or_default();
            let mut p = Map::new();
            p.insert("Name".into(), "AiParams".into());
            p.insert("Type".into(), "object".into());
            p.insert("ReadOnly".into(), false.into());
            p.insert(
                "Value".into(),
                object(&ai, false, &AI_READ_ONLY, &["LaneCountSpecificOverrides"]),
            );
            props.push(Value::Object(p));
        }
        let mut root = Map::new();
        root.insert(
            "Properties".into(),
            vec![named_object("Server", server), named_object("Extra", extra)].into(),
        );
        Value::Object(root)
    }

    /// Sets `key` ("Extra.MaxPing", "Extra.AiParams.MaxSpeedKph", …) from
    /// text, as AssettoServer's SetProperty.
    pub fn set_property(&self, key: &str, value: &str) -> Result<(), String> {
        let parts: Vec<&str> = key.split('.').collect();
        match parts.as_slice() {
            ["Server", field] => {
                let mut live = self.live();
                let mut v = serde_json::to_value(live.server).map_err(|e| e.to_string())?;
                set_field(&mut v, field, value)?;
                live.server = serde_json::from_value(v).map_err(|e| e.to_string())?;
                live.validate()?;
                *self.live.write() = live;
            }
            ["Extra", "AiParams", field] => {
                let traffic = self.traffic.as_ref().ok_or("AI traffic is off")?;
                if AI_READ_ONLY.contains(field) {
                    return Err(format!("Property {field} is read only"));
                }
                let mut v = serde_json::to_value(&*traffic.params()).map_err(|e| e.to_string())?;
                set_field(&mut v, field, value)?;
                let params: AiParams = serde_json::from_value(v).map_err(|e| e.to_string())?;
                params.validate()?;
                traffic.set_params(params);
                self.adjust_overbooking();
            }
            ["Extra", field] => {
                let mut live = self.live();
                let mut v = serde_json::to_value(live.extra).map_err(|e| e.to_string())?;
                set_field(&mut v, field, value)?;
                live.extra = serde_json::from_value(v).map_err(|e| e.to_string())?;
                live.validate()?;
                *self.live.write() = live;
            }
            _ => return Err(format!("Could not find property {key}")),
        }
        tracing::info!(key, value, "configuration changed");
        Ok(())
    }

    /// `/set <key> <value>`, with EnableLiveConfiguration.
    pub(crate) fn set_command(&self, args: &[&str]) -> Option<String> {
        if !self.cfg.extra.enable_live_configuration {
            return None;
        }
        let (Some(key), Some(_)) = (args.first(), args.get(1)) else {
            return Some("Usage: /set <key> <value>, e.g. /set Extra.MaxPing 300".to_string());
        };
        let value = args[1..].join(" ");
        Some(match self.set_property(key, &value) {
            Ok(()) => format!("Property {key} set to {value}"),
            Err(err) => err,
        })
    }

    /// `/api/configuration` (docs/05 §5.7): GET the tree, POST
    /// `?key=Root.<path>&value=<v>`. Only for a connected admin, by the
    /// X-Car-Id and X-Api-Key headers the in-game window sends.
    pub fn configuration_http(
        &self,
        method: &str,
        query: &HashMap<String, String>,
        headers: &HashMap<String, String>,
    ) -> (u16, String) {
        if !self.cfg.extra.enable_live_configuration {
            return (404, String::new());
        }
        let client = headers
            .get("x-car-id")
            .and_then(|id| id.trim().parse::<u8>().ok())
            .and_then(|id| self.entry_cars.get(usize::from(id))?.client());
        let authorised = client.is_some_and(|c| {
            c.is_admin()
                && headers
                    .get("x-api-key")
                    .is_some_and(|k| super::handshake::secret_equal(k, c.api_key()))
        });
        if !authorised {
            return (401, String::new());
        }
        match method {
            "GET" => (200, to_lua(&self.configuration_tree())),
            "POST" => {
                let key = query.get("key").map(String::as_str).unwrap_or_default();
                let key = key.strip_prefix("Root.").unwrap_or(key);
                let value = query.get("value").map(String::as_str).unwrap_or_default();
                let reply = match self.set_property(key, value) {
                    Ok(()) => serde_json::json!({ "Status": "OK" }),
                    Err(err) => serde_json::json!({ "Status": "Error", "ErrorMessage": err }),
                };
                (200, to_lua(&reply))
            }
            _ => (405, String::new()),
        }
    }
}

fn named_object(name: &str, value: Value) -> Value {
    let mut p = Map::new();
    p.insert("Name".into(), name.into());
    p.insert("Type".into(), "object".into());
    p.insert("ReadOnly".into(), false.into());
    p.insert("Value".into(), value);
    Value::Object(p)
}

/// The in-game configuration window (EnableLiveConfiguration).
pub fn window_script() -> ServerScript {
    ServerScript::Local {
        name: "acserver_admin.lua".to_string(),
        source: include_bytes!("acserver_admin.lua").to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_tables() {
        let v =
            serde_json::json!({"Status": "OK", "Properties": [1, true, "a\"b"], "two words": null});
        assert_eq!(
            to_lua(&v),
            r#"{Properties={1,true,"a\"b"},Status="OK",["two words"]=nil}"#
        );
    }

    #[test]
    fn fields_parse_by_type() {
        let mut v = serde_json::json!({"MaxPing": 500, "ForceLights": false, "MaxSpeedKph": 80.0, "AfkKickBehavior": "PlayerInput"});
        set_field(&mut v, "MaxPing", "300").expect("int");
        set_field(&mut v, "ForceLights", "true").expect("bool");
        set_field(&mut v, "MaxSpeedKph", "92.5").expect("float");
        set_field(&mut v, "AfkKickBehavior", "MinimumSpeed").expect("enum");
        assert!(set_field(&mut v, "AfkKickBehavior", "Never").is_err());
        assert!(set_field(&mut v, "MaxPing", "lots").is_err());
        assert!(set_field(&mut v, "Nope", "1").is_err());
        assert_eq!(v["MaxPing"], 300);
        assert_eq!(v["ForceLights"], true);
        assert_eq!(v["MaxSpeedKph"], 92.5);
    }
}
