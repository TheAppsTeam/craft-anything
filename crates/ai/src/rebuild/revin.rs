//! RevIn in the study: when the RevIn module is installed (a plugin with `module = true` whose
//! tools include `recover_swift`), a Deep-mode study of an iOS/macOS app or an Android package
//! hands the parts RevIn does best to it, and tells the builder which system to ask for what.
//!
//! The split comes from a measured comparison on a real release build (an iOS app with an
//! Android package inside), against its source:
//!
//! - RevIn: Swift types, fields, enum cases and conformances from reflection metadata (Craft's
//!   symbol-based list finds none of a stripped app's own types), function and file names,
//!   readable logic (Ghidra pseudocode lifted to Swift), Kotlin declarations from an APK, and
//!   running an iOS app in the Simulator.
//! - Craft: code signature and entitlements, Info.plist, Mach-O and libraries, fast navigation
//!   (cross-references, call graph, search, bytes), an APK's manifest, signature and method
//!   bytecode, assets as they are, and building and checking the rebuild.
//! - Both: strings (each finds literals the other misses; together they find the most).

use super::*;

/// Where RevIn's study output goes, under the capture folder.
pub(super) const REVIN_DIR: &str = "revin";
const SWIFT_TOOL: &str = "revin_recover_swift";
const ANDROID_TOOL: &str = "revin_recover_android";
const SIMULATOR_TOOL: &str = "revin_run_simulator";
/// Where the original's screens from the Simulator go, under [`REVIN_DIR`].
pub(super) const DYNAMIC_DIR: &str = "dynamic";
/// Where an iOS rebuild starts from RevIn's recovered package, under the app folder.
pub(super) const RECOVERED_DIR: &str = "Recovered";
/// The stack whose build starts from RevIn's recovered Swift package.
const SWIFT_STACK: &str = "SwiftUI (iOS)";

/// Told to the builder when the app starts from RevIn's recovered package (`app/Recovered`).
pub(super) const RECOVERED_BRIEF: &str = "- app/Recovered/ is the original's own architecture, recovered by RevIn from its Swift metadata: \
a Swift package with every module, type, stored property, enum case and conformance of the original, and it builds for the Simulator as it is. \
Start from it; don't write these types again elsewhere. Add it to app/project.yml as a local package (`packages: Recovered: path: Recovered`) \
and make the app target depend on the modules it needs, so the app is a thin `@main` over them. \
Then make it work in place: give the types their behaviour (function bodies, computed properties, the views' `body`), \
keeping each name, field and case as RevIn recovered it. Where the original's code is needed, ask revin_recover_swift with logic true \
or read Ghidra's pseudocode; capture/revin/swift/ARCHITECTURE.md says which module holds what.";

/// Which system the builder asks for what, in its brief (when RevIn's study output exists).
pub(super) const ROUTING_BRIEF: &str = "- Two engines studied the original; ask each for what it does best.\n  \
RevIn (its output is in capture/revin/, and its tools are the revin_* tools): Swift types, their fields, enum cases and protocol conformances \
(capture/revin/swift/Sources and RECOVERY_REPORT.json are exact: use these names and shapes), function and original file names, \
readable logic (revin_recover_swift with logic true rewrites function bodies as Swift from Ghidra's pseudocode), Kotlin declarations of an Android package \
(capture/revin/android/), and running an iOS app in the Simulator for its screens and UI tree (revin_run_simulator).\n  \
Craft (the native_*, apple_*, android_* tools): code signature and entitlements (apple_signature_show), Info.plist (apple_plist_show), \
Mach-O and libraries (apple_macho_show, apple_dylibs_resolve), finding your way in the code (native_xrefs_list, native_call_graph_show, native_strings_find, native_bytes_show), \
an Android package's manifest, components and method bytecode (android_package_show, android_components_list, android_method_show), and assets as they are (apple_assets_list).\n  \
When capture/revin/dynamic/ has screen_NN.png, those are the original's own screens on an iPhone: match them, \
and keep the app an iOS app (the person asked to recreate an iOS app); the screenshot check compares your build's picture with them.\n  \
Strings: use both, capture/revin/swift/RECOVERED_STRINGS.json and native_strings_browse; each finds text the other misses.\n  \
Don't take sounds or pictures from revin_extract_media: it can add made-up sounds. Use the app's own files.";

