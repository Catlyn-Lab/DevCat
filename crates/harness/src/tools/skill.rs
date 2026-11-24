//! `skill_save`: write down a procedure as a skill (`<folder>/<name>/SKILL.md` with `name` and
//! `description` in its front matter, the format Claude Code, Codex and agentskills.io read), so
//! later chats can follow it. A skill is instructions every later chat may load, so saving one
//! asks like any edit (outside the project for the user's own skills), an existing skill is
//! changed with `read` and `edit` rather than replaced, and nothing that looks like a secret goes
//! in. (Hermes Agent's `skill_manage`, MIT, is where the idea comes from.)
use super::{Tool, ToolCtx, ToolOutput, str_arg};
use crate::approval::Kind;
use crate::message::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;

pub const SKILL_SAVE: &str = "skill_save";

/// Where skills are saved, in the project and in the home folder: the one of the skill folders
/// (see [`crate::prompt::SKILL_DIRS`]) that belongs to no single agent.
pub const SKILLS_DIR: &str = ".agents/skills";

const MAX_DESCRIPTION: usize = 300;
const MAX_BODY: usize = 24_000;

pub struct SkillSave {
    /// The user's home folder, for skills of every project; None: project skills only.
    pub home: Option<PathBuf>,
}

impl SkillSave {
    fn file(&self, input: &Value, ctx: &ToolCtx) -> Option<PathBuf> {
        let name = input.get("name").and_then(Value::as_str)?.trim();
        let root = match input.get("scope").and_then(Value::as_str).unwrap_or("project") {
            "user" => self.home.clone()?,
            _ => ctx.cwd.clone(),
        };
        Some(root.join(SKILLS_DIR).join(name).join("SKILL.md"))
    }
}

/// A skill's name: lowercase letters, digits and dashes, as the folder it's in.
fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-') && !n.ends_with('-')
}

