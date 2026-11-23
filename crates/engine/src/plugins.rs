//! Plugins: external MCP servers whose tools become Craft Anything tools, each server its own
//! toolset. A server is listed in `config.toml`:
//!
//! ```toml
//! [[plugins.mcp]]
//! id = "notes"                 # toolset id; tool ids become notes_<tool>
//! command = "notes-mcp"
//! args = ["--stdio"]
//! consent = true               # ask before each call (default): it runs someone else's code
//!
//! [[plugins.mcp]]
//! id = "docs"                  # a remote server (Streamable HTTP)
//! url = "https://mcp.example.com/mcp"
//! headers = { Authorization = "Bearer …" }
//! ```
//!
//! Its tool list is cached (`plugins_refresh`), so starting the app or a CLI command never starts
//! every server; a server starts on its first call and stays up for the session. Results are
//! recorded as evidence from an external service. Effects come from the server's own tool
//! annotations, plus launching a process (local) or using the network (remote); `consent` adds
//! "runs foreign code", which asks first. `env` and `headers` values are never shown by
//! `settings_show` or `plugins_list`.

use crate::registry::{RuntimeTool, custom_toolset};
use crate::session::Session;
use crate::tool::{Ctx, Effects, Requires, ToolOutput};
use craft_anything_engines::mcp_client::{McpClient, RemoteTool, Server, ServerCommand, ServerUrl, content_text, url_allowed};
use craft_anything_evidence::{Authority, Confidence, Environment, ErrorCode, EvidenceBuilder, Subject, ToolError};
use craft_anything_platform::sync::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// How long a server may take to start and answer the handshake or its tool list.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Most tools one server may add.
const MAX_TOOLS: usize = 500;

/// One configured MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPlugin {
    /// Toolset id and tool-id prefix: lower-case letters, digits and `_`.
    pub id: String,
    /// The name people see (default: the id).
    #[serde(default)]
    pub name: Option<String>,
    /// The program of a local server (or `url` for a remote one).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// A remote server's address: `https://…`, or `http://` to this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Sent with every request to a remote server (e.g. `Authorization`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Ask before each call (it runs someone else's code). Default true.
    #[serde(default = "yes")]
    pub consent: bool,
    /// A module: shown as its own section of the app, whose tools are read live from the running
    /// server (so tools it adds while the app is open appear at once), besides the cached ones.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub module: bool,
    /// Files or folders the server is made from. When one is newer than the cached tool list, the
    /// list is read again at start, so a changed server's tools are offered without a manual refresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch: Vec<PathBuf>,
    /// How long one call may take, in seconds (default 15 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_timeout_secs: Option<u64>,
}

fn yes() -> bool {
    true
}

impl McpPlugin {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// How to reach it: a remote `url`, else the local `command`.
    pub fn server(&self) -> Server {
        match self.remote_url() {
            Some(url) => Server::Url(ServerUrl { url: url.to_string(), headers: self.headers.clone() }),
            None => Server::Command(ServerCommand { command: self.command.clone(), args: self.args.clone(), env: self.env.clone(), cwd: self.cwd.clone() }),
        }
    }

    fn remote_url(&self) -> Option<&str> {
        self.url.as_deref().map(str::trim).filter(|u| !u.is_empty())
    }

    /// What's wrong with it, if anything: its id, or not exactly one of `command` and `url`.
    pub fn problem(&self) -> Option<String> {
        if !RuntimeTool::valid_id(&self.id) || custom_toolset(&self.id).is_none() {
            return Some(format!("Plugin id {:?} must be lower-case letters, digits and _.", self.id));
        }
        match (self.command.trim().is_empty(), self.remote_url()) {
            (true, None) => Some(format!("The {} plugin needs a command (a local server) or a url (a remote one).", self.id)),
            (false, Some(_)) => Some(format!("The {} plugin has both a command and a url; keep one.", self.id)),
            (true, Some(u)) if !url_allowed(u) => Some(format!("The {} plugin's url must start with https:// (or http:// to this computer).", self.id)),
            _ => None,
        }
    }