/// An iOS/macOS app (bundle, .ipa) or a Mach-O program.
pub(super) fn is_apple(path: &Path) -> bool {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("app") || e.eq_ignore_ascii_case("ipa")) {
        return true;
    }
    let mut magic = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic)).is_ok()
        && matches!(magic, [0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe] | [0xca, 0xfe, 0xba, 0xbf])
}

/// Android packages: the target itself, or ones shipped inside an app bundle (a few levels down).
pub(super) fn android_packages(path: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("apk")) && p.is_file() {
                out.push(p);
            } else if depth > 0 && p.is_dir() {
                walk(&p, depth - 1, out);
            }
            if out.len() >= 8 {
                return;
            }
        }
    }
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("apk")) {
        return vec![path.to_path_buf()];
    }
    let mut out = Vec::new();
    if path.is_dir() {
        walk(path, 3, &mut out);
    }
    out.sort();
    out
}

/// The original's screens RevIn photographed in the Simulator (`screen_NN.png`), in order: the
/// screenshot check compares the build with these.
pub(super) fn original_screens(capture: &Path) -> Vec<PathBuf> {
    let dir = capture.join(REVIN_DIR).join(DYNAMIC_DIR);
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("screen_")))
        .collect();
    out.sort();
    out
}

/// The RevIn study output exists for this capture.
pub(super) fn studied(capture: &Path) -> bool {
    capture.join(REVIN_DIR).is_dir()
}

/// RevIn's recovered package built (its own xcodebuild check, with self-healing, passed).
fn recovered_builds(dir: &Path) -> bool {
    std::fs::read(dir.join("RECOVERY_REPORT.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .is_some_and(|v| v["build"]["ok"].as_bool() == Some(true))
}

/// The app starts from RevIn's recovered package.
pub(super) fn seeded(app: &Path) -> bool {
    app.join(RECOVERED_DIR).join("Package.swift").is_file()
}

/// Copy the recovered package (its manifest and sources, not RevIn's reports or extras) into
/// `app/Recovered`, unless the app already has one: the builder's work there is kept.
fn seed(from: &Path, app: &Path) -> std::io::Result<bool> {
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            let (src, dst) = (e.path(), to.join(e.file_name()));
            if e.file_type()?.is_dir() { copy(&src, &dst)? } else { std::fs::copy(&src, &dst).map(drop)? }
        }
        Ok(())
    }
    if seeded(app) {
        return Ok(false);
    }
    let to = app.join(RECOVERED_DIR);
    for part in ["Package.swift", "Sources", "ObjCHeaders"] {
        let src = from.join(part);
        if src.is_dir() {
            copy(&src, &to.join(part))?;
        } else if src.is_file() {
            std::fs::create_dir_all(&to)?;
            std::fs::copy(&src, to.join(part))?;
        }
    }
    Ok(true)
}

