//! Cordis loader: a profile is an ordered list of plugin entries, patches edit
//! that list, and `start` activates the result in dependency order.
//!
//! Patch files are YAML arrays. Each element is one operation:
//!
//! ```yaml
//! - id: system-prompt            # merge config into an existing entry
//!   config: { personaPrefix: hello }
//! - id: tool-bash                # disable (or re-enable) an entry
//!   disabled: true
//! - insert:                      # add entries (optionally before/after an id)
//!     - id: mcp-tools
//!       name: '@deepseek-ai/dsh-mcp-client'
//!       config: { serverName: tools, url: http://127.0.0.1:8000/mcp }
//! - remove: tool-web             # drop an entry entirely
//! ```
//!
//! A settings file is a YAML mapping of `entry-id: config`, merged the same way.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::{Context, Plugin};
use crate::json::Json;

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub config: Json,
    pub disabled: bool,
}

impl Entry {
    pub fn new(id: &str) -> Entry {
        Entry { id: id.into(), name: id.into(), config: Json::obj(), disabled: false }
    }

    pub fn named(id: &str, name: &str) -> Entry {
        Entry { id: id.into(), name: name.into(), config: Json::obj(), disabled: false }
    }

    pub fn with_config(mut self, config: Json) -> Entry {
        self.config = config;
        self
    }

    pub fn off(mut self) -> Entry {
        self.disabled = true;
        self
    }
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, Arc<dyn Plugin>>,
    aliases: HashMap<String, String>,
}

impl PluginRegistry {
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.insert(plugin.name().to_string(), plugin);
    }

    pub fn alias(&mut self, alias: &str, name: &str) {
        self.aliases.insert(alias.to_string(), name.to_string());
    }

    /// Resolve a plugin by name, alias, or DeepSeek Harness package name
    /// (`@deepseek-ai/dsh-<name>`).
    pub fn resolve(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        if let Some(plugin) = self.plugins.get(name) {
            return Some(plugin.clone());
        }
        if let Some(target) = self.aliases.get(name) {
            return self.plugins.get(target).cloned();
        }
        let short = name.strip_prefix("@deepseek-ai/dsh-").or_else(|| name.strip_prefix("@melchior/"))?;
        self.resolve(short)
    }
}

fn entry_from_json(value: &Json) -> Result<Entry, String> {
    let id = value.get("id").as_str().ok_or("inserted entries need a string id")?;
    let name = value.get("name").as_str().unwrap_or(id);
    let config = match value.get("config") {
        Json::Null => Json::obj(),
        c @ Json::Obj(_) => c.clone(),
        _ => return Err(format!("entry {id:?}: config must be a mapping")),
    };
    Ok(Entry {
        id: id.into(),
        name: name.into(),
        config,
        disabled: value.get("disabled").as_bool().unwrap_or(false),
    })
}

/// Apply one parsed patch file. Returns warnings for operations that named
/// entries this harness does not have (they are skipped, not fatal).
pub fn apply_patch(entries: &mut Vec<Entry>, patch: &Json, origin: &str) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    let ops = match patch {
        Json::Null => return Ok(warnings),
        Json::Arr(items) => items.clone(),
        Json::Obj(fields) => fields
            .iter()
            .map(|(id, config)| crate::jobj! { "id" => id.as_str(), "config" => config.clone() })
            .collect(),
        _ => return Err(format!("{origin}: a patch must be a list of operations or a mapping")),
    };
    for op in &ops {
        if !matches!(op, Json::Obj(_)) {
            return Err(format!("{origin}: each patch operation must be a mapping"));
        }
        if let Some(inserted) = op.get("insert").as_array() {
            let mut position = entries.len();
            if let Some(anchor) = op.get("before").as_str() {
                position = entries.iter().position(|e| e.id == anchor).ok_or(format!("{origin}: unknown anchor {anchor:?}"))?;
            } else if let Some(anchor) = op.get("after").as_str() {
                position =
                    entries.iter().position(|e| e.id == anchor).ok_or(format!("{origin}: unknown anchor {anchor:?}"))? + 1;
            }
            for item in inserted {
                let entry = entry_from_json(item).map_err(|e| format!("{origin}: {e}"))?;
                if entries.iter().any(|e| e.id == entry.id) {
                    return Err(format!("{origin}: entry {:?} already exists", entry.id));
                }
                entries.insert(position, entry);
                position += 1;
            }
            continue;
        }
        if let Some(id) = op.get("remove").as_str() {
            entries.retain(|e| e.id != id);
            continue;
        }
        let Some(id) = op.get("id").as_str() else {
            return Err(format!("{origin}: operation needs id, insert, or remove"));
        };
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            let only_disables = op.get("disabled").as_bool() == Some(true) && op.get("config").is_null();
            if !only_disables {
                warnings.push(format!("{origin}: no plugin entry {id:?}; operation skipped"));
            }
            continue;
        };
        if op.get("remove").as_bool() == Some(true) {
            entries.retain(|e| e.id != id);
            continue;
        }
        if let Some(disabled) = op.get("disabled").as_bool() {
            entry.disabled = disabled;
        }
        if let Some(name) = op.get("name").as_str() {
            entry.name = name.to_string();
        }
        match op.get("config") {
            Json::Null => {}
            patch @ Json::Obj(_) => entry.config.deep_merge(patch),
            _ => return Err(format!("{origin}: config for {id:?} must be a mapping")),
        }
    }
    Ok(warnings)
}

