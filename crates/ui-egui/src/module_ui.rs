//! A module's screen: a plugin marked `module = true` in config.toml (RevIn, for one) gets its own
//! sidebar row and this screen. Its tools are read live from the running server
//! (`<id>_module_tools`) and grouped; each opens a form made from its schema and runs through
//! `<id>_module_call`, so a tool the module gains while the app is open shows up without a
//! restart. When a watched file of the module changes, the list is read again by itself.

use crate::i18n::tr;
use crate::state::Screen;
use crate::theme::{self, Tokens};
use crate::{CraftApp, schema_form, widgets};
use craft_anything_engine::evidence::ToolError;
use craft_anything_engine::plugins::{MODULE_CALL, MODULE_TOOLS};
use egui::{RichText, Ui};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

/// How often the module's files are looked at for changes.
const WATCH_EVERY: Duration = Duration::from_secs(2);

/// One run of a module tool, newest first on the screen.
pub struct Run {
    pub job: u64,
    pub tool: String,
    pub args: Value,
    pub started: Instant,
    pub took: Option<Duration>,
    /// The answer's text, and whether the tool said it failed.
    pub answer: Option<Result<(String, bool), String>>,
}

/// What the module screen shows.
pub struct ModuleView {
    pub id: String,
    pub name: String,
    pub tools: Vec<Value>,
    pub list_job: Option<u64>,
    pub list_error: Option<String>,
    pub selected: Option<String>,
    pub form: BTreeMap<String, String>,
    pub form_error: Option<String>,
    pub filter: String,
    pub runs: Vec<Run>,
    /// The newest watched file when the list was last read.
    pub seen_mtime: Option<SystemTime>,
    pub checked: Instant,
    pub updated_note: Option<Instant>,
    /// Clean room refused the last list: read it again once Deep mode is on.
    pub refused: bool,
}

impl ModuleView {
    fn new(id: &str, name: &str) -> Self {
        ModuleView {
            id: id.to_string(),
            name: name.to_string(),
            tools: Vec::new(),
            list_job: None,
            list_error: None,
            selected: None,
            form: BTreeMap::new(),
            form_error: None,
            filter: String::new(),
            runs: Vec::new(),
            seen_mtime: None,
            checked: Instant::now(),
            updated_note: None,
            refused: false,
        }
    }

    fn tool(&self, name: &str) -> Option<&Value> {
        self.tools.iter().find(|t| t["name"].as_str() == Some(name))
    }
}

/// A tool's group and short name: RevIn titles read "RevIn <group> · <command>"; others go under
/// "Tools" by their title or name.
pub fn group_and_label(t: &Value) -> (String, String) {
    let name = t["name"].as_str().unwrap_or_default().to_string();
    let title = t["title"].as_str().unwrap_or_default();
    if let Some((head, label)) = title.split_once(" · ") {
        let group = head.split_once(' ').map_or(head, |(_, g)| g).trim();
        return (if group.is_empty() { "Tools".into() } else { group.to_string() }, label.trim().to_string());
    }
    ("Tools".into(), if title.is_empty() { name } else { title.to_string() })
}

/// The tools grouped, groups in the module's order of first appearance.
pub fn grouped(tools: &[Value], filter: &str) -> Vec<(String, Vec<(String, String)>)> {
    let filter = filter.trim().to_lowercase();
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for t in tools {
        let (group, label) = group_and_label(t);
        let name = t["name"].as_str().unwrap_or_default().to_string();
        let hay = format!("{label} {name} {}", t["description"].as_str().unwrap_or_default()).to_lowercase();
        if !filter.is_empty() && !hay.contains(&filter) {
            continue;
        }
        match out.iter_mut().find(|(g, _)| *g == group) {
            Some((_, v)) => v.push((name, label)),
            None => out.push((group, vec![(name, label)])),
        }
    }
    out
}

/// Whether a finished `act` job belongs to a module.
pub fn is_module_tool(tool: &str) -> bool {
    tool.ends_with(&format!("_{MODULE_TOOLS}")) || tool.ends_with(&format!("_{MODULE_CALL}"))
}