/// A short line from RevIn's recovery report.
fn report_line(dir: &Path) -> String {
    let Ok(raw) = std::fs::read(dir.join("RECOVERY_REPORT.json")) else { return String::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else { return String::new() };
    let s = &v["summary"];
    let n = |k: &str| s[k].as_u64().unwrap_or(0);
    let modules = s["modules"].as_array().map_or(0, Vec::len);
    format!(
        "{} types in {modules} modules, {} stored properties, {} enum cases, {} conformances.",
        n("types"),
        n("stored_properties"),
        n("enum_cases"),
        n("conformances")
    )
}

impl Worker {
    /// Deep-mode study of a local target with RevIn, when it's installed and the target is one it
    /// reads. Returns the notes' section (empty when RevIn has nothing to do here). A RevIn failure
    /// is said in the notes, never fatal: the builder still has Craft's own tools.
    pub(super) fn revin_study(&mut self, ws: &mut Workspace, path: &Path, program: &Path) -> Result<String, ToolError> {
        let has = |id: &str| self.engine.registry().get(id).is_some();
        let apple = has(SWIFT_TOOL) && (is_apple(path) || is_apple(program));
        let apks = if has(ANDROID_TOOL) { android_packages(path) } else { Vec::new() };
        if !apple && apks.is_empty() {
            return Ok(String::new());
        }
        let out = ws.capture_dir().join(REVIN_DIR);
        if out.exists() {
            std::fs::remove_dir_all(&out).map_err(|e| ToolError::io(out.display(), e))?;
        }
        std::fs::create_dir_all(&out).map_err(|e| ToolError::io(out.display(), e))?;
        let mut notes = String::from(
            "\n## RevIn\n\nRevIn read what it reads best (Swift metadata, names, Kotlin declarations); Craft's tools cover the rest. See the brief for who answers what.\n",
        );
        if apple {
            self.step(ws, Step::Study, Status::Inspecting, "RevIn is recovering the app's Swift types, names and strings…")?;
            let dir = out.join("swift");
            let input = if is_apple(path) { path } else { program };
            // RevIn builds what it recovered and heals compile errors, so the package can be the
            // rebuild's starting point (about 20 s on a real app).
            match self.call(SWIFT_TOOL, json!({"input": input.display().to_string(), "output": dir.display().to_string()})) {
                Ok(_) => {
                    let builds = recovered_builds(&dir);
                    notes.push_str(&format!(
                        "\n- Swift: capture/{REVIN_DIR}/swift/ (Sources/, RECOVERY_REPORT.json, RECOVERED_STRINGS.json). {} {}\n",
                        report_line(&dir),
                        if builds { "The recovered package builds." } else { "The recovered package doesn't build as it is." }
                    ));
                    if builds && ws.manifest.stack == SWIFT_STACK {
                        match seed(&dir, &ws.app_dir()) {
                            Ok(true) => notes.push_str(&format!("- The rebuild starts from it: copied to app/{RECOVERED_DIR}/.\n")),
                            Ok(false) => notes.push_str(&format!("- app/{RECOVERED_DIR}/ is already there and is kept.\n")),
                            Err(e) => notes.push_str(&format!("- It couldn't be copied to app/{RECOVERED_DIR}/ ({e}).\n")),
                        }
                    }
                }
                Err(e) => notes.push_str(&format!("\n- Swift: RevIn couldn't recover it ({}).\n", e.message)),
            }
        }
        // The original running: its screens are the pictures the build is checked against.
        let runtime = self.engine.session().settings.runtime_study;
        if apple && runtime && has(SIMULATOR_TOOL) {
            self.step(ws, Step::Study, Status::Inspecting, "RevIn is running the original in the iOS Simulator to photograph its screens…")?;
            let dir = out.join(DYNAMIC_DIR);
            let input = if is_apple(path) { path } else { program };
            match self.call(SIMULATOR_TOOL, json!({"input": input.display().to_string(), "output": dir.display().to_string(), "seconds": 60})) {
                Ok(_) => {
                    let n = original_screens(&ws.capture_dir()).len();
                    notes.push_str(&format!(
                        "- The original running in the iOS Simulator: {n} screen(s) in capture/{REVIN_DIR}/{DYNAMIC_DIR}/screen_NN.png, its UI tree per screen in probe/, its log and URLs. The build is checked against these screens.\n"
                    ));
                }
                Err(e) => {
                    notes.push_str(&format!("- The original couldn't be run in the Simulator ({}), so the build isn't compared with its screens.\n", e.message))
                }
            }
        }
        for apk in apks {
            let name = apk.file_stem().map_or_else(|| "package".to_string(), |s| s.to_string_lossy().to_string());
            self.step(ws, Step::Study, Status::Inspecting, &format!("RevIn is reading the Kotlin declarations of {name}…"))?;
            let dir = out.join("android").join(&name);
            match self.call(ANDROID_TOOL, json!({"input": apk.display().to_string(), "output": dir.display().to_string(), "no_verify": true})) {
                Ok(_) => notes.push_str(&format!(
                    "- Android package {}: Kotlin declarations in capture/{REVIN_DIR}/android/{name}/ (bodies: read them with android_method_show).\n",
                    apk.display()
                )),
                Err(e) => notes.push_str(&format!("- Android package {}: RevIn couldn't read it ({}).\n", apk.display(), e.message)),
            }
        }
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_targets_and_nested_android_packages_are_found() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("Muse.app");
        std::fs::create_dir_all(app.join("Watch/Inner.app")).unwrap();
        std::fs::write(app.join("Muse"), [0xcf, 0xfa, 0xed, 0xfe, 0, 0]).unwrap();
        std::fs::write(app.join("pointer.apk"), b"PK").unwrap();
        std::fs::write(app.join("Watch/Inner.app/side.apk"), b"PK").unwrap();
        std::fs::write(d.path().join("notes.txt"), b"hello").unwrap();
        assert!(is_apple(&app) && is_apple(&app.join("Muse")) && !is_apple(&d.path().join("notes.txt")));
        let apks = android_packages(&app);
        assert_eq!(apks.len(), 2, "{apks:?}");
        assert_eq!(android_packages(&app.join("pointer.apk")), vec![app.join("pointer.apk")]);
        assert!(android_packages(&d.path().join("notes.txt")).is_empty());
    }

    #[test]
    fn the_originals_screens_are_found_in_order_and_other_files_are_not() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join(REVIN_DIR).join(DYNAMIC_DIR);
        std::fs::create_dir_all(dir.join("probe")).unwrap();
        for f in ["screen_01.png", "screen_00.png", "app.log", "probe/ui_0.json", "notes.png"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let screens = original_screens(d.path());
        assert_eq!(screens, vec![dir.join("screen_00.png"), dir.join("screen_01.png")]);
        assert!(original_screens(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn the_report_line_counts_what_revin_recovered() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("RECOVERY_REPORT.json"),
            br#"{"summary": {"types": 205, "modules": ["A", "B"], "stored_properties": 563, "enum_cases": 255, "conformances": 380}}"#,
        )
        .unwrap();
        let line = report_line(d.path());
        assert!(line.contains("205 types in 2 modules") && line.contains("380 conformances"), "{line}");
        assert!(report_line(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn a_package_that_builds_seeds_the_app_once_and_the_builders_work_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let (rec, app) = (d.path().join("swift"), d.path().join("app"));
        std::fs::create_dir_all(rec.join("Sources/Kit")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(rec.join("Package.swift"), b"// pkg").unwrap();
        std::fs::write(rec.join("Sources/Kit/A.swift"), b"struct A {}").unwrap();
        std::fs::write(rec.join("RECOVERY_REPORT.json"), br#"{"build": {"ok": false}}"#).unwrap();
        assert!(!recovered_builds(&rec) && !recovered_builds(&d.path().join("missing")));
        std::fs::write(rec.join("RECOVERY_REPORT.json"), br#"{"build": {"ok": true}}"#).unwrap();
        assert!(recovered_builds(&rec));

        assert!(seed(&rec, &app).unwrap() && seeded(&app));
        let kit = app.join(RECOVERED_DIR).join("Sources/Kit/A.swift");
        assert!(kit.is_file() && !app.join(RECOVERED_DIR).join("RECOVERY_REPORT.json").exists());
        // The builder gave A its behaviour; a second study doesn't undo it.
        std::fs::write(&kit, b"struct A { func go() {} }").unwrap();
        assert!(!seed(&rec, &app).unwrap());
        assert_eq!(std::fs::read(&kit).unwrap(), b"struct A { func go() {} }");
    }
}
