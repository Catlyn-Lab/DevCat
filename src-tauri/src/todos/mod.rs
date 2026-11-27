//! To do: a per-project task list the user's coding agents keep current (what's running, what's
//! next, what waits on someone), so a session that restarts, or another agent, picks the list up
//! where it was. Agents write it with `todo_write` and read it with `todo_list` (`tools.rs`); the
//! app shows it in the To do sidebar, where the user ticks items done or moves them.
//!
//! - One list per place, the way the shared memory names places (`memory::Place`): a DevCat
//!   project (its id), else the repository the agent works in. Each list is
//!   `<data>/todos/<key>.json`, written whole to a temp file and renamed into place.
//! - Several `devcat mcp` processes may write while DevCat is closed: the last write wins. With
//!   DevCat open every call runs in the app (`agent_bridge.rs`), one at a time ([`LOCK`]).
//! - A file that isn't valid JSON is never overwritten: the next write moves it aside
//!   (`<key>.bad-<time>.json`) and starts a new list.
//! - After a change the windows reload the list (`devcat://todos` event).
pub mod tools;

use crate::error::{AppError, AppResult};
use crate::memory::Place;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Most items in one list; done items beyond [`MAX_DONE`] go first.
pub const MAX_ITEMS: usize = 200;
/// Done items kept (the newest).
pub const MAX_DONE: usize = 50;
const MAX_TEXT: usize = 500;
const MAX_WAITING: usize = 200;
const MAX_OWNER: usize = 80;
const MAX_LINKS: usize = 10;
const MAX_LINK: usize = 1000;

/// The windows reload a list when this comes (payload `{ key }`).
pub const EVENT: &str = "devcat://todos";

static DATA_DIR: OnceCell<PathBuf> = OnceCell::new();
/// One change at a time in this process (read, change, write).
static LOCK: Mutex<()> = Mutex::new(());

pub fn init(data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
}

fn dir() -> AppResult<PathBuf> {
    DATA_DIR.get().map(|d| d.join("todos")).ok_or_else(|| AppError::msg("DevCat's To do list isn't ready yet"))
}

// ---------- items

/// Where an item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Running,
    Next,
    WaitingOnOthers,
    WaitingOnYou,
    Done,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Running, Group::Next, Group::WaitingOnOthers, Group::WaitingOnYou, Group::Done];

    pub fn name(self) -> &'static str {
        match self {
            Group::Running => "running",
            Group::Next => "next",
            Group::WaitingOnOthers => "waiting_on_others",
            Group::WaitingOnYou => "waiting_on_you",
            Group::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Group::Running => "Running",
            Group::Next => "Next",
            Group::WaitingOnOthers => "Waiting on others",
            Group::WaitingOnYou => "Waiting on you",
            Group::Done => "Done",
        }
    }

    /// The group an agent names: `waiting_on_you`, `Waiting on you`, `waiting-on-you`, `todo`…
    pub fn parse(s: &str) -> Option<Group> {
        let s: String = s.trim().to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        Some(match s.as_str() {
            "running" | "in_progress" | "doing" | "active" => Group::Running,
            "next" | "todo" | "to_do" | "pending" | "queued" => Group::Next,
            "waiting_on_others" | "waiting_others" | "blocked" | "waiting" => Group::WaitingOnOthers,
            "waiting_on_you" | "waiting_on_user" | "waiting_you" | "needs_you" => Group::WaitingOnYou,
            "done" | "completed" | "finished" => Group::Done,
            _ => return None,
        })
    }
}

/// One item on the list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub text: String,
    pub group: Group,
    /// Who or what it waits on (a person, a review, a deploy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    /// PR / issue / file URLs or paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// The agent or session working on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// RFC 3339, UTC.
    pub updated_at: String,
}

/// A change to one item: a new item (no `id`, or an id the list hasn't), or the fields to change
/// on an existing one. An empty `waiting_on` / `owner` clears it; `links` replaces the links.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub group: Option<Group>,
    #[serde(default)]
    pub waiting_on: Option<String>,
    #[serde(default)]
    pub links: Option<Vec<String>>,
    #[serde(default)]
    pub owner: Option<String>,
}

