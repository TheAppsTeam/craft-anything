//! A module (a plugin with `module = true`) gets a sidebar row and its own screen, whose tools are
//! read live from the running server and run from forms. Driven against RevIn when
//! `REVIN_ENGINE` names its `revin_engine.py`; without it there is no module server to talk to.

use craft_anything_engine::plugins::{self, McpPlugin};
use craft_anything_engine::{Engine, Mode, Session, Settings};
use craft_anything_ui_egui::state::Screen;
use craft_anything_ui_egui::{CraftApp, demo};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::time::{Duration, Instant};

fn wait(h: &mut Harness<'static, CraftApp>, secs: u64, done: impl Fn(&CraftApp) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        h.step();
        if done(h.state()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_module_has_its_own_screen_with_live_tools_that_run() {
    let Some(engine_py) = std::env::var_os("REVIN_ENGINE") else {
        eprintln!("REVIN_ENGINE not set: no module server to drive");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let copy = src.path().join("revin_engine.py");
    // A copy, so the test can add a command to it the way a person editing RevIn would.
    std::fs::copy(std::path::PathBuf::from(engine_py), &copy).unwrap();
    let engine_py = copy;
    // Run with CRAFT_ANYTHING_CONFIG_DIR set to a scratch folder: the plugin's tool cache and logs go there.
    let plugin = McpPlugin {
        id: "revin".into(),
        name: Some("RevIn".into()),
        command: "/usr/bin/python3".into(),
        args: vec![engine_py.display().to_string(), "mcp-serve".into(), "--tools".into(), "cli".into()],
        env: Default::default(),
        cwd: engine_py.parent().map(std::path::Path::to_path_buf),
        url: None,
        headers: Default::default(),
        enabled: true,
        consent: false,
        module: true,
        watch: vec![engine_py.clone()],
        call_timeout_secs: None,
    };
    let mut settings = Settings { mode: Mode::Beginner, first_run_done: true, deep_mode: true, language: "en".into(), ..Default::default() };
    settings.plugins.mcp = vec![plugin];
    let mut session = Session::new(settings.clone());
    let tools = plugins::install(&mut session);
    let engine = Engine::shared_with(session, Vec::new(), tools);
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 820.0)).with_max_steps(32).build_eframe(move |cc| {
        CraftApp::setup_context(&cc.egui_ctx);
        let mut app = demo::app(engine);
        app.settings = settings;
        app
    });
    h.run_steps(3);
    h.get_by_label("RevIn").click();
    assert!(wait(&mut h, 60, |a| a.state.screen == Screen::Module && a.module.as_ref().is_some_and(|m| m.tools.len() > 10)), "the live tool list arrives");
    h.get_by_label("recover-swift");
    h.get_by_label("pricing-status").click();
    h.run_steps(3);
    // The form's Run button (the screen behind has a Run of its own).
    let runs: Vec<_> = h.query_all_by_label("Run").collect();
    assert!(!runs.is_empty());
    runs[runs.len() - 1].click();
    assert!(
        wait(&mut h, 120, |a| a.module.as_ref().is_some_and(|m| m.runs.first().is_some_and(|r| r.answer.is_some()))),
        "the run answers: {:?}",
        h.state().module.as_ref().map(|m| (
            m.selected.clone(),
            m.form_error.clone(),
            m.runs.iter().map(|r| (r.tool.clone(), r.args.to_string(), r.answer.clone())).collect::<Vec<_>>()
        ))
    );
    let answer = h.state().module.as_ref().and_then(|m| m.runs.first()).and_then(|r| r.answer.clone());
    assert!(matches!(answer, Some(Ok((ref text, false))) if text.contains("off_peak")), "{answer:?}");
    h.run_steps(4);
    // A command added to the module while the app is open shows up by itself.
    let before = h.state().module.as_ref().map_or(0, |m| m.tools.len());
    let code = std::fs::read_to_string(&engine_py).unwrap();
    let marker = "    mcp_cmd = sub.add_parser(\"mcp-serve\"";
    let added = "    hw = sub.add_parser(\"hello-craft\", help=\"Added while Craft was open\")\n    hw.add_argument(\"--name\", default=\"x\")\n";
    std::fs::write(&engine_py, code.replacen(marker, &format!("{added}{marker}"), 1)).unwrap();
    assert!(
        wait(&mut h, 30, |a| a.module.as_ref().is_some_and(|m| m.tools.len() == before + 1 && m.tools.iter().any(|t| t["name"] == "hello_craft"))),
        "the new command is listed without a restart"
    );
    h.get_by_label("hello-craft");
    h.run_steps(4);
    if let Some(out) = std::env::var_os("MODULE_SCREENSHOT") {
        let image = h.render().expect("render");
        image.save(out).expect("save screenshot");
    }
}