    /// Where it runs, for people: the url, or the command line.
    pub fn place(&self) -> String {
        match self.remote_url() {
            Some(u) => u.to_string(),
            None => std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        }
    }

    /// Where its tool list is cached, under a config folder ([`Session::config_dir`]).
    pub fn cache_path(&self, config: &std::path::Path) -> PathBuf {
        config.join("plugins").join(format!("{}.tools.json", self.id))
    }

    /// The tools cached by the last refresh (none before the first).
    pub fn cached_tools(&self, config: &std::path::Path) -> Vec<RemoteTool> {
        std::fs::read(self.cache_path(config)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// The newest modification time among the watched files (folders are walked, a few levels
    /// deep, skipping hidden entries and build output).
    pub fn watched_mtime(&self) -> Option<std::time::SystemTime> {
        fn newest(path: &std::path::Path, depth: u32, best: &mut Option<std::time::SystemTime>, seen: &mut u32) {
            if *seen > 20_000 {
                return;
            }
            *seen += 1;
            let Ok(meta) = std::fs::metadata(path) else { return };
            if meta.is_dir() {
                if depth == 0 {
                    return;
                }
                let Ok(entries) = std::fs::read_dir(path) else { return };
                for e in entries.flatten() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') || ["target", "node_modules", "__pycache__", "build", "dist"].contains(&name.as_ref()) {
                        continue;
                    }
                    newest(&e.path(), depth - 1, best, seen);
                }
            } else if let Ok(m) = meta.modified()
                && best.is_none_or(|b| m > b)
            {
                *best = Some(m);
            }
        }
        let mut best = None;
        let mut seen = 0;
        for w in &self.watch {
            newest(w, 4, &mut best, &mut seen);
        }
        best
    }

    /// The cached tool list is missing or older than a watched file.
    pub fn cache_is_stale(&self, config: &std::path::Path) -> bool {
        if self.watch.is_empty() && !self.module {
            return false;
        }
        let Ok(cached) = std::fs::metadata(self.cache_path(config)).and_then(|m| m.modified()) else { return true };
        self.watched_mtime().is_some_and(|w| w > cached)
    }

    fn call_timeout(&self) -> Duration {
        self.call_timeout_secs.map_or(CALL_TIMEOUT, Duration::from_secs)
    }
}

/// `[plugins]` in config.toml.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    pub mcp: Vec<McpPlugin>,
    /// WASM component plugins (`crate::wasm_plugins`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wasm: Vec<crate::wasm_plugins::WasmPluginConfig>,
}

/// The live tool list of a module (`<id>_module_tools`).
pub const MODULE_TOOLS: &str = "module_tools";
/// Call any tool of a module by its name there (`<id>_module_call`), including ones added since start.
pub const MODULE_CALL: &str = "module_call";

/// The modules among the configured plugins: (id, name), in config order.
pub fn modules(settings: &PluginSettings) -> Vec<(String, String)> {
    settings.mcp.iter().filter(|p| p.enabled && p.module && p.problem().is_none()).map(|p| (p.id.clone(), p.display_name().to_string())).collect()
}

/// One proxied tool: which server, and the tool's name there.
struct Route {
    server: String,
    remote: String,
}

/// The session's plugins: their settings, running clients and the tool routes.
#[derive(Default)]
pub struct Plugins {
    servers: HashMap<String, (McpPlugin, Mutex<Option<McpClient>>)>,
    routes: HashMap<&'static str, Route>,
}

/// A remote tool name as part of our id: lower case, `_` for anything else, no repeats.
fn id_part(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' };
        if !(c == '_' && out.ends_with('_')) {
            out.push(c);
        }
    }
    out.trim_matches('_').to_string()
}