fn static_id(app: &CraftApp, id: &str) -> Option<&'static str> {
    app.engine.registry().get(id).map(|s| s.id)
}

/// Open a module's screen and read its tools.
pub fn open(app: &mut CraftApp, id: &str, name: &str) {
    if app.module.as_ref().is_none_or(|m| m.id != id) {
        app.module = Some(ModuleView::new(id, name));
    }
    app.state.screen = Screen::Module;
    if app.module.as_ref().is_some_and(|m| m.tools.is_empty() && m.list_job.is_none()) {
        reload(app);
    }
}

/// Read the module's tools from its running server.
pub fn reload(app: &mut CraftApp) {
    let Some(id) = app.module.as_ref().map(|m| m.id.clone()) else { return };
    let Some(tool) = static_id(app, &format!("{id}_{MODULE_TOOLS}")) else {
        if let Some(m) = app.module.as_mut() {
            m.list_error = Some(tr("This module's tools aren't loaded. Restart the app after adding it.").to_string());
        }
        return;
    };
    let mtime = watched_mtime(app, &id);
    let job = app.act(tool, json!({}), format!("{} {}", tr("Read the tools of"), id));
    if let Some(m) = app.module.as_mut() {
        m.list_job = Some(job);
        m.seen_mtime = mtime;
    }
}

fn watched_mtime(app: &CraftApp, id: &str) -> Option<SystemTime> {
    app.settings.plugins.mcp.iter().find(|p| p.id == id).and_then(|p| p.watched_mtime())
}

/// Each frame on the module screen: notice changed module files and read the tools again.
fn watch(app: &mut CraftApp) {
    let Some(m) = app.module.as_ref() else { return };
    if m.list_job.is_some() || m.checked.elapsed() < WATCH_EVERY {
        return;
    }
    let (id, seen, empty) = (m.id.clone(), m.seen_mtime, m.tools.is_empty());
    // Deep mode was just turned on after Clean room refused the list.
    if empty && app.settings.deep_mode && m.refused {
        if let Some(m) = app.module.as_mut() {
            m.list_error = None;
            m.refused = false;
        }
        reload(app);
        return;
    }
    let now = watched_mtime(app, &id);
    if let Some(m) = app.module.as_mut() {
        m.checked = Instant::now();
    }
    if now.is_some() && now != seen {
        reload(app);
        if let Some(m) = app.module.as_mut() {
            m.updated_note = Some(Instant::now());
        }
    }
}

/// A finished module job (from `CraftApp::acted`).
pub fn done(app: &mut CraftApp, job: u64, tool: &str, result: Result<Value, ToolError>) {
    let Some(m) = app.module.as_mut() else { return };
    if tool.ends_with(&format!("_{MODULE_TOOLS}")) {
        if m.list_job != Some(job) {
            return;
        }
        m.list_job = None;
        match result {
            Ok(v) => {
                m.tools = v["tools"].as_array().cloned().unwrap_or_default();
                m.list_error = None;
                if m.selected.as_ref().is_some_and(|s| m.tool(s).is_none()) {
                    m.selected = None;
                }
            }
            Err(e) => {
                m.refused = craft_anything_engine::clean_room::is_refusal(&e);
                m.list_error = Some(e.for_people());
            }
        }
        return;
    }
    let Some(run) = m.runs.iter_mut().find(|r| r.job == job) else { return };
    run.took = Some(run.started.elapsed());
    run.answer = Some(match result {
        Ok(v) => {
            let failed = v["isError"].as_bool() == Some(true);
            let mut text = craft_anything_engines::mcp_client::content_text(&v);
            if let Some(s) = v.get("structuredContent").filter(|s| !s.is_null())
                && text.trim_start().starts_with('{')
            {
                text = serde_json::to_string_pretty(s).unwrap_or(text);
            }
            Ok((text, failed))
        }
        Err(e) => Err(e.for_people()),
    });
}

fn run(app: &mut CraftApp, tool: String, args: Value) {
    let Some(id) = app.module.as_ref().map(|m| m.id.clone()) else { return };
    let Some(call) = static_id(app, &format!("{id}_{MODULE_CALL}")) else { return };
    let job = app.act_visible(call, json!({"tool": tool, "arguments": args}), format!("{id}: {tool}"));
    if let Some(m) = app.module.as_mut() {
        m.runs.insert(0, Run { job, tool, args, started: Instant::now(), took: None, answer: None });
        m.runs.truncate(30);
    }
}

