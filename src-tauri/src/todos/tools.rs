//! `todo_write` and `todo_list`: the To do list (`mod.rs`) as tools, built into the DevCat Agent
//! and served by `devcat mcp`, so every agent keeps the same list for the project it works in.
use super::{Change, Edit, Group, Item, TodoList};
use crate::agent_tools::Scope;
use crate::memory::{Place, place_for};
use async_trait::async_trait;
use devcat_harness::approval::Kind;
use devcat_harness::message::ToolSpec;
use devcat_harness::tools::{Tool, ToolCtx, ToolOutput};
use serde_json::{Value, json};
use std::sync::Arc;

/// Done items `todo_list` shows unless asked for the done group.
const DONE_SHOWN: usize = 10;

/// Who calls the tools and where.
pub struct Ctx {
    /// The place the agent works in (none: no list).
    pub here: Option<Place>,
    /// The agent (`claude-code`, `codex`, `devcat-agent`, …): new items' owner.
    pub origin: String,
    /// The tools' names: [`MCP_NAMES`], or [`AGENT_NAMES`] in the DevCat Agent.
    pub names: Names,
}

/// What the two tools are called.
#[derive(Clone, Copy)]
pub struct Names {
    pub write: &'static str,
    pub list: &'static str,
}

/// Over `devcat mcp`, where the client puts the server's name in front (`mcp__devcat__todo_write`).
pub const MCP_NAMES: Names = Names { write: "todo_write", list: "todo_list" };
/// In the DevCat Agent, whose own `todo_write` is the chat's checklist (the harness's): a request
/// with two tools of one name is refused by the model provider ("Tool names must be unique").
pub const AGENT_NAMES: Names = Names { write: "project_todo_write", list: "project_todo_list" };

/// The To do tools for an agent working in `scope`, served over `devcat mcp`.
pub fn for_scope(scope: &Scope, origin: &str) -> Vec<Arc<dyn Tool>> {
    named(scope, origin, MCP_NAMES)
}

/// The To do tools for the DevCat Agent working in `scope` ([`AGENT_NAMES`]).
pub fn for_agent(scope: &Scope) -> Vec<Arc<dyn Tool>> {
    named(scope, "devcat-agent", AGENT_NAMES)
}

fn named(scope: &Scope, origin: &str, names: Names) -> Vec<Arc<dyn Tool>> {
    let ctx = Arc::new(Ctx { here: place_for(scope).filter(|p| *p != Place::Global), origin: origin.to_string(), names });
    vec![Arc::new(TodoWrite(ctx.clone())), Arc::new(TodoListTool(ctx))]
}

const NO_PLACE: &str = "The To do list belongs to a project: this folder isn't a project or repository DevCat knows. Work from the project's folder.";

fn group_names() -> Vec<&'static str> {
    Group::ALL.iter().map(|g| g.name()).collect()
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k)).and_then(Value::as_str)
}

/// One item of `items`, as the agent wrote it.
fn change(v: &Value, n: usize) -> Result<Change, String> {
    if !v.is_object() {
        return Err(format!("items[{n}] must be an object"));
    }
    let group = match str_field(v, &["group"]) {
        None => None,
        Some(g) => Some(Group::parse(g).ok_or_else(|| format!("items[{n}]: unknown group \"{g}\"; use one of {}", group_names().join(", ")))?),
    };
    let links = match v.get("links") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(a)) => Some(a.iter().filter_map(Value::as_str).map(String::from).collect()),
        Some(_) => return Err(format!("items[{n}].links must be a list of URLs or paths")),
    };
    let id = match v.get("id") {
        Some(Value::Number(x)) => Some(x.to_string()),
        _ => str_field(v, &["id"]).map(String::from),
    };
    Ok(Change {
        id,
        text: str_field(v, &["text"]).map(String::from),
        group,
        waiting_on: str_field(v, &["waiting_on", "waitingOn"]).map(String::from),
        links,
        owner: str_field(v, &["owner"]).map(String::from),
    })
}