/// What a remote tool may do, from its annotations (MCP's hints), plus starting a local server
/// or reaching a remote one.
fn effects_of(t: &RemoteTool, consent: bool, remote: bool) -> Effects {
    let hint = |k: &str| t.annotations.get(k).and_then(Value::as_bool);
    let mut e = Effects::MUTATES_SESSION | if remote { Effects::ACCESSES_NETWORK } else { Effects::LAUNCHES_PROCESS };
    if hint("readOnlyHint") != Some(true) {
        e |= Effects::WRITES_FILESYSTEM;
    }
    if hint("destructiveHint") == Some(true) {
        e |= Effects::MAY_DISCARD_DATA;
    }
    if hint("openWorldHint") != Some(false) || remote {
        e |= Effects::ACCESSES_NETWORK;
    }
    if hint("idempotentHint") == Some(true) {
        e |= Effects::IDEMPOTENT;
    }
    if consent {
        e |= Effects::EXECUTES_FOREIGN_CODE;
    }
    e
}

/// Read the configured plugins into `session` and return their (cached) tools for
/// [`crate::Engine::shared_with`]. A plugin with an invalid id, or a tool whose id clashes, is
/// skipped and logged.
pub fn install(session: &mut Session) -> Vec<RuntimeTool> {
    let mut plugins = Plugins::default();
    let mut tools = Vec::new();
    let config = session.config_dir();
    for p in session.settings.plugins.mcp.iter().filter(|p| p.enabled) {
        if let Some(problem) = p.problem() {
            log::error!("plugin skipped: {problem}");
            continue;
        }
        let Some(set) = custom_toolset(&p.id) else { continue };
        let remote = matches!(p.server(), Server::Url(_));
        if p.cache_is_stale(&config) {
            match refresh(p, &config) {
                Ok(t) => log::info!("plugin {}: read {} tools again (its files changed)", p.id, t.len()),
                Err(e) => log::warn!("plugin {}: couldn't read its tools again: {}", p.id, e.message),
            }
        }
        if p.module {
            module_tools(p, set, remote, &mut plugins, &mut tools);
        }
        for t in p.cached_tools(&config).into_iter().take(MAX_TOOLS) {
            let mut id = format!("{}_{}", p.id, id_part(&t.name));
            id.truncate(64);
            if !RuntimeTool::valid_id(&id) || tools.iter().any(|x: &RuntimeTool| x.id == id) {
                log::error!("plugin {} tool {:?} skipped (its id {id:?} is invalid or taken)", p.id, t.name);
                continue;
            }
            let schema = if t.input_schema.is_object() { t.input_schema.clone() } else { json!({"type": "object"}) };
            let leaked: &'static str = Box::leak(id.clone().into_boxed_str());
            plugins.routes.insert(leaked, Route { server: p.id.clone(), remote: t.name.clone() });
            tools.push(RuntimeTool {
                id,
                title: t.title.clone().unwrap_or_else(|| format!("{}: {}", p.display_name(), t.name)),
                summary: t.description.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| format!("{} from the {} plugin.", t.name, p.display_name())),
                beginner: format!("A tool from the {} plugin.", p.display_name()),
                toolset: set,
                effects: effects_of(&t, p.consent, remote),
                requires: Requires::Nothing,
                schema,
                run: call_plugin,
            });
        }
        plugins.servers.insert(p.id.clone(), (p.clone(), Mutex::new(None)));
    }
    session.ext.insert(Arc::new(plugins));
    crate::wasm_plugins::install(session, &mut tools);
    tools
}

