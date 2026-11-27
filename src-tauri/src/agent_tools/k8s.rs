//! Kubernetes tools beyond `k8s_get` / `k8s_logs` (in `mod.rs`): an object's events, the
//! cluster's overview and pod / node metrics, Helm releases, and changes (scale, restart, suspend
//! or resume a CronJob, apply YAML). Helm values hide secret-looking keys, and the rendered
//! manifest (which holds Secrets) isn't shown. Changes need the connection's AI access at Full
//! over MCP; the DevCat Agent asks first. Deleting can't be undone, so `k8s_delete` is the DevCat
//! Agent's only, confirmed by typing the name (`Tool::confirm`). Port-forwards are network
//! profiles (`net_profile_save` with kind k8s).
use super::*;
use crate::infra::k8s::{self as kube_infra, helm};

pub(super) fn add(out: &mut Vec<Arc<dyn Tool>>, s: &Arc<Scope>) {
    out.push(Arc::new(K8sEvents(s.clone())));
    out.push(Arc::new(K8sOverview(s.clone())));
    out.push(Arc::new(K8sHelm(s.clone())));
    out.push(Arc::new(K8sAction(s.clone())));
    out.push(Arc::new(K8sApply(s.clone())));
    // deleting can't be undone: the user types the name, which only the app can ask (not over MCP)
    if !s.mcp {
        out.push(Arc::new(K8sDelete(s.clone())));
    }
}

fn ns_of(input: &Value) -> Option<String> {
    arg(input, "namespace")
}

// ---------- k8s_events

struct K8sEvents(Arc<Scope>);