pub fn show(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    watch(app);
    ui.ctx().request_repaint_after(WATCH_EVERY);
    let Some((name, count, loading, error, note)) = app.module.as_ref().map(|m| {
        (m.name.clone(), m.tools.len(), m.list_job.is_some(), m.list_error.clone(), m.updated_note.is_some_and(|n| n.elapsed() < Duration::from_secs(6)))
    }) else {
        app.state.screen = Screen::Home;
        return;
    };
    ui.add_space(theme::space::LG);
    ui.horizontal(|ui| {
        ui.add_space(theme::space::LG);
        crate::icons::show(ui, "puzzle", theme::icon::LG, t.accent);
        ui.label(RichText::new(&name).font(theme::semibold(theme::TITLE)).color(t.text));
        ui.add_space(theme::space::SM);
        let status = if loading { tr("reading its tools…").to_string() } else { format!("{count} {}", tr("tools")) };
        ui.label(RichText::new(status).color(t.text_dim));
        if note {
            ui.label(RichText::new(tr("updated: its files changed")).color(t.success));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(theme::space::LG);
            if widgets::secondary(ui, Some("refresh-cw"), "Read its tools again").clicked() {
                reload(app);
            }
        });
    });
    let deep = app.settings.deep_mode;
    if !deep {
        // A module like RevIn reads the original's code and files: Clean room refuses its tools.
        ui.horizontal(|ui| {
            ui.add_space(theme::space::LG);
            crate::icons::show(ui, "shield", theme::icon::SM, t.warning);
            ui.label(RichText::new(tr("This module looks inside apps and files, so it works in Deep mode.")).color(t.warning));
            if widgets::secondary(ui, None, "Turn on Deep mode").clicked() {
                app.state.modal = Some(crate::state::Modal::DeepMode);
            }
        });
    } else if let Some(e) = error {
        ui.horizontal(|ui| {
            ui.add_space(theme::space::LG);
            ui.colored_label(t.danger, e);
        });
    }
    ui.add_space(theme::space::MD);
    let avail = ui.available_size();
    ui.horizontal_top(|ui| {
        ui.add_space(theme::space::LG);
        ui.allocate_ui_with_layout(egui::vec2(260.0, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| tool_list(app, ui, &t));
        ui.add_space(theme::space::LG);
        ui.allocate_ui_with_layout(egui::vec2((ui.available_width() - theme::space::LG).max(200.0), avail.y), egui::Layout::top_down(egui::Align::Min), |ui| {
            egui::ScrollArea::vertical().id_salt("module-main").auto_shrink([false, false]).show(ui, |ui| {
                tool_form(app, ui, &t);
                ui.add_space(theme::space::LG);
                runs(app, ui, &t);
            });
        });
    });
}

fn tool_list(app: &mut CraftApp, ui: &mut Ui, t: &Tokens) {
    let Some(m) = app.module.as_mut() else { return };
    ui.add(egui::TextEdit::singleline(&mut m.filter).hint_text(tr("Find a tool")).desired_width(f32::INFINITY));
    ui.add_space(theme::space::SM);
    let groups = grouped(&m.tools, &m.filter);
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("module-tools").auto_shrink([false, false]).show(ui, |ui| {
        for (group, tools) in groups {
            ui.add_space(theme::space::SM);
            widgets::caption(ui, &group);
            for (name, label) in tools {
                let on = m.selected.as_deref() == Some(name.as_str());
                let text = RichText::new(&label).color(if on { t.text } else { t.text_dim });
                if ui.add(egui::Button::selectable(on, text).min_size(egui::vec2(ui.available_width(), 26.0))).clicked() {
                    pick = Some(name);
                }
            }
        }
    });
    if let Some(name) = pick {
        if m.selected.as_ref() != Some(&name) {
            m.form.clear();
            m.form_error = None;
        }
        m.selected = Some(name);
    }
}