/// The two tools that make a plugin a module: its live tool list and a call by name.
fn module_tools(p: &McpPlugin, set: crate::tool::Toolset, remote: bool, plugins: &mut Plugins, tools: &mut Vec<RuntimeTool>) {
    let base = if remote { Effects::ACCESSES_NETWORK } else { Effects::LAUNCHES_PROCESS };
    let foreign = if p.consent { Effects::EXECUTES_FOREIGN_CODE } else { Effects::empty() };
    let specs: [(&str, String, String, Effects, Value, crate::tool::ToolFn); 2] = [
        (
            MODULE_TOOLS,
            format!("{}: its tools now", p.display_name()),
            format!("Read the {} module's tools from the running server, including ones added since the app started.", p.display_name()),
            base | Effects::MUTATES_SESSION | foreign,
            json!({"type": "object", "properties": {}}),
            module_tools_live,
        ),
        (
            MODULE_CALL,
            format!("{}: run a tool", p.display_name()),
            format!("Run one of the {} module's tools by its name there, with its arguments.", p.display_name()),
            base | Effects::MUTATES_SESSION | Effects::WRITES_FILESYSTEM | Effects::ACCESSES_NETWORK | foreign,
            json!({"type": "object", "properties": {"tool": {"type": "string", "description": "The tool's name in the module"}, "arguments": {"type": "object", "description": "Its arguments"}}, "required": ["tool"]}),
            module_call,
        ),
    ];
    for (suffix, title, summary, effects, schema, run) in specs {
        let id = format!("{}_{suffix}", p.id);
        if !RuntimeTool::valid_id(&id) || tools.iter().any(|x| x.id == id) {
            log::error!("module {}: tool id {id:?} is invalid or taken", p.id);
            continue;
        }
        let leaked: &'static str = Box::leak(id.clone().into_boxed_str());
        plugins.routes.insert(leaked, Route { server: p.id.clone(), remote: String::new() });
        tools.push(RuntimeTool {
            id,
            title,
            summary,
            beginner: format!("Part of the {} module.", p.display_name()),
            toolset: set,
            effects,
            requires: Requires::Nothing,
            schema,
            run,
        });
    }
}

/// Run `f` with the server behind `ctx.tool`, starting it if needed; a server that died is started
/// again on the next call.
fn with_server<T>(ctx: &mut Ctx<'_>, f: impl FnOnce(&McpPlugin, &mut McpClient, &Route) -> Result<T, ToolError>) -> Result<(McpPlugin, T), ToolError> {
    let plugins = ctx.session.ext.get::<Plugins>().ok_or_else(|| ToolError::internal("plugins aren't loaded in this session"))?;
    let route = plugins.routes.get(ctx.tool).ok_or_else(|| ToolError::internal(format!("no plugin serves {}", ctx.tool)))?;
    let (plugin, client) = plugins.servers.get(&route.server).ok_or_else(|| ToolError::internal(format!("plugin {} isn't loaded", route.server)))?;
    let mut slot = client.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        ctx.report(None, format!("Starting the {} plugin…", plugin.display_name()));
        *slot = Some(start(plugin, &ctx.session.config_dir())?);
    }
    let Some(c) = slot.as_mut() else { return Err(ToolError::internal("plugin client missing")) };
    match f(plugin, c, route) {
        Ok(v) => Ok((plugin.clone(), v)),
        Err(e) => {
            if e.code == ErrorCode::EngineMissing {
                *slot = None;
            }
            Err(e)
        }
    }
}

fn module_tools_live(ctx: &mut Ctx<'_>, _params: Value) -> Result<ToolOutput, ToolError> {
    let (plugin, tools) = with_server(ctx, |_, c, _| c.list_tools(START_TIMEOUT))?;
    let modified = plugin.watched_mtime().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    Ok(ToolOutput::Value(json!({"module": plugin.id, "name": plugin.display_name(), "tools": tools, "watched_modified": modified})))
}

#[derive(Deserialize)]
struct ModuleCall {
    tool: String,
    #[serde(default)]
    arguments: Value,
}

fn module_call(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let call: ModuleCall = serde_json::from_value(params.clone()).map_err(|e| ToolError::invalid(e.to_string()))?;
    if call.tool.trim().is_empty() {
        return Err(ToolError::invalid("tool is empty"));
    }
    let cancel = ctx.cancel.clone();
    let args = if call.arguments.is_null() { json!({}) } else { call.arguments.clone() };
    let name = call.tool.clone();
    let (plugin, result) = with_server(ctx, move |p, c, _| c.call_tool(&name, args, p.call_timeout(), &move || cancel.is_cancelled()))?;
    record(ctx, &plugin, params, result)
}