/// One place's list, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoList {
    pub key: String,
    pub place: Place,
    /// How the app shows the place (project name, or the folder).
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub items: Vec<Item>,
}

/// A list in the sidebar's picker.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub key: String,
    pub place: Place,
    pub label: String,
    pub updated_at: String,
    /// Items not done.
    pub open: usize,
}

// ---------- keys and files

/// The file name for a place's list (none for every project: lists belong to one).
pub fn key(place: &Place) -> Option<String> {
    match place {
        Place::Global => None,
        Place::Project(id) => Some(crate::memory::safe(id)).filter(|k| !k.is_empty()),
        Place::Folder(_) => place.rel_dir().strip_prefix("folders/").map(str::to_string),
    }
}

/// A key as the app sends it: letters, digits, `-` and `_` only (never a path).
fn checked_key(key: &str) -> AppResult<&str> {
    let ok = !key.is_empty() && key.len() <= 200 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(key) } else { Err(AppError::msg(format!("Not a To do list: {key:?}"))) }
}

fn file(key: &str) -> AppResult<PathBuf> {
    Ok(dir()?.join(format!("{}.json", checked_key(key)?)))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn label(place: &Place) -> String {
    place.label(&crate::memory::projects())
}

fn empty(key: &str, place: &Place) -> TodoList {
    TodoList { key: key.to_string(), place: place.clone(), label: label(place), updated_at: String::new(), items: vec![] }
}

/// The list in `path`: none when there's no file, an error when it isn't a list.
fn read_file(path: &Path) -> AppResult<Option<TodoList>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| AppError::msg(format!("{} isn't a valid To do list ({e})", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A place's list (empty when it has none yet).
pub fn load(place: &Place) -> AppResult<TodoList> {
    let key = key(place).ok_or_else(|| AppError::msg("To do lists belong to a project"))?;
    let mut list = read_file(&file(&key)?)?.unwrap_or_else(|| empty(&key, place));
    list.label = label(&list.place);
    Ok(list)
}

/// The list stored under `key` (the app's sidebar).
pub fn load_key(key: &str) -> AppResult<TodoList> {
    let mut list = read_file(&file(key)?)?.ok_or_else(|| AppError::msg("That To do list is gone"))?;
    list.label = label(&list.place);
    Ok(list)
}

fn save(list: &TodoList) -> AppResult<()> {
    crate::memory::write_atomic(&file(&list.key)?, &serde_json::to_vec_pretty(list)?)
}

/// Moves a file that isn't a list aside so a write doesn't lose it; says where.
fn set_aside(path: &Path) -> AppResult<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let to = path.with_file_name(format!("{stem}.bad-{stamp}.json"));
    std::fs::rename(path, &to)?;
    Ok(to)
}

// ---------- changes

/// What a write does.
#[derive(Debug, Clone)]
pub enum Edit {
    /// The list becomes these items (an id it had keeps that item's other fields).
    Replace(Vec<Change>),
    /// Add or change these items, then remove the ids in `remove`.
    Apply { changes: Vec<Change>, remove: Vec<String> },
}

/// What a write did.
#[derive(Debug, Clone)]
pub struct Written {
    pub list: TodoList,
    /// Ids of the items it added or changed, in the list's order.
    pub changed: Vec<String>,
    /// Ids it took off the list: asked for, left out of a replace, or done items beyond the limit.
    pub removed: Vec<String>,
    /// Ids it removed that the list didn't have.
    pub unknown: Vec<String>,
    /// Where an unreadable file went.
    pub set_aside: Option<PathBuf>,
}

/// Changes a place's list for `origin` (the agent, or `user` from the app) and tells the windows.
pub fn write(place: &Place, edit: Edit, origin: &str) -> AppResult<Written> {
    let key = key(place).ok_or_else(|| AppError::msg("To do lists belong to a project: this folder isn't one DevCat knows"))?;
    write_at(&key, place, edit, origin)
}

/// The same, for the list stored under `key` (the app's sidebar: tick, move, remove).
pub fn write_key(key: &str, edit: Edit) -> AppResult<TodoList> {
    let place = load_key(key)?.place;
    Ok(write_at(key, &place, edit, "user")?.list)
}

fn write_at(key: &str, place: &Place, edit: Edit, origin: &str) -> AppResult<Written> {
    let _g = LOCK.lock();
    let path = file(key)?;
    let (old, set_aside) = match read_file(&path) {
        Ok(l) => (l.unwrap_or_else(|| empty(key, place)), None),
        Err(_) => (empty(key, place), Some(set_aside(&path)?)),
    };
    let before = old.items.clone();
    let (mut list, unknown) = edited(old, edit, origin, &now())?;
    list.key = key.to_string();
    list.place = place.clone();
    list.label = label(place);
    save(&list)?;
    crate::events::emit(EVENT, serde_json::json!({ "key": key }));
    let changed = list.items.iter().filter(|i| before.iter().find(|b| b.id == i.id) != Some(*i)).map(|i| i.id.clone()).collect();
    let removed = before.iter().filter(|b| !list.items.iter().any(|i| i.id == b.id)).map(|b| b.id.clone()).collect();
    Ok(Written { list, changed, removed, unknown, set_aside })
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
}

/// Some(trimmed) or None for an empty value.
fn opt(s: &Option<String>, max: usize) -> Option<Option<String>> {
    s.as_ref().map(|v| Some(clip(v, max)).filter(|v| !v.is_empty()))
}

fn links(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for l in v.iter().map(|l| clip(l, MAX_LINK)).filter(|l| !l.is_empty()) {
        if !out.contains(&l) && out.len() < MAX_LINKS {
            out.push(l);
        }
    }
    out
}

/// The next free id (`t1`, `t2`, …).
fn next_id(items: &[Item]) -> String {
    let n = items.iter().filter_map(|i| i.id.strip_prefix('t').and_then(|n| n.parse::<u64>().ok())).max().unwrap_or(0);
    format!("t{}", n + 1)
}

/// `item` with `c`'s fields; its time moves only when something changed.
fn merged(mut item: Item, c: &Change, at: &str) -> AppResult<Item> {
    let before = item.clone();
    if let Some(t) = &c.text {
        let t = clip(t, MAX_TEXT);
        if t.is_empty() {
            return Err(AppError::msg(format!("Item {} needs text", item.id)));
        }
        item.text = t;
    }
    if let Some(g) = c.group {
        item.group = g;
    }
    if let Some(w) = opt(&c.waiting_on, MAX_WAITING) {
        item.waiting_on = w;
    }
    if let Some(o) = opt(&c.owner, MAX_OWNER) {
        item.owner = o;
    }
    if let Some(l) = &c.links {
        item.links = links(l);
    }
    if item != before {
        item.updated_at = at.to_string();
    }
    Ok(item)
}

fn created(id: String, c: &Change, origin: &str, at: &str) -> AppResult<Item> {
    let text = c.text.as_deref().map(|t| clip(t, MAX_TEXT)).unwrap_or_default();
    if text.is_empty() {
        return Err(AppError::msg(match &c.id {
            Some(id) => format!("There's no item {id}: a new item needs `text`"),
            None => "A new item needs `text`".into(),
        }));
    }
    let owner = match opt(&c.owner, MAX_OWNER) {
        Some(o) => o,
        // the agent that added it, unless the user did
        None => Some(origin.to_string()).filter(|o| !o.is_empty() && o != "user"),
    };
    Ok(Item {
        id,
        text,
        group: c.group.unwrap_or(Group::Next),
        waiting_on: opt(&c.waiting_on, MAX_WAITING).flatten(),
        links: c.links.as_deref().map(links).unwrap_or_default(),
        owner,
        updated_at: at.to_string(),
    })
}

/// Adds or changes one item in `items`.
fn upsert(items: &mut Vec<Item>, c: &Change, origin: &str, at: &str) -> AppResult<()> {
    let id = c.id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| clip(s, 64));
    match id.as_ref().and_then(|id| items.iter().position(|i| &i.id == id)) {
        Some(at_ix) => items[at_ix] = merged(items[at_ix].clone(), c, at)?,
        None => {
            let id = id.unwrap_or_else(|| next_id(items));
            let item = created(id, c, origin, at)?;
            items.push(item);
        }
    }
    Ok(())
}

/// `list` after `edit` (pure, for the tests), and the removed ids it didn't have.
pub fn edited(mut list: TodoList, edit: Edit, origin: &str, at: &str) -> AppResult<(TodoList, Vec<String>)> {
    let mut unknown = vec![];
    match edit {
        Edit::Replace(changes) => {
            let old = std::mem::take(&mut list.items);
            for c in &changes {
                let id = c.id.as_deref().map(str::trim).unwrap_or("");
                // an item it had (by id) keeps what this change leaves out
                if let Some(prev) = old.iter().find(|i| !id.is_empty() && i.id == id)
                    && !list.items.iter().any(|i| i.id == id)
                {
                    list.items.push(prev.clone());
                }
                upsert(&mut list.items, c, origin, at)?;
            }
        }
        Edit::Apply { changes, remove } => {
            for c in &changes {
                upsert(&mut list.items, c, origin, at)?;
            }
            for id in remove.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let before = list.items.len();
                list.items.retain(|i| i.id != id);
                if list.items.len() == before {
                    unknown.push(id.to_string());
                }
            }
        }
    }
    prune(&mut list.items);
    if list.items.len() > MAX_ITEMS {
        return Err(AppError::msg(format!("A To do list holds at most {MAX_ITEMS} items: remove some, or mark them done")));
    }
    list.updated_at = at.to_string();
    Ok((list, unknown))
}