fn tool_form(app: &mut CraftApp, ui: &mut Ui, t: &Tokens) {
    let Some(m) = app.module.as_mut() else { return };
    let Some(name) = m.selected.clone() else {
        ui.label(RichText::new(tr("Pick a tool on the left.")).color(t.text_dim));
        return;
    };
    let Some(tool) = m.tool(&name).cloned() else { return };
    let (_, label) = group_and_label(&tool);
    let fields = schema_form::fields(&tool["inputSchema"]);
    let mut go = false;
    widgets::card(t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(&label).font(theme::semibold(theme::SECTION)).color(t.text));
        if let Some(d) = tool["description"].as_str() {
            ui.label(RichText::new(d).color(t.text_dim));
        }
        ui.label(RichText::new(&name).font(theme::mono(theme::CAPTION)).color(t.text_faint));
        ui.add_space(theme::space::SM);
        schema_form::show(ui, &fields, &mut m.form);
        if let Some(e) = &m.form_error {
            ui.colored_label(t.danger, e);
        }
        ui.add_space(theme::space::MD);
        go = widgets::primary_icon(ui, Some("play"), "Run", true).clicked();
    });
    if go {
        match schema_form::build(&fields, &m.form) {
            Ok(args) => {
                m.form_error = None;
                run(app, name, args);
            }
            Err(e) => m.form_error = Some(e),
        }
    }
}

fn runs(app: &mut CraftApp, ui: &mut Ui, t: &Tokens) {
    let Some(m) = app.module.as_ref() else { return };
    if m.runs.is_empty() {
        return;
    }
    widgets::caption(ui, "Runs");
    let mut cancel = None;
    for r in &m.runs {
        widgets::card(t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(&r.tool).font(theme::semibold(theme::BODY)).color(t.text));
                let (state, colour) = match &r.answer {
                    None => (format!("{} {}s", tr("running"), r.started.elapsed().as_secs()), t.text_dim),
                    Some(Ok((_, false))) => (format!("{} {:.1}s", tr("done in"), r.took.unwrap_or_default().as_secs_f32()), t.success),
                    Some(Ok((_, true))) | Some(Err(_)) => (tr("failed").to_string(), t.danger),
                };
                ui.label(RichText::new(state).color(colour));
                if r.answer.is_none() && widgets::secondary(ui, Some("square"), "Stop").clicked() {
                    cancel = Some(r.job);
                }
            });
            ui.label(RichText::new(r.args.to_string()).font(theme::mono(theme::CAPTION)).color(t.text_faint));
            let text = match &r.answer {
                Some(Ok((s, _))) => s.as_str(),
                Some(Err(e)) => e.as_str(),
                None => "",
            };
            if !text.is_empty() {
                egui::ScrollArea::vertical().id_salt(("module-run", r.job)).max_height(320.0).show(ui, |ui| {
                    let mut shown = text;
                    ui.add(egui::TextEdit::multiline(&mut shown).font(theme::mono(theme::CODE)).desired_width(f32::INFINITY));
                });
            }
        });
        ui.add_space(theme::space::SM);
    }
    if let Some(job) = cancel {
        app.jobs.cancel(job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revin_titles_group_their_tools_and_others_go_under_tools() {
        let tools = vec![
            json!({"name": "recover_swift", "title": "RevIn Recover · recover-swift", "description": "Swift"}),
            json!({"name": "doctor", "title": "RevIn Setup · doctor"}),
            json!({"name": "reconstruct", "title": "RevIn Recover · reconstruct"}),
            json!({"name": "echo"}),
        ];
        let g = grouped(&tools, "");
        assert_eq!(g.iter().map(|(g, _)| g.as_str()).collect::<Vec<_>>(), ["Recover", "Setup", "Tools"]);
        assert_eq!(g[0].1, vec![("recover_swift".to_string(), "recover-swift".to_string()), ("reconstruct".to_string(), "reconstruct".to_string())]);
        assert_eq!(grouped(&tools, "swift").len(), 1);
        assert!(is_module_tool("revin_module_call") && is_module_tool("revin_module_tools") && !is_module_tool("revin_doctor"));
    }
}