/// The edit `todo_write`'s input asks for.
fn edit(input: &Value) -> Result<Edit, String> {
    let items = match input.get("items") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.iter().enumerate().map(|(n, v)| change(v, n)).collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("`items` must be a list".into()),
    };
    let remove: Vec<String> = match input.get("remove") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(_) => return Err("`remove` must be a list of item ids".into()),
    };
    if input.get("replace").and_then(Value::as_bool).unwrap_or(false) {
        if !remove.is_empty() {
            return Err("With `replace`, leave out `remove`: items not in the new list are removed".into());
        }
        return Ok(Edit::Replace(items));
    }
    if items.is_empty() && remove.is_empty() {
        return Err("Nothing to change: give `items` to add or update, `remove` to delete, or `replace: true` with the whole list".into());
    }
    Ok(Edit::Apply { changes: items, remove })
}

fn item_line(i: &Item) -> String {
    let mut s = format!("- [{}] {}", i.id, i.text);
    if let Some(w) = &i.waiting_on {
        s.push_str(&format!(" · waiting on {w}"));
    }
    if let Some(o) = &i.owner {
        s.push_str(&format!(" · {o}"));
    }
    if !i.links.is_empty() {
        s.push_str(&format!(" · {}", i.links.join(", ")));
    }
    s.push_str(&format!(" · {}", i.updated_at));
    s
}

/// The list as the agent reads it: each group with its items (only `only`, when given).
pub fn render(list: &TodoList, only: Option<Group>) -> String {
    let mut out = format!("To do for {}", list.label);
    if !list.updated_at.is_empty() {
        out.push_str(&format!(" (updated {})", list.updated_at));
    }
    out.push('\n');
    let mut any = false;
    for g in Group::ALL.into_iter().filter(|g| only.is_none_or(|o| o == *g)) {
        let mut items: Vec<&Item> = list.items.iter().filter(|i| i.group == g).collect();
        if items.is_empty() {
            continue;
        }
        any = true;
        out.push_str(&format!("\n{} ({})\n", g.label(), items.len()));
        if g == Group::Done && only.is_none() && items.len() > DONE_SHOWN {
            items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            items.truncate(DONE_SHOWN);
            out.push_str(&format!("(newest {DONE_SHOWN}; reading the list with group \"done\" lists them all)\n"));
        }
        for i in items {
            out.push_str(&item_line(i));
            out.push('\n');
        }
    }
    if !any {
        out.push_str(if only.is_some() { "\nNothing in that group." } else { "\nThe list is empty." });
    }
    out.trim_end().to_string()
}

// ---------- todo_write

struct TodoWrite(Arc<Ctx>);