/// Start a server, list its tools and cache them. New tools are offered from the next start.
pub fn refresh(p: &McpPlugin, config: &std::path::Path) -> Result<Vec<RemoteTool>, ToolError> {
    if let Some(problem) = p.problem() {
        return Err(ToolError::invalid(problem).with_remediation("Fix it in config.toml ([[plugins.mcp]])."));
    }
    let mut client = start(p, config)?;
    let tools = client.list_tools(START_TIMEOUT).map_err(|e| logged(p, config, client.with_output(e)))?;
    let path = p.cache_path(config);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir.display(), e))?;
    }
    let json = serde_json::to_vec_pretty(&tools).map_err(|e| ToolError::internal(e.to_string()))?;
    crate::project::write_atomic(&path, &json)?;
    Ok(tools)
}

/// Start a plugin's server and complete the handshake, keeping a failure log when it fails.
fn start(p: &McpPlugin, config: &std::path::Path) -> Result<McpClient, ToolError> {
    McpClient::connect(p.display_name(), &p.server(), START_TIMEOUT).map_err(|e| logged(p, config, e))
}

/// A local server that didn't start or answer: write what ran and what it printed to a failure
/// log in the config folder's `logs/` ([`crate::failure_log`]) and name it in the error. Its
/// environment is left out and what looks like a key in its arguments is hidden. A remote server's error already says what it
/// answered, and its address can carry a key, so it gets no log; nor does a stop.
fn logged(p: &McpPlugin, config: &std::path::Path, e: ToolError) -> ToolError {
    let Server::Command(cmd) = p.server() else { return e };
    if e.code == ErrorCode::Cancelled {
        return e;
    }
    // An argument can carry a key where no flag names it (`https://…/mcp?token=…`).
    let command = crate::journal::scrubbed_line(&craft_anything_platform::process::printable_command(std::ffi::OsStr::new(&cmd.command), &cmd.args));
    let detail = |k: &str| e.details.as_ref().and_then(|d| d.get(k)).cloned();
    let report = crate::failure_log::Report {
        what: format!("The {} plugin (an MCP server) didn't start", p.display_name()),
        check: command.clone(),
        command,
        error: Some(e.message.clone()),
        cwd: cmd.cwd.clone(),
        exit_code: detail("exit_code").and_then(|c| c.as_i64()).and_then(|c| i32::try_from(c).ok()),
        stderr: detail("output_tail").and_then(|t| t.as_str().map(|s| s.as_bytes().to_vec())).unwrap_or_default(),
        ..Default::default()
    };
    match crate::failure_log::write(&crate::failure_log::dir_in(config), &report) {
        Ok(path) => {
            let said = e.remediation.clone().map(|r| format!("{r}\n\n")).unwrap_or_default();
            e.with_remediation(format!("{said}The details are in {}", path.display()))
        }
        Err(w) => {
            log::warn!("couldn't write a failure log for the {} plugin: {w}", p.display_name());
            e
        }
    }
}

/// The function behind every proxied tool: route by [`Ctx::tool`], start the server if needed,
/// call it, and record the result as evidence.
fn call_plugin(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let cancel = ctx.cancel.clone();
    let args = if params.is_null() { json!({}) } else { params.clone() };
    let (plugin, result) = with_server(ctx, move |p, c, route| c.call_tool(&route.remote, args, p.call_timeout(), &move || cancel.is_cancelled()))?;
    record(ctx, &plugin, params, result)
}