/// Keeps the newest [`MAX_DONE`] done items.
fn prune(items: &mut Vec<Item>) {
    let mut done: Vec<(String, String)> = items.iter().filter(|i| i.group == Group::Done).map(|i| (i.updated_at.clone(), i.id.clone())).collect();
    if done.len() <= MAX_DONE {
        return;
    }
    done.sort();
    let drop: Vec<String> = done[..done.len() - MAX_DONE].iter().map(|(_, id)| id.clone()).collect();
    items.retain(|i| !drop.contains(&i.id));
}

// ---------- the app's commands

/// Every list there is, newest first.
#[cfg_attr(feature = "gui", tauri::command)]
pub async fn todos_lists() -> AppResult<Vec<Summary>> {
    let Ok(dir) = dir() else { return Ok(vec![]) };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(vec![]) };
    let projects = crate::memory::projects();
    let mut out: Vec<Summary> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.') || n.to_string_lossy().contains(".bad-")))
        .filter_map(|p| read_file(&p).ok().flatten())
        .map(|l| Summary { label: l.place.label(&projects), open: l.items.iter().filter(|i| i.group != Group::Done).count(), key: l.key, place: l.place, updated_at: l.updated_at })
        .collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(out)
}

/// The list stored under `key`; for a project, its list (empty when it has none yet).
#[cfg_attr(feature = "gui", tauri::command)]
pub async fn todos_get(key: Option<String>, project_id: Option<String>) -> AppResult<TodoList> {
    match (key, project_id) {
        (_, Some(id)) => load(&Place::Project(id)),
        (Some(k), None) => load_key(&k),
        (None, None) => Err(AppError::msg("Which To do list?")),
    }
}