/// Parse a YAML patch file, evaluating `!!js` with the safe evaluator.
pub fn read_patch_file(path: &Path) -> Result<Json, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read patch {}: {e}", path.display()))?;
    parse_patch_text(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_patch_text(text: &str) -> Result<Json, String> {
    crate::yaml::parse(text, &mut |tag, body| match tag {
        "!!js" | "!js" => crate::jsexpr::eval_process_env(body),
        "!env" => Ok(std::env::var(body.trim()).map(Json::Str).unwrap_or(Json::Null)),
        other => Err(format!("unsupported YAML tag {other}")),
    })
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub active: Vec<(String, String)>,
    pub disabled: Vec<String>,
    pub unknown: Vec<(String, String)>,
    pub pending: Vec<(String, Vec<String>)>,
    pub failed: Vec<(String, String)>,
}

impl LoadReport {
    pub fn to_json(&self) -> Json {
        let pairs = |items: &[(String, String)], a: &str, b: &str| {
            Json::Arr(items.iter().map(|(x, y)| crate::jobj! { a => x.as_str(), b => y.as_str() }).collect())
        };
        crate::jobj! {
            "active" => pairs(&self.active, "id", "plugin"),
            "disabled" => Json::Arr(self.disabled.iter().map(|d| Json::from(d.as_str())).collect()),
            "unknown" => pairs(&self.unknown, "id", "name"),
            "pending" => Json::Arr(self.pending.iter().map(|(id, missing)| crate::jobj! {
                "id" => id.as_str(),
                "missing" => Json::Arr(missing.iter().map(|m| Json::from(m.as_str())).collect()),
            }).collect()),
            "failed" => pairs(&self.failed, "id", "error"),
        }
    }
}

/// Activate enabled entries. Each pass applies every entry whose injected
/// services exist, in profile order, until a pass makes no progress.
pub fn start(root: &Context, registry: &PluginRegistry, entries: &[Entry]) -> LoadReport {
    let mut report = LoadReport::default();
    let mut waiting: Vec<(Entry, Arc<dyn Plugin>)> = Vec::new();
    for entry in entries {
        if entry.disabled {
            report.disabled.push(entry.id.clone());
            continue;
        }
        match registry.resolve(&entry.name) {
            Some(plugin) => waiting.push((entry.clone(), plugin)),
            None => {
                crate::log_warn!("loader", "entry {:?} names unknown plugin {:?}; skipped", entry.id, entry.name);
                report.unknown.push((entry.id.clone(), entry.name.clone()));
            }
        }
    }
    loop {
        let mut progressed = false;
        let mut still = Vec::new();
        for (entry, plugin) in waiting {
            let ready = plugin.inject().iter().all(|name| root.app().has_service(name));
            if !ready {
                still.push((entry, plugin));
                continue;
            }
            progressed = true;
            match root.plugin(plugin.as_ref(), &entry.id, entry.config.clone()) {
                Ok(_) => {
                    crate::log_debug!("loader", "activated {} ({})", entry.id, plugin.name());
                    report.active.push((entry.id.clone(), plugin.name().to_string()));
                }
                Err(error) => {
                    crate::log_error!("loader", "plugin {} failed to start: {error}", entry.id);
                    report.failed.push((entry.id.clone(), error));
                }
            }
        }
        waiting = still;
        if !progressed || waiting.is_empty() {
            break;
        }
    }
    for (entry, plugin) in waiting {
        let missing: Vec<String> =
            plugin.inject().iter().filter(|n| !root.app().has_service(n)).map(|n| n.to_string()).collect();
        crate::log_warn!("loader", "plugin {} is waiting for {}", entry.id, missing.join(", "));
        report.pending.push((entry.id, missing));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_deepseek_harness_style_patches() {
        let mut entries = vec![Entry::new("system-prompt"), Entry::new("tool-bash"), Entry::new("llm-pi-ai")];
        let patch = parse_patch_text(
            "- id: system-prompt\n  config:\n    personaPrefix: hi\n- id: tool-bash\n  disabled: true\n- id: tool-ralph\n  disabled: true\n- id: nope\n  config: {a: 1}\n- insert:\n    - id: mcp-tools\n      name: '@deepseek-ai/dsh-mcp-client'\n      config: {serverName: tools}\n",
        )
        .unwrap();
        let warnings = apply_patch(&mut entries, &patch, "test").unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(entries[0].config.get("personaPrefix").as_str(), Some("hi"));
        assert!(entries[1].disabled);
        assert_eq!(entries[3].name, "@deepseek-ai/dsh-mcp-client");
    }

    #[test]
    fn settings_mapping_merges_configs() {
        let mut entries = vec![Entry::new("llm-pi-ai").with_config(crate::jobj! { "keep" => 1 })];
        let patch = parse_patch_text("llm-pi-ai:\n  providers:\n    openrouter:\n      baseURL: http://x\n").unwrap();
        apply_patch(&mut entries, &patch, "settings").unwrap();
        assert_eq!(entries[0].config.get("keep").as_i64(), Some(1));
        assert_eq!(entries[0].config.get("providers").get("openrouter").get("baseURL").as_str(), Some("http://x"));
    }
}