#[async_trait]
impl Tool for TodoWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.names.write.into(),
            description: format!("Keep the user's To do list for this project current, in DevCat: what's running, what's next, what waits on someone else, what waits on the user, and what's done. It outlives your session (another agent, or you after a restart, picks it up with `{}`) and the user watches it in DevCat's To do panel. Update it as you work: add tasks when you plan, move one to running when you start it, to waiting_on_others / waiting_on_you when it's blocked (say on whom in `waiting_on`), to done when it's finished; add PR, issue or file links. `items` adds or updates items: an item with an `id` the list has changes only the fields you give; without an `id` (or a new one) it's added, with `text`. `remove` deletes items by id. `replace: true` makes `items` the whole list. Returns the list with the ids.", self.0.names.list),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "Items to add or update",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "The item's id (from the list) to update it; leave out to add one" },
                                "text": { "type": "string", "description": "What the task is, in a short line" },
                                "group": { "type": "string", "enum": group_names(), "description": "Where it stands (new items: next)" },
                                "waiting_on": { "type": "string", "description": "Who or what it waits on (a person, a review, a deploy); empty clears it" },
                                "links": { "type": "array", "items": { "type": "string" }, "description": "PR, issue or file URLs or paths (replaces the item's links)" },
                                "owner": { "type": "string", "description": "The agent or session working on it (new items: you)" }
                            }
                        }
                    },
                    "remove": { "type": "array", "items": { "type": "string" }, "description": "Ids of items to delete (wrong or no longer wanted; finished work goes to done instead)" },
                    "replace": { "type": "boolean", "description": "true: `items` is the whole list, anything else is removed" }
                }
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Edit
    }
    fn summary(&self, input: &Value) -> String {
        let n = input.get("items").and_then(Value::as_array).map_or(0, Vec::len);
        let r = input.get("remove").and_then(Value::as_array).map_or(0, Vec::len);
        match (input.get("replace").and_then(Value::as_bool).unwrap_or(false), n, r) {
            (true, n, _) => format!("replace with {n} items"),
            (false, n, 0) => format!("{n} items"),
            (false, 0, r) => format!("remove {r}"),
            (false, n, r) => format!("{n} items, remove {r}"),
        }
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let Some(place) = self.0.here.clone() else { return ToolOutput::err(NO_PLACE) };
        let edit = match edit(&input) {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(e),
        };
        match super::write(&place, edit, &self.0.origin) {
            Ok(w) => {
                let mut out = String::new();
                if let Some(p) = &w.set_aside {
                    out.push_str(&format!("The old list file wasn't readable; it was kept as {} and a new list started.\n", p.display()));
                }
                if !w.unknown.is_empty() {
                    out.push_str(&format!("Not on the list (nothing removed): {}\n", w.unknown.join(", ")));
                }
                out.push_str(&render(&w.list, None));
                ToolOutput::ok(out)
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

// ---------- todo_list

struct TodoListTool(Arc<Ctx>);

#[async_trait]
impl Tool for TodoListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.names.list.into(),
            description: format!("Read the user's To do list for this project (DevCat): what's running, next, waiting on others, waiting on the user, and done, with each item's id, who it waits on, its owner and links. Read it when you start or resume work in the project, before you change it with `{}`. `group` shows one group only.", self.0.names.write),
            input_schema: json!({
                "type": "object",
                "properties": { "group": { "type": "string", "enum": group_names() } }
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Read
    }
    fn summary(&self, input: &Value) -> String {
        str_field(input, &["group"]).unwrap_or("").to_string()
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let Some(place) = self.0.here.clone() else { return ToolOutput::err(NO_PLACE) };
        let only = match str_field(&input, &["group"]).map(str::trim).filter(|g| !g.is_empty()) {
            None => None,
            Some(g) => match Group::parse(g) {
                Some(g) => Some(g),
                None => return ToolOutput::err(format!("Unknown group \"{g}\"; use one of {}", group_names().join(", "))),
            },
        };
        match super::load(&place) {
            Ok(list) => ToolOutput::ok(render(&list, only)),
            Err(e) => ToolOutput::err(format!("{e}. The next `todo_write` keeps that file aside and starts a new list.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::at(std::env::temp_dir())
    }

    fn tool(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools.iter().find(|t| t.spec().name == name).cloned().unwrap()
    }

    #[test]
    fn the_devcat_agent_gets_names_its_own_tools_dont_have() {
        let standard: Vec<String> = devcat_harness::tools::Registry::standard().specs().into_iter().map(|s| s.name).collect();
        let agent: Vec<String> = for_agent(&Scope::default()).iter().map(|t| t.spec().name).collect();
        assert_eq!(agent, ["project_todo_write", "project_todo_list"]);
        assert!(agent.iter().all(|n| !standard.contains(n)), "{agent:?} vs {standard:?}");
        // each tool points at its partner by the name it has
        let tools = for_agent(&Scope::default());
        assert!(tools[0].spec().description.contains("`project_todo_list`"));
        assert!(tools[1].spec().description.contains("`project_todo_write`"));
        let mcp = for_scope(&Scope::default(), "codex");
        assert!(mcp[0].spec().description.contains("`todo_list`") && mcp[1].spec().description.contains("`todo_write`"));
    }

    #[test]
    fn reads_the_agents_arguments() {
        let e = edit(&json!({ "items": [{ "text": "A", "group": "Waiting on others", "waiting_on": "Pete", "links": "https://x/1" }, { "id": 3, "group": "done" }], "remove": ["t9"] })).unwrap();
        let Edit::Apply { changes, remove } = e else { panic!("apply") };
        assert_eq!(changes[0].group, Some(Group::WaitingOnOthers));
        assert_eq!((changes[0].waiting_on.as_deref(), changes[0].links.clone()), (Some("Pete"), Some(vec!["https://x/1".to_string()])));
        assert_eq!((changes[1].id.as_deref(), changes[1].group), (Some("3"), Some(Group::Done)));
        assert_eq!(remove, vec!["t9".to_string()]);
        assert!(matches!(edit(&json!({ "items": [], "replace": true })).unwrap(), Edit::Replace(v) if v.is_empty()), "replace with nothing clears the list");
        assert!(edit(&json!({})).unwrap_err().contains("Nothing to change"));
        assert!(edit(&json!({ "items": [{ "text": "A", "group": "someday" }] })).unwrap_err().contains("unknown group \"someday\""));
        assert!(edit(&json!({ "items": "A" })).is_err());
        assert!(edit(&json!({ "items": ["A"] })).is_err());
        assert!(edit(&json!({ "items": [{ "text": "A", "links": 5 }] })).is_err());
        assert!(edit(&json!({ "replace": true, "items": [], "remove": ["t1"] })).is_err());
    }

    #[tokio::test]
    async fn one_agent_writes_another_reads() {
        super::super::tests::data();
        let dir = std::env::temp_dir().join(format!("devcat-todo-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let scope = Scope::folder(&dir.join("src"));
        let claude = for_scope(&scope, "claude-code");
        let codex = for_scope(&scope, "codex");

        let out = tool(&claude, "todo_write")
            .run(json!({ "items": [{ "text": "Port the ledger API", "group": "running", "links": ["https://github.com/o/r/pull/7"] }, { "text": "Ask Pete about the fx rates", "group": "waiting_on_others", "waiting_on": "Pete" }] }), &ctx())
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Running (1)\n- [t1] Port the ledger API · claude-code · https://github.com/o/r/pull/7"), "{}", out.content);
        assert!(out.content.contains("[t2] Ask Pete about the fx rates · waiting on Pete"), "{}", out.content);

        let read = tool(&codex, "todo_list").run(json!({}), &ctx()).await;
        assert!(read.content.contains("Port the ledger API") && read.content.contains("Waiting on others (1)"), "{}", read.content);
        let moved = tool(&codex, "todo_write").run(json!({ "items": [{ "id": "t1", "group": "done" }], "remove": ["t5"] }), &ctx()).await;
        assert!(moved.content.contains("Not on the list (nothing removed): t5") && moved.content.contains("Done (1)"), "{}", moved.content);
        let done = tool(&codex, "todo_list").run(json!({ "group": "done" }), &ctx()).await;
        assert!(done.content.contains("[t1]") && !done.content.contains("[t2]"), "{}", done.content);
        assert!(tool(&codex, "todo_list").run(json!({ "group": "later" }), &ctx()).await.is_error);
        let replaced = tool(&codex, "todo_write").run(json!({ "replace": true, "items": [] }), &ctx()).await;
        assert!(replaced.content.contains("The list is empty."), "{}", replaced.content);

        // no project: the tools say so
        let none = for_scope(&Scope::default(), "codex");
        assert!(tool(&none, "todo_write").run(json!({ "items": [{ "text": "x" }] }), &ctx()).await.is_error);
        assert!(tool(&none, "todo_list").run(json!({}), &ctx()).await.is_error);
        let _ = std::fs::remove_dir_all(dir);
    }
}