#[async_trait]
impl Tool for SkillSave {
    fn spec(&self) -> ToolSpec {
        let user = if self.home.is_some() { " `scope` \"user\" saves it for all the user's projects (in ~/.agents/skills) instead of this one's (.agents/skills)." } else { "" };
        ToolSpec {
            name: SKILL_SAVE.into(),
            description: format!(
                "Save a procedure as a skill that later chats can load and follow: when the user asks you to remember how to do something, or, after asking them, once you've worked out a non-trivial procedure they'll want again (a release, a migration, how this project's tests are run). Write it as steps to follow, with the pitfalls you hit and why, not as a story of this chat; leave out dates, ticket numbers and anything secret. The `description` says when to use it: later chats see only that until they read the skill.{user} To change an existing skill, `read` its SKILL.md and `edit` it instead."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Lowercase with dashes, e.g. \"release-checklist\"" },
                    "description": { "type": "string", "description": "One line: what it does and when to use it" },
                    "body": { "type": "string", "description": "The instructions, in Markdown" },
                    "scope": { "type": "string", "enum": ["project", "user"] }
                },
                "required": ["name", "description", "body"]
            }),
        }
    }
    fn kind(&self, input: &Value, ctx: &ToolCtx) -> Kind {
        match input.get("scope").and_then(Value::as_str) {
            Some("user") => Kind::EditOutside,
            _ if self.file(input, ctx).is_some_and(|f| super::inside(ctx, &f)) => Kind::Edit,
            _ => Kind::EditOutside,
        }
    }
    fn summary(&self, input: &Value) -> String {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("?");
        if input.get("scope").and_then(Value::as_str) == Some("user") { format!("{name} (for all projects)") } else { name.to_string() }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (name, description, body) = match (str_arg(&input, "name"), str_arg(&input, "description"), str_arg(&input, "body")) {
            (Ok(n), Ok(d), Ok(b)) => (n.trim(), d.trim(), b.trim()),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };
        if !valid_name(name) {
            return ToolOutput::err("`name` must be lowercase letters, digits and dashes (at most 64), e.g. \"release-checklist\".");
        }
        if description.is_empty() || description.contains('\n') || description.chars().count() > MAX_DESCRIPTION {
            return ToolOutput::err(format!("`description` must be one line of at most {MAX_DESCRIPTION} characters: what the skill does and when to use it."));
        }
        if body.is_empty() || body.chars().count() > MAX_BODY {
            return ToolOutput::err(format!("`body` must have the instructions, in at most {MAX_BODY} characters; put long references in other files next to SKILL.md."));
        }
        if super::chat_search::redact(body) != body || super::chat_search::redact(description) != description {
            return ToolOutput::err("Not saved: it looks like it has a secret in it (a key, token or password). Leave it out and say where it's found instead.");
        }
        if input.get("scope").and_then(Value::as_str) == Some("user") && self.home.is_none() {
            return ToolOutput::err("Skills for all projects can't be saved here; save it for this project instead.");
        }
        let Some(file) = self.file(&input, ctx) else { return ToolOutput::err("Couldn't tell where to save it.") };
        if file.exists() {
            return ToolOutput::err(format!("A skill named \"{name}\" exists already ({}). To change it, `read` it and `edit` it; to keep both, pick another name.", file.display()));
        }
        // (a description that starts like YAML would break the front matter: quoted, always)
        let text = format!("---\nname: {name}\ndescription: {}\n---\n\n{body}\n", serde_json::to_string(description).unwrap_or_default());
        if let Some(dir) = file.parent()
            && let Err(e) = tokio::fs::create_dir_all(dir).await
        {
            return ToolOutput::err(format!("Can't create {}: {e}", dir.display()));
        }
        match tokio::fs::write(&file, text).await {
            Ok(()) => ToolOutput::ok(format!("Saved the skill \"{name}\" in {}. Chats opened from now on list it; this one reads it like any file.", file.display())),
            Err(e) => ToolOutput::err(format!("Can't write {}: {e}", file.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saves_a_skill_that_the_prompt_lists() {
        let dir = std::env::temp_dir().join(format!("devcat-harness-skill-{}", uuid::Uuid::new_v4()));
        let (proj, home) = (dir.join("proj"), dir.join("home"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let ctx = ToolCtx::at(&proj);
        let t = SkillSave { home: Some(home.clone()) };
        let input = json!({ "name": "release-checklist", "description": "How to cut a release: tag, build, publish", "body": "1. Bump the version.\n2. Tag it." });
        assert_eq!(t.kind(&input, &ctx), Kind::Edit);
        let out = t.run(input.clone(), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let skills = crate::prompt::skills(&proj, None);
        assert_eq!(skills.len(), 1);
        assert_eq!((skills[0].name.as_str(), skills[0].description.as_str()), ("release-checklist", "How to cut a release: tag, build, publish"));
        // not replaced
        assert!(t.run(input, &ctx).await.content.contains("exists already"));
        // for every project: in the home folder, and it asks as an edit outside the project
        let mine = json!({ "name": "my-style", "description": "Commit messages: say why", "body": "Say why.", "scope": "user" });
        assert_eq!(t.kind(&mine, &ctx), Kind::EditOutside);
        assert!(!t.run(mine, &ctx).await.is_error);
        assert!(home.join(".agents/skills/my-style/SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refuses_bad_names_long_descriptions_and_secrets() {
        let ctx = ToolCtx::at(std::env::temp_dir());
        let t = SkillSave { home: None };
        let run = |name: &str, description: &str, body: &str| t.run(json!({ "name": name, "description": description, "body": body }), &ctx);
        assert!(run("../escape", "d", "b").await.content.contains("lowercase"));
        assert!(run("Has Caps", "d", "b").await.is_error);
        assert!(run("ok", "two\nlines", "b").await.content.contains("one line"));
        assert!(run("ok", "d", "Use api_key=sk-abcdefghijklmnopqrstuv to log in").await.content.contains("secret"));
        assert!(t.run(json!({ "name": "ok", "description": "d", "body": "b", "scope": "user" }), &ctx).await.content.contains("can't be saved here"));
    }
}