/// The sidebar's changes (tick done, move, remove) to the list under `key`.
#[cfg_attr(feature = "gui", tauri::command)]
pub async fn todos_edit(key: String, changes: Vec<Change>, remove: Vec<String>) -> AppResult<TodoList> {
    write_key(&key, Edit::Apply { changes, remove })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn data() {
        let d = crate::memory::tests::data();
        init(d);
    }

    fn change(v: serde_json::Value) -> Change {
        serde_json::from_value(v).unwrap()
    }

    fn list() -> TodoList {
        empty("k", &Place::Folder("/x".into()))
    }

    fn apply(l: TodoList, changes: Vec<Change>, remove: &[&str]) -> TodoList {
        edited(l, Edit::Apply { changes, remove: remove.iter().map(|s| s.to_string()).collect() }, "codex", "2026-11-25T01:00:00Z").unwrap().0
    }

    #[test]
    fn upserts_moves_and_removes() {
        let l = apply(list(), vec![change(serde_json::json!({ "text": "Port the API", "group": "running" })), change(serde_json::json!({ "text": " Write tests " }))], &[]);
        assert_eq!(l.items.iter().map(|i| (i.id.as_str(), i.text.as_str(), i.group)).collect::<Vec<_>>(), [("t1", "Port the API", Group::Running), ("t2", "Write tests", Group::Next)]);
        assert_eq!(l.items[0].owner.as_deref(), Some("codex"), "the agent that added it");
        // move, with who it waits on and a link; the other item's time doesn't move
        let l = edited(l, Edit::Apply { changes: vec![change(serde_json::json!({ "id": "t2", "group": "waiting_on_others", "waitingOn": "Pete's review", "links": ["https://x/pr/1", "https://x/pr/1", " "] }))], remove: vec![] }, "codex", "2026-11-25T02:00:00Z").unwrap().0;
        let t2 = &l.items[1];
        assert_eq!((t2.group, t2.waiting_on.as_deref(), t2.links.clone(), t2.updated_at.as_str()), (Group::WaitingOnOthers, Some("Pete's review"), vec!["https://x/pr/1".to_string()], "2026-11-25T02:00:00Z"));
        assert_eq!(l.items[0].updated_at, "2026-11-25T01:00:00Z");
        // an empty value clears it
        let l = apply(l, vec![change(serde_json::json!({ "id": "t2", "waitingOn": "" }))], &[]);
        assert_eq!(l.items[1].waiting_on, None);
        // remove: a known id goes, an unknown one is reported
        let (l, unknown) = edited(l, Edit::Apply { changes: vec![], remove: vec!["t1".into(), "t9".into()] }, "codex", "x").unwrap();
        assert_eq!((l.items.len(), unknown), (1, vec!["t9".to_string()]));
        assert_eq!(apply(l, vec![change(serde_json::json!({ "text": "n" }))], &[]).items[1].id, "t3", "ids aren't reused");
    }

    #[test]
    fn replace_keeps_what_it_leaves_out() {
        let l = apply(list(), vec![change(serde_json::json!({ "text": "A", "links": ["a"] })), change(serde_json::json!({ "text": "B" }))], &[]);
        let (l, _) = edited(l, Edit::Replace(vec![change(serde_json::json!({ "id": "t1", "group": "done" })), change(serde_json::json!({ "text": "C" }))]), "claude-code", "later").unwrap();
        assert_eq!(l.items.iter().map(|i| (i.id.as_str(), i.text.as_str(), i.group)).collect::<Vec<_>>(), [("t1", "A", Group::Done), ("t2", "C", Group::Next)]);
        assert_eq!(l.items[0].links, vec!["a".to_string()]);
        assert!(edited(l, Edit::Replace(vec![change(serde_json::json!({ "id": "t7" }))]), "x", "y").is_err(), "a new item needs text");
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(edited(list(), Edit::Apply { changes: vec![Change::default()], remove: vec![] }, "x", "y").is_err());
        let l = apply(list(), vec![change(serde_json::json!({ "text": "A" }))], &[]);
        assert!(edited(l, Edit::Apply { changes: vec![change(serde_json::json!({ "id": "t1", "text": "  " }))], remove: vec![] }, "x", "y").is_err());
        let many: Vec<Change> = (0..=MAX_ITEMS).map(|n| change(serde_json::json!({ "text": format!("item {n}") }))).collect();
        assert!(edited(list(), Edit::Apply { changes: many, remove: vec![] }, "x", "y").is_err());
        assert!(checked_key("../etc").is_err() && checked_key("").is_err() && checked_key("abc-12_x").is_ok());
        assert_eq!(Group::parse("Waiting on you"), Some(Group::WaitingOnYou));
        assert_eq!(Group::parse("in-progress"), Some(Group::Running));
        assert_eq!(Group::parse("later"), None);
    }

    #[test]
    fn keeps_the_newest_done_items() {
        let mut l = list();
        for n in 0..MAX_DONE + 5 {
            l = edited(l, Edit::Apply { changes: vec![change(serde_json::json!({ "text": format!("d{n}"), "group": "done" }))], remove: vec![] }, "x", &format!("2026-11-25T00:{n:02}:00Z")).unwrap().0;
        }
        assert_eq!(l.items.len(), MAX_DONE);
        assert_eq!(l.items[0].text, "d5", "the oldest went");
    }

    #[tokio::test]
    async fn saves_loads_and_survives_a_bad_file() {
        data();
        let place = Place::Project(format!("todo-test-{}", uuid::Uuid::new_v4()));
        let k = key(&place).unwrap();
        assert!(load(&place).unwrap().items.is_empty(), "no list yet");
        let w = write(&place, Edit::Apply { changes: vec![change(serde_json::json!({ "text": "Ship it", "group": "running" }))], remove: vec![] }, "codex").unwrap();
        assert!(w.set_aside.is_none());
        assert_eq!((w.changed, w.removed.len()), (vec!["t1".to_string()], 0), "a write says what it touched");
        let more = Edit::Apply { changes: vec![change(serde_json::json!({ "text": "Later" })), change(serde_json::json!({ "id": "t1", "group": "running" }))], remove: vec![] };
        assert_eq!(write(&place, more, "codex").unwrap().changed, vec!["t2".to_string()], "an item given as it already is didn't change");
        let w = write(&place, Edit::Apply { changes: vec![], remove: vec!["t2".into(), "t9".into()] }, "codex").unwrap();
        assert_eq!((w.changed.len(), w.removed, w.unknown), (0, vec!["t2".to_string()], vec!["t9".to_string()]));
        let path = file(&k).unwrap();
        // written whole: no temp file left next to it
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.contains(&k)).collect();
        assert_eq!(names, vec![format!("{k}.json")]);
        let again = load(&place).unwrap();
        assert_eq!((again.items.len(), again.items[0].text.as_str(), again.key.as_str()), (1, "Ship it", k.as_str()));
        assert_eq!(load_key(&k).unwrap().items, again.items);
        assert!(todos_lists().await.unwrap().iter().any(|s| s.key == k && s.open == 1));

        // the app's tick
        let ticked = write_key(&k, Edit::Apply { changes: vec![change(serde_json::json!({ "id": "t1", "group": "done" }))], remove: vec![] }).unwrap();
        assert_eq!(ticked.items[0].group, Group::Done);
        // the app writes the file it read, whatever the key
        let other = format!("{k}-copy");
        std::fs::copy(&path, file(&other).unwrap()).unwrap();
        let moved = write_key(&other, Edit::Apply { changes: vec![change(serde_json::json!({ "id": "t1", "group": "next" }))], remove: vec![] }).unwrap();
        assert_eq!((moved.key.as_str(), moved.items[0].group), (other.as_str(), Group::Next));
        assert_eq!(load(&place).unwrap().items[0].group, Group::Done, "the place's own list is untouched");
        let _ = std::fs::remove_file(file(&other).unwrap());

        // a file that isn't JSON: loading says so, the next write moves it aside
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load(&place).is_err());
        let w = write(&place, Edit::Apply { changes: vec![change(serde_json::json!({ "text": "Fresh" }))], remove: vec![] }, "codex").unwrap();
        let aside = w.set_aside.expect("moved aside");
        assert_eq!(std::fs::read(&aside).unwrap(), b"{ not json");
        assert_eq!(load(&place).unwrap().items.len(), 1);
        assert!(!todos_lists().await.unwrap().iter().any(|s| s.key.contains(".bad-")));
        assert!(key(&Place::Global).is_none());
        let _ = std::fs::remove_file(aside);
        let _ = std::fs::remove_file(path);
    }
}
