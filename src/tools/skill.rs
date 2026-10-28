//! Skills (plugin `tool-skill`): markdown playbooks loaded on demand.
//!
//! A skill is a directory containing `SKILL.md` with `name:` and
//! `description:` front matter. Skills are discovered in `<home>/skills`,
//! `<workspace>/.skills`, and any `dirs` from the plugin config. The system
//! prompt lists them; the `skill` tool returns one skill's full text.

use std::fs;
use std::path::PathBuf;

use super::{Tool, ToolCx, ToolOutput, ToolSpec, prop, schema, str_arg, try_out};
use crate::jobj;
use crate::json::Json;
use crate::util;

#[derive(Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

pub fn discover(dirs: &[PathBuf]) -> Vec<SkillInfo> {
    let mut skills: Vec<SkillInfo> = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path().join("SKILL.md");
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let folder = entry.file_name().to_string_lossy().into_owned();
            let (name, description) = front_matter(&text);
            let name = name.unwrap_or(folder);
            if skills.iter().any(|s| s.name == name) {
                continue;
            }
            skills.push(SkillInfo { name, description: description.unwrap_or_default(), path });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---") else { return (None, None) };
    let Some(end) = rest.find("\n---") else { return (None, None) };
    let mut name = None;
    let mut description = None;
    for line in rest[..end].lines() {
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().trim_matches('"').to_string());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().trim_matches('"').to_string());
        }
    }
    (name, description)
}

pub fn prompt_section(skills: &[SkillInfo]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut text = String::from("# Skills\nLoad a skill with the `skill` tool when its description matches the task:\n");
    for skill in skills {
        text.push_str(&format!("- {}: {}\n", skill.name, util::truncate(&skill.description, 300, "…")));
    }
    Some(text)
}

pub struct SkillTool {
    pub dirs: Vec<PathBuf>,
}

impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".into(),
            description: "Load the full instructions of a named skill listed in the system prompt.".into(),
            parameters: schema(jobj! { "name" => prop("string", "Skill name.") }, &["name"]),
        }
    }

    fn call(&self, _cx: &ToolCx<'_>, args: &Json) -> ToolOutput {
        let name = try_out!(str_arg(args, "name"));
        let skills = discover(&self.dirs);
        match skills.iter().find(|s| s.name == name) {
            Some(skill) => match fs::read_to_string(&skill.path) {
                Ok(text) => ToolOutput::ok(util::truncate(&text, 60_000, "\n…[truncated]")),
                Err(e) => ToolOutput::err(format!("cannot read skill {name}: {e}")),
            },
            None => ToolOutput::err(format!(
                "no skill named {name:?}; available: {}",
                skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}