#[async_trait]
impl Tool for K8sEvents {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_events".into(),
            description: "The events of one Kubernetes object in a DevCat cluster connection (why a pod is Pending or restarting, failed mounts, image pulls, scheduling, probes), newest last. For a namespace's events at large, use `k8s_get` with kind events.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "kind": { "type": "string", "description": "e.g. pods, deployments" },
                    "namespace": { "type": "string" },
                    "name": { "type": "string" }
                },
                "required": ["connection", "kind", "name"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Query
    }
    fn summary(&self, input: &Value) -> String {
        format!("{}: {} {}", conn_summary(input), arg(input, "kind").unwrap_or_default(), arg(input, "name").unwrap_or_default())
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let c = match k8s_conn(&self.0, &input) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let (Some(kind), Some(name)) = (arg(&input, "kind"), arg(&input, "name")) else { return ToolOutput::err("Missing `kind` or `name`.") };
        match kube_infra::k8s_events_for(St::get(), c.id.clone(), kind, ns_of(&input), name).await {
            Ok(evs) if evs.is_empty() => ToolOutput::ok("No events (Kubernetes keeps them for about an hour)."),
            Ok(evs) => {
                let lines: Vec<String> = evs
                    .iter()
                    .map(|e| {
                        let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
                        let when = e["lastTimestamp"].as_str().or(e["eventTime"].as_str()).unwrap_or("");
                        let count = e["count"].as_u64().filter(|n| *n > 1).map(|n| format!(" ×{n}")).unwrap_or_default();
                        format!("{when} {} {}{count}: {}", s("type"), s("reason"), s("message"))
                    })
                    .collect();
                ToolOutput::ok(lines.join("\n"))
            }
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

// ---------- k8s_overview

struct K8sOverview(Arc<Scope>);

#[async_trait]
impl Tool for K8sOverview {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_overview".into(),
            description: "A DevCat cluster connection at a glance: Kubernetes version, nodes ready, pods by phase, restarts, deployments ready, capacity and the newest warning events, one line each. With `metrics` = pods or nodes: CPU and memory use from metrics-server (pods of `namespace`, or all).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "metrics": { "type": "string", "enum": ["pods", "nodes"] },
                    "namespace": { "type": "string" }
                },
                "required": ["connection"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Query
    }
    fn summary(&self, input: &Value) -> String {
        format!("{}{}", conn_summary(input), arg(input, "metrics").map(|m| format!(": {m} metrics")).unwrap_or_default())
    }
    fn timeout(&self, _: &Value) -> Duration {
        Duration::from_secs(90)
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let c = match k8s_conn(&self.0, &input) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if let Some(kind) = arg(&input, "metrics") {
            return match kube_infra::k8s_metrics(St::get(), c.id.clone(), kind, ns_of(&input)).await {
                Ok(m) => ToolOutput::ok(serde_json::to_string_pretty(&m).unwrap_or_default()),
                Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
            };
        }
        match kube_infra::k8s_cluster_overview(St::get(), c.id.clone()).await {
            Ok(o) => {
                let mut v = serde_json::to_value(&o).unwrap_or_default();
                brief_warnings(&mut v);
                ToolOutput::ok(pretty(&v))
            }
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

/// Warning events an overview shows.
const WARNINGS_SHOWN: usize = 15;

/// The overview's warning events (the app's dashboard gets up to 100 whole Event objects, which
/// were most of this tool's answer) as one line each, the newest [`WARNINGS_SHOWN`], and how many
/// more there are.
fn brief_warnings(overview: &mut Value) {
    let Some(all) = overview.get("warnings").and_then(Value::as_array).cloned() else { return };
    overview["warnings"] = all.iter().take(WARNINGS_SHOWN).map(|e| Value::String(warning_line(e))).collect();
    if all.len() > WARNINGS_SHOWN {
        overview["warningsNotShown"] = json!(format!("{} older ones: `k8s_get` with kind events lists a namespace's, `k8s_events` one object's", all.len() - WARNINGS_SHOWN));
    }
}

/// One warning event: when, what it is about, the reason and the start of the message (whichever
/// of them the event has).
fn warning_line(e: &Value) -> String {
    let about = &e["involvedObject"];
    let s = |v: &Value| v.as_str().unwrap_or_default().trim().to_string();
    let when = [&e["lastTimestamp"], &e["eventTime"], &e["metadata"]["creationTimestamp"]].into_iter().find_map(Value::as_str).unwrap_or_default().to_string();
    let name = match (s(&about["namespace"]), s(&about["name"])) {
        (ns, name) if !ns.is_empty() && !name.is_empty() => format!("{ns}/{name}"),
        (ns, name) => format!("{ns}{name}"),
    };
    let object = [s(&about["kind"]), name].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
    let message: String = s(&e["message"]).split_whitespace().collect::<Vec<_>>().join(" ");
    let message = if message.chars().count() > 240 { format!("{}…", message.chars().take(240).collect::<String>()) } else { message };
    let times = e["count"].as_u64().filter(|n| *n > 1).map(|n| format!(" (x{n})")).unwrap_or_default();
    let what = [s(&e["reason"]), message].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(": ");
    format!("{}{times}", [when, object, what].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("  "))
}

// ---------- k8s_helm

struct K8sHelm(Arc<Scope>);

#[async_trait]
impl Tool for K8sHelm {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_helm".into(),
            description: "Helm releases in a DevCat cluster connection (read from the cluster, no helm binary needed). Without `name`: every release of `namespace` (or all) with chart, app version, status and revision. With `name` and `namespace`: that release's summary, the values it was installed with (secret-looking keys hidden), its notes and revision history; `revision` for an older one.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "namespace": { "type": "string" },
                    "name": { "type": "string" },
                    "revision": { "type": "integer" }
                },
                "required": ["connection"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Query
    }
    fn summary(&self, input: &Value) -> String {
        format!("{}: {}", conn_summary(input), arg(input, "name").unwrap_or_else(|| "releases".into()))
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let c = match k8s_conn(&self.0, &input) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let Some(name) = arg(&input, "name") else {
            return match helm::k8s_helm_releases(St::get(), c.id.clone(), ns_of(&input)).await {
                Ok(r) if r.is_empty() => ToolOutput::ok("No Helm releases."),
                Ok(r) => ToolOutput::ok(r.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n")),
                Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
            };
        };
        let Some(ns) = ns_of(&input) else { return ToolOutput::err("Give the release's `namespace` too.") };
        match helm::k8s_helm_release(St::get(), c.id.clone(), ns, name, input.get("revision").and_then(Value::as_i64)).await {
            Ok(mut v) => {
                if let Some(m) = v.as_object_mut() {
                    // the rendered manifest holds the release's Secrets; chart defaults are noise
                    m.remove("manifest");
                    m.remove("defaultValues");
                }
                redact_secrets(&mut v);
                ToolOutput::ok(pretty(&v))
            }
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

// ---------- k8s_action

struct K8sAction(Arc<Scope>);

#[async_trait]
impl Tool for K8sAction {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_action".into(),
            description: "Change a Kubernetes object in a DevCat cluster connection: `action` = scale (a deployment / statefulset / replicaset to `replicas`), restart (a rollout restart of a deployment / statefulset / daemonset), suspend or resume (a CronJob). Deleting is `k8s_delete`, in the DevCat Agent only.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "action": { "type": "string", "enum": ["scale", "restart", "suspend", "resume"] },
                    "kind": { "type": "string", "description": "e.g. deployments, statefulsets, cronjobs" },
                    "namespace": { "type": "string" },
                    "name": { "type": "string" },
                    "replicas": { "type": "integer" }
                },
                "required": ["connection", "action", "kind", "name"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Exec
    }
    fn summary(&self, input: &Value) -> String {
        let n = input.get("replicas").and_then(Value::as_i64).map(|r| format!(" → {r}")).unwrap_or_default();
        format!("{}: {} {} {}{n}", conn_summary(input), arg(input, "action").unwrap_or_default(), arg(input, "kind").unwrap_or_default(), arg(input, "name").unwrap_or_default())
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let c = match k8s_conn(&self.0, &input).and_then(|c| may_change(&self.0, &c).map(|_| c)) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let (Some(action), Some(kind), Some(name)) = (arg(&input, "action"), arg(&input, "kind"), arg(&input, "name")) else {
            return ToolOutput::err("Missing `action`, `kind` or `name`.");
        };
        let (st, id, ns) = (St::get(), c.id.clone(), ns_of(&input));
        let res = match action.as_str() {
            "scale" => match input.get("replicas").and_then(Value::as_i64) {
                Some(r) if (0..=10_000).contains(&r) => kube_infra::k8s_scale(st, id, kind.clone(), ns, name.clone(), r as i32).await,
                _ => return ToolOutput::err("`scale` needs `replicas` (0 or more)."),
            },
            "restart" => kube_infra::k8s_restart(st, id, kind.clone(), ns, name.clone()).await,
            "suspend" | "resume" => match ns {
                Some(ns) => kube_infra::k8s_cronjob_suspend(st, id, ns, name.clone(), action == "suspend").await,
                None => return ToolOutput::err("Give the CronJob's `namespace`."),
            },
            "delete" => return ToolOutput::err("Deleting is `k8s_delete` (DevCat Agent only: the user confirms it by typing the name)."),
            other => return ToolOutput::err(format!("Unknown `action` \"{other}\": scale, restart, suspend or resume.")),
        };
        match res {
            Ok(()) => ToolOutput::ok(format!("Done: {action} {kind} {name}.")),
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

// ---------- k8s_delete

struct K8sDelete(Arc<Scope>);

#[async_trait]
impl Tool for K8sDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_delete".into(),
            description: "Delete a Kubernetes object in a DevCat cluster connection (`force` for a pod stuck terminating). It can't be undone: only when the user asked to delete that object, say which one first. DevCat asks the user to confirm by typing its name.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "kind": { "type": "string", "description": "e.g. pods, deployments, jobs" },
                    "namespace": { "type": "string" },
                    "name": { "type": "string" },
                    "force": { "type": "boolean" }
                },
                "required": ["connection", "kind", "name"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Exec
    }
    fn summary(&self, input: &Value) -> String {
        format!("{}: delete {} {}", conn_summary(input), arg(input, "kind").unwrap_or_default(), arg(input, "name").unwrap_or_default())
    }
    fn confirm(&self, input: &Value) -> Option<String> {
        Some(arg(input, "name").unwrap_or_else(|| "(no name)".into()))
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        if self.0.mcp {
            return ToolOutput::err("Kubernetes objects are deleted from the DevCat Agent in DevCat only, where the user confirms it.");
        }
        let c = match k8s_conn(&self.0, &input).and_then(|c| may_change(&self.0, &c).map(|_| c)) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let (Some(kind), Some(name)) = (arg(&input, "kind"), arg(&input, "name")) else { return ToolOutput::err("Missing `kind` or `name`.") };
        match kube_infra::k8s_delete(St::get(), c.id.clone(), kind.clone(), ns_of(&input), name.clone(), arg_bool(&input, "force")).await {
            Ok(()) => ToolOutput::ok(format!("Deleted {kind} {name}.")),
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

// ---------- k8s_apply

struct K8sApply(Arc<Scope>);

#[async_trait]
impl Tool for K8sApply {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "k8s_apply".into(),
            description: "Apply Kubernetes YAML (one or more documents separated by ---) to a DevCat cluster connection, like `kubectl apply` (server-side apply: objects are created or updated). Each object needs its namespace unless it's cluster-wide. Reports what was created or changed.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Cluster connection name or id" },
                    "yaml": { "type": "string" }
                },
                "required": ["connection", "yaml"]
            }),
        }
    }
    fn kind(&self, _: &Value, _: &ToolCtx) -> Kind {
        Kind::Exec
    }
    fn summary(&self, input: &Value) -> String {
        let yaml = input.get("yaml").and_then(Value::as_str).unwrap_or("");
        let kinds: Vec<&str> = yaml.lines().filter_map(|l| l.strip_prefix("kind:")).map(str::trim).collect();
        format!("{}: apply {}", conn_summary(input), if kinds.is_empty() { "YAML".to_string() } else { kinds.join(", ") })
    }
    fn timeout(&self, _: &Value) -> Duration {
        Duration::from_secs(120)
    }
    async fn run(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let c = match k8s_conn(&self.0, &input).and_then(|c| may_change(&self.0, &c).map(|_| c)) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let Some(yaml) = input.get("yaml").and_then(Value::as_str).filter(|y| !y.trim().is_empty()) else { return ToolOutput::err("Missing `yaml`.") };
        match kube_infra::k8s_apply_yaml(St::get(), c.id.clone(), yaml.to_string()).await {
            Ok(out) => ToolOutput::ok(out),
            Err(e) => ToolOutput::err(connect_error(&self.0, &c, &e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overviews_warnings_are_one_line_each() {
        let event = |n: usize| json!({ "involvedObject": { "kind": "Pod", "namespace": "shop", "name": format!("api-{n}") }, "reason": "BackOff", "message": "Back-off restarting\n  failed container", "count": n, "lastTimestamp": "2026-11-28T00:00:00Z", "metadata": { "name": "e", "managedFields": [] } });
        let mut o = json!({ "version": "v1.31", "warnings": (1..=40).map(event).collect::<Vec<_>>() });
        brief_warnings(&mut o);
        let lines = o["warnings"].as_array().unwrap();
        assert_eq!(lines.len(), WARNINGS_SHOWN);
        assert_eq!(lines[0], "2026-11-28T00:00:00Z  Pod shop/api-1  BackOff: Back-off restarting failed container");
        assert_eq!(lines[1], "2026-11-28T00:00:00Z  Pod shop/api-2  BackOff: Back-off restarting failed container (x2)");
        assert!(o["warningsNotShown"].as_str().unwrap().starts_with("25 older ones"), "{o}");
        assert_eq!(warning_line(&json!({ "message": "only this", "involvedObject": { "name": "node-1" } })), "node-1  only this");
        assert_eq!(warning_line(&json!({})), "");
        let mut few = json!({ "warnings": [event(1)] });
        brief_warnings(&mut few);
        assert!(few.get("warningsNotShown").is_none() && few["warnings"].as_array().unwrap().len() == 1);
    }
}