/// A plugin's answer as evidence from an outside service.
fn record(ctx: &mut Ctx<'_>, plugin: &McpPlugin, params: Value, result: Value) -> Result<ToolOutput, ToolError> {
    let text = content_text(&result);
    let summary: String = if text.is_empty() { format!("{} answered.", plugin.display_name()) } else { text.chars().take(300).collect() };
    let env = Environment {
        clean_room: ctx.session.clean_room(),
        engine: format!("plugin:{}", plugin.id),
        engine_version: None,
        host: craft_anything_evidence::host_triple(),
        language_id: None,
    };
    let e = EvidenceBuilder::new(ctx.tool, Subject { kind: "plugin".into(), sha256: None, name: Some(plugin.display_name().to_string()) }, env)
        .params(params)
        .authority(Authority::OutsideService)
        .confidence(Confidence::Observed)
        .result(result)
        .summary(summary)
        .build()?;
    Ok(ToolOutput::evidence(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_become_our_ids_and_annotations_our_effects() {
        assert_eq!(id_part("Search Notes!"), "search_notes");
        assert_eq!(id_part("get-page__v2"), "get_page_v2");
        let t = |ann: Value| RemoteTool { name: "x".into(), title: None, description: None, input_schema: json!({}), annotations: ann };
        let read = effects_of(&t(json!({"readOnlyHint": true, "openWorldHint": false})), false, false);
        assert!(!read.contains(Effects::WRITES_FILESYSTEM) && !read.contains(Effects::ACCESSES_NETWORK) && !read.needs_consent());
        let risky = effects_of(&t(json!({"destructiveHint": true})), true, false);
        assert!(risky.contains(Effects::MAY_DISCARD_DATA) && risky.needs_consent(), "consent plugins ask first");
        assert!(effects_of(&t(Value::Null), false, false).contains(Effects::WRITES_FILESYSTEM), "no hints: assume it may write");
        let remote = effects_of(&t(json!({"readOnlyHint": true, "openWorldHint": false})), false, true);
        assert!(remote.contains(Effects::ACCESSES_NETWORK) && !remote.contains(Effects::LAUNCHES_PROCESS), "a remote server uses the network");
    }

    #[test]
    fn plugins_without_a_cache_add_no_tools_and_bad_ids_are_skipped() {
        let mut s = crate::testing::session();
        s.settings.plugins.mcp =
            vec![McpPlugin { id: "Bad Id".into(), ..plugin("x") }, McpPlugin { id: "never_refreshed_plugin_for_tests".into(), ..plugin("x") }];
        assert!(install(&mut s).is_empty());
        assert!(s.ext.get::<Plugins>().is_some());
        let parsed: PluginSettings = toml::from_str("[[mcp]]\nid = \"notes\"\ncommand = \"notes-mcp\"\n").unwrap();
        assert!(parsed.mcp[0].enabled && parsed.mcp[0].consent, "on and asking by default");
    }

    fn plugin(command: &str) -> McpPlugin {
        McpPlugin {
            id: "notes".into(),
            name: None,
            command: command.into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            consent: true,
            module: false,
            watch: vec![],
            call_timeout_secs: None,
        }
    }

    #[test]
    fn a_plugin_that_exits_at_once_says_what_it_printed_and_leaves_a_log() {
        #[cfg(windows)]
        let (command, args) = ("cmd", vec!["/C".to_string(), "echo bad token in notes.cfg 1>&2 & exit 3".to_string()]);
        #[cfg(not(windows))]
        let (command, args) = ("sh", vec!["-c".to_string(), "echo bad token in notes.cfg >&2; exit 3".to_string()]);
        let config = tempfile::tempdir().unwrap();
        let p = McpPlugin { args, env: BTreeMap::from([("NOTES_KEY".to_string(), "sk-test-secret".to_string())]), ..plugin(command) };
        let e = refresh(&p, config.path()).unwrap_err();
        assert!(e.message.contains("exited with exit code 3"), "{e:?}");
        let said = e.remediation.unwrap_or_default();
        assert!(said.contains("bad token in notes.cfg") && said.contains("plugins.mcp"), "{said}");
        let logs: Vec<_> = std::fs::read_dir(crate::failure_log::dir_in(config.path())).unwrap().flatten().map(|f| f.path()).collect();
        assert_eq!(logs.len(), 1);
        assert!(said.contains(&format!("The details are in {}", logs[0].display())), "{said}");
        let log = std::fs::read_to_string(&logs[0]).unwrap();
        for part in ["notes plugin (an MCP server) didn't start", "Exit code: 3", "bad token in notes.cfg", command] {
            assert!(log.contains(part), "{part}: {log}");
        }
        assert!(!log.contains("sk-test-secret"), "its environment stays out of the log");
    }

    #[test]
    fn a_plugin_that_cant_start_names_its_log_and_a_remote_one_writes_none() {
        let config = tempfile::tempdir().unwrap();
        let args = vec!["--url".to_string(), "https://mcp.example.test/mcp?token=plainsecretvalue42".to_string()];
        let e = refresh(&McpPlugin { args, ..plugin("craft-anything-no-such-plugin-server") }, config.path()).unwrap_err();
        assert!(e.remediation.unwrap_or_default().contains("The details are in"), "a missing program is logged too");
        let logs: Vec<_> = std::fs::read_dir(crate::failure_log::dir_in(config.path())).unwrap().flatten().map(|f| f.path()).collect();
        let log = std::fs::read_to_string(&logs[0]).unwrap();
        assert!(log.contains("mcp.example.test/mcp?token=") && !log.contains("plainsecretvalue42"), "a key in an argument is hidden: {log}");
        let remote = McpPlugin { url: Some("http://127.0.0.1:0/mcp".into()), ..plugin("") };
        let other = tempfile::tempdir().unwrap();
        assert!(refresh(&remote, other.path()).is_err());
        assert!(!crate::failure_log::dir_in(other.path()).exists(), "no log for a remote server");
    }

    #[test]
    fn a_watched_file_newer_than_the_cache_makes_it_stale_and_modules_get_their_two_tools() {
        let config = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("engine.py");
        std::fs::write(&file, "x").unwrap();
        let p = McpPlugin { id: "revin_test_mod".into(), module: true, watch: vec![src.path().to_path_buf()], consent: false, ..plugin("revin-mcp") };
        assert!(p.cache_is_stale(config.path()), "no cache yet");
        let cache = p.cache_path(config.path());
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "[]").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        assert!(!p.cache_is_stale(config.path()), "the cache is newer than the source");
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(std::time::SystemTime::now() + Duration::from_secs(60)).unwrap();
        assert!(p.cache_is_stale(config.path()), "the source changed after the cache");
        assert!(!plugin("notes-mcp").cache_is_stale(config.path()), "an ordinary plugin is refreshed by hand");

        let mut tools = Vec::new();
        let mut plugins = Plugins::default();
        module_tools(&p, custom_toolset(&p.id).unwrap(), false, &mut plugins, &mut tools);
        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["revin_test_mod_module_tools", "revin_test_mod_module_call"]);
        assert!(!tools[1].effects.needs_consent(), "consent = false: a trusted module runs without asking");
        let s = PluginSettings { mcp: vec![p.clone(), McpPlugin { id: "other".into(), ..plugin("x") }], wasm: vec![] };
        assert_eq!(modules(&s), vec![("revin_test_mod".to_string(), "revin_test_mod".to_string())]);
    }

    #[test]
    fn a_plugin_is_local_or_remote_never_both() {
        assert!(plugin("notes-mcp").problem().is_none());
        assert!(matches!(plugin("notes-mcp").server(), Server::Command(_)));
        let remote = McpPlugin { url: Some("https://mcp.example.com/mcp".into()), ..plugin("") };
        assert!(remote.problem().is_none() && matches!(remote.server(), Server::Url(_)));
        assert_eq!(remote.place(), "https://mcp.example.com/mcp");
        assert!(plugin("").problem().unwrap().contains("command"));
        assert!(McpPlugin { url: Some("https://x".into()), ..plugin("notes-mcp") }.problem().unwrap().contains("both"));
        assert!(McpPlugin { url: Some("http://example.com/mcp".into()), ..plugin("") }.problem().unwrap().contains("https://"));
        let parsed: PluginSettings =
            toml::from_str("[[mcp]]\nid = \"docs\"\nurl = \"https://mcp.example.com/mcp\"\nheaders = { Authorization = \"Bearer t\" }\n").unwrap();
        assert!(parsed.mcp[0].problem().is_none() && parsed.mcp[0].headers.contains_key("Authorization"));
        assert!(!toml::to_string(&parsed).unwrap().contains("command"), "a remote plugin is saved without an empty command");
    }
}
