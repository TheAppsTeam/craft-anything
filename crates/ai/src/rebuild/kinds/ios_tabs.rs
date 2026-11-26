//! An iOS build compared with the original tab by tab. Study had RevIn run the original in the
//! Simulator (`capture/revin/dynamic`): its probe opens each tab and saves the screen
//! (`screen_NN.png`) and its UI tree (`probe/ui_NN.json`). The check has RevIn run the build the
//! same way, then matches the screens by title: each of the original's tabs must open in the
//! build and hold the original's buttons (claims, so they decide); how alike each tab looks and
//! how many of its texts the build shows are advisory, as content can differ between runs.
//!
//! RevIn's probe then presses each tab's buttons (and types a short text into plain fields) and
//! records what changed (`probe/tap_NN_MM.json`): what opened, the title after, the texts that
//! appeared. Each of the original's presses is a claim: the same button on the same tab of the
//! build must open the same kind of thing, reach the same title and show most of the same new
//! texts. What can't be pressed safely (deleting, buying, sending to other devices) the probe
//! skips in both, so it isn't checked.

use super::*;
use craft_anything_engine::tools::web_common::decode_png;
use craft_anything_engine::workspace::Claim;
use std::collections::BTreeSet;

/// RevIn's tool that runs an app in the Simulator with its probe.
const RUN_TOOL: &str = "revin_run_simulator";
/// How long RevIn lets the build run while its probe opens the tabs.
const RUN_SECS: u64 = 45;
/// At most this many claims, so a busy screen doesn't drown the rest.
const MAX_CLAIMS: usize = 24;
/// `UIAccessibilityTraitButton`.
const TRAIT_BUTTON: u64 = 1;

/// One screen the probe saw: its title, texts, buttons and picture.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Screen {
    pub title: String,
    pub texts: BTreeSet<String>,
    pub buttons: Vec<String>,
    pub picture: Option<PathBuf>,
}

fn key(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A shown text as compared between runs: its digits don't count, as times, counts and versions
/// differ from one run to the next ("19:06:07 session active" is the same line at 19:21:03).
fn text_key(s: &str) -> String {
    key(s).chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
}

/// The screens in a RevIn run's output folder (`probe/ui_NN.json` with `screen_NN.png`), in order.
pub fn screens(dir: &Path) -> Vec<Screen> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("probe"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ui_")))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| {
            let doc: Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
            let mut s = Screen { title: doc["title"].as_str().unwrap_or_default().trim().to_string(), ..Screen::default() };
            let picture = dir.join(format!("screen_{:02}.png", doc["screen"].as_u64().unwrap_or(0)));
            s.picture = picture.is_file().then_some(picture);
            let mut seen = BTreeSet::new();
            let mut stack: Vec<&Value> = doc["windows"].as_array().into_iter().flatten().collect();
            while let Some(n) = stack.pop() {
                stack.extend(n["children"].as_array().into_iter().flatten());
                if n["hidden"].as_bool() == Some(true) {
                    continue;
                }
                let class = n["class"].as_str().unwrap_or_default();
                // Tab buttons say which tabs exist, not what this one holds; scroll bars are chrome.
                if class.contains("TabButton") || class.contains("TabBar") || class.contains("ScrollIndicator") {
                    continue;
                }
                let label = ["label", "text", "value"].iter().find_map(|k| n[*k].as_str().map(str::trim).filter(|t| !t.is_empty()));
                let Some(label) = label else { continue };
                s.texts.insert(text_key(label));
                if n["traits"].as_u64().is_some_and(|t| t & TRAIT_BUTTON != 0) && seen.insert(key(label)) {
                    s.buttons.push(label.to_string());
                }
            }
            Some(s)
        })
        .collect()
}

/// One press of the probe: on which tab, which control, and what changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tap {
    pub screen: String,
    pub button: String,
    /// What it opened: sheet, alert, push, tab, window, closed or none.
    pub opened: String,
    /// The title after the press, when it changed.
    pub title: Option<String>,
    /// Texts that appeared (the probe's own typed text left out).
    pub added: BTreeSet<String>,
}

/// The probe's presses in a RevIn run's folder (`probe/tap_NN_MM.json`), each with its tab's title.
pub fn taps(dir: &Path) -> Vec<Tap> {
    let probe = dir.join("probe");
    let read = |p: &Path| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let mut titles = std::collections::BTreeMap::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&probe).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.sort();
    for f in &files {
        if f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ui_"))
            && let Some(doc) = read(f)
        {
            titles.insert(doc["screen"].as_u64().unwrap_or(0), doc["title"].as_str().unwrap_or_default().trim().to_string());
        }
    }
    files
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("tap_")) && f.extension().is_some_and(|e| e == "json"))
        .filter_map(|f| {
            let doc = read(f)?;
            let button = doc["button"].as_str()?.trim().to_string();
            let screen = titles.get(&doc["screen"].as_u64()?).cloned().unwrap_or_default();
            let typed = doc["typed"].as_str().map(text_key);
            let words = |k: &str| -> BTreeSet<String> { doc[k].as_array().into_iter().flatten().filter_map(Value::as_str).map(text_key).collect() };
            let mut added = words("texts_added");
            if let Some(t) = &typed {
                added.remove(t);
            }
            let (before, after) = (doc["before_title"].as_str().unwrap_or_default(), doc["after_title"].as_str().unwrap_or_default());
            let title = (key(before) != key(after) && !after.trim().is_empty()).then(|| after.trim().to_string());
            Some(Tap { screen, button, opened: doc["opened"].as_str().unwrap_or("none").to_string(), title, added })
        })
        .collect()
}

/// Compare what each of the original's presses did with the same press in the build.
pub fn compare_taps(original: &[Tap], build: &[Tap]) -> (String, Vec<Claim>) {
    let mut claims = Vec::new();
    for o in original {
        let what = format!("Pressing \"{}\" on the \"{}\" tab does what it does in the original", o.button, o.screen);
        let found = build.iter().find(|b| key(&b.screen) == key(&o.screen) && key(&b.button) == key(&o.button));
        let Some(b) = found else {
            claims.push(Claim::new(what, false, "the build's probe found no such control there to press"));
            continue;
        };
        let mut wrong = Vec::new();
        if b.opened != o.opened {
            wrong.push(format!("the original opened {}, the build {}", o.opened, b.opened));
        }
        if let Some(t) = &o.title
            && b.title.as_deref().map(key) != Some(key(t))
        {
            wrong.push(format!("the original went to \"{t}\", the build to \"{}\"", b.title.as_deref().unwrap_or("the same screen")));
        }
        // Most of what appeared must appear: content can differ in a detail between runs.
        let shown = o.added.intersection(&b.added).count();
        if !o.added.is_empty() && shown * 2 < o.added.len() {
            let missing: Vec<&str> = o.added.difference(&b.added).take(3).map(String::as_str).collect();
            wrong.push(format!("{shown} of the {} texts the original showed appeared (missing: {})", o.added.len(), missing.join(", ")));
        }
        claims.push(Claim::new(what, wrong.is_empty(), wrong.join("; ")));
    }
    claims.truncate(MAX_CLAIMS);
    let same = claims.iter().filter(|c| c.passed).count();
    let note =
        if original.is_empty() { String::new() } else { format!(" Pressing the original's controls: {same} of {} do the same in the build.", claims.len()) };
    (note, claims)
}

/// The build's screen with the original's title, if it has one.
fn matching<'a>(original: &Screen, build: &'a [Screen]) -> Option<&'a Screen> {
    build.iter().find(|b| !original.title.is_empty() && key(&b.title) == key(&original.title))
}

/// Compare the original's screens with the build's: a note and the claims.
pub fn compare(original: &[Screen], build: &[Screen]) -> (String, Vec<Claim>) {
    let mut parts = Vec::new();
    let mut claims = Vec::new();
    for o in original {
        let name = if o.title.is_empty() { "untitled".to_string() } else { o.title.clone() };
        let found = matching(o, build);
        claims.push(Claim::new(format!("The original's \"{name}\" tab opens"), found.is_some(), "the build has no screen with that title"));
        let Some(b) = found else {
            parts.push(format!("{name}: not in the build"));
            continue;
        };
        let mut bits = Vec::new();
        if let (Some(op), Some(bp)) = (&o.picture, &b.picture)
            && let Some(img) = std::fs::read(bp).ok().and_then(|x| decode_png(&x).ok())
            && let screenshot::Likeness::Best { score, .. } = screenshot::picture_likeness(&img, std::slice::from_ref(op))
        {
            bits.push(format!("looks {score:.0}% alike"));
        }
        if !o.texts.is_empty() {
            let shared = o.texts.intersection(&b.texts).count();
            bits.push(format!("{shared} of its {} texts", o.texts.len()));
        }
        let theirs: BTreeSet<String> = b.buttons.iter().map(|x| key(x)).collect();
        for button in &o.buttons {
            claims.push(Claim::new(
                format!("The \"{name}\" tab has the original's \"{button}\" button"),
                theirs.contains(&key(button)),
                "no button with that label there",
            ));
        }
        parts.push(format!("{name}: {}", if bits.is_empty() { "opens".to_string() } else { bits.join(", ") }));
    }
    claims.truncate(MAX_CLAIMS);
    let note = format!(" Tab by tab (RevIn ran the build the way it ran the original): {}.", parts.join("; "));
    (note, claims)
}

/// Have RevIn run the build `app` tab by tab and compare it with the original's run. Empty when
/// Study didn't run the original.
pub fn tabs_note(cx: &CheckCx, app: &Path, dir: &Path) -> (String, Vec<Claim>) {
    let original_dir = cx.ws.capture_dir().join(crate::rebuild::revin::REVIN_DIR).join(crate::rebuild::revin::DYNAMIC_DIR);
    let original = screens(&original_dir);
    if original.is_empty() {
        return (String::new(), Vec::new());
    }
    let out = dir.join("tabs");
    let _ = std::fs::remove_dir_all(&out);
    if let Err(e) = cx.call(RUN_TOOL, json!({"input": app.display().to_string(), "output": out.display().to_string(), "seconds": RUN_SECS})) {
        return (format!(" Its tabs couldn't be compared with the original's: RevIn couldn't run the build ({}).", e.message), Vec::new());
    }
    let build = screens(&out);
    if build.is_empty() {
        return (" RevIn ran the build but its probe saw no screens, so the tabs weren't compared.".into(), Vec::new());
    }
    let (mut note, mut claims) = compare(&original, &build);
    let pressed = taps(&original_dir);
    if pressed.is_empty() {
        note.push_str(" The original's controls weren't pressed in Study, so what a press does isn't compared.");
    } else {
        let (tap_note, tap_claims) = compare_taps(&pressed, &taps(&out));
        note.push_str(&tap_note);
        claims.extend(tap_claims);
    }
    (note, claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_screen(dir: &Path, n: u64, title: &str, nodes: Value) {
        std::fs::create_dir_all(dir.join("probe")).unwrap();
        let doc = json!({"title": title, "screen": n, "windows": [{"class": "UIWindow", "children": nodes}]});
        std::fs::write(dir.join(format!("probe/ui_{n:02}.json")), serde_json::to_vec(&doc).unwrap()).unwrap();
    }

    #[test]
    fn a_missing_tab_and_a_missing_button_fail_their_claims() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("original"), d.path().join("build"));
        let main = json!([
            {"class": "SwiftUI.AccessibilityNode", "label": "Send", "traits": 257},
            {"class": "UILabel", "label": "Tap ollo and talk", "traits": 64},
            {"class": "_UITabButton", "label": "History", "traits": 268435457},
            {"class": "_UIScrollViewScrollIndicator", "label": "Horizontal scroll bar", "traits": 4097}
        ]);
        write_screen(&o, 0, "ollo", main.clone());
        write_screen(&o, 1, "History", json!([{"class": "UILabel", "label": "Past answers", "traits": 64}]));
        write_screen(&b, 0, "Ollo", json!([{"class": "UILabel", "label": "tap  ollo and talk", "traits": 64}]));

        let original = screens(&o);
        assert_eq!(original[0].buttons, vec!["Send".to_string()], "tab buttons and scroll bars aren't the screen's buttons");
        let (note, claims) = compare(&original, &screens(&b));
        let failed: Vec<&str> = claims.iter().filter(|c| !c.passed).map(|c| c.title.as_str()).collect();
        assert!(failed.iter().any(|t| t.contains("\"History\" tab opens")), "{failed:?}");
        assert!(failed.iter().any(|t| t.contains("\"Send\" button")), "{failed:?}");
        assert!(claims.iter().any(|c| c.passed && c.title.contains("\"ollo\" tab opens")), "titles match without case");
        assert!(note.contains("1 of its 2 texts") && note.contains("History: not in the build"), "{note}");

        // The build gains the tab and the button: every claim passes.
        write_screen(&b, 0, "ollo", main);
        write_screen(&b, 1, "History", json!([]));
        let (_, claims) = compare(&original, &screens(&b));
        assert!(claims.iter().all(|c| c.passed), "{claims:?}");
    }
}

#[cfg(test)]
mod tap_tests {
    use super::*;

    fn write(dir: &Path, name: &str, doc: Value) {
        std::fs::create_dir_all(dir.join("probe")).unwrap();
        std::fs::write(dir.join("probe").join(name), serde_json::to_vec(&doc).unwrap()).unwrap();
    }

    fn run(dir: &Path, tv_opens: &str, tv_shows: &[&str]) {
        write(dir, "ui_02.json", json!({"title": "Lab", "screen": 2, "windows": []}));
        write(
            dir,
            "tap_02_00.json",
            json!({"screen": 2, "button": "TV", "opened": tv_opens, "before_title": "Link", "after_title": "TV", "texts_added": tv_shows}),
        );
        write(dir, "tap_02_01.json", json!({"screen": 2, "button": "Message", "opened": "none", "typed": "RevIn test", "texts_added": ["RevIn test"]}));
    }

    #[test]
    fn a_press_that_opens_something_else_or_shows_less_fails_its_claim() {
        let d = tempfile::tempdir().unwrap();
        let (o, b) = (d.path().join("o"), d.path().join("b"));
        run(&o, "none", &["TV address", "Find", "not connected"]);
        let original = taps(&o);
        assert_eq!(original[0].screen, "Lab", "a press knows its tab's title");
        assert!(original[1].added.is_empty(), "the probe's own typed text isn't something the app showed");

        run(&b, "sheet", &["TV address"]);
        let (note, claims) = compare_taps(&original, &taps(&b));
        let tv = claims.iter().find(|c| c.title.contains("\"TV\"")).unwrap();
        assert!(!tv.passed && tv.detail.contains("sheet") && tv.detail.contains("texts"), "{tv:?}");
        assert!(claims.iter().any(|c| c.passed && c.title.contains("\"Message\"")), "{claims:?}");
        assert!(note.contains("1 of 2"), "{note}");

        let empty = d.path().join("e");
        write(&empty, "ui_02.json", json!({"title": "Lab", "screen": 2, "windows": []}));
        assert!(compare_taps(&original, &taps(&empty)).1.iter().all(|c| !c.passed), "an unpressed control fails");

        run(&b, "none", &["tv  address", "Find", "Something new"]);
        assert!(compare_taps(&original, &taps(&b)).1.iter().all(|c| c.passed), "most of the same texts and the same title pass");
    }
}

/// Compares two real RevIn runs when `IOS_TABS_ORIGINAL` and `IOS_TABS_BUILD` name their folders.
#[cfg(test)]
mod live {
    #[test]
    fn two_real_runs_are_compared_tab_by_tab() {
        let (Some(o), Some(b)) = (std::env::var_os("IOS_TABS_ORIGINAL"), std::env::var_os("IOS_TABS_BUILD")) else { return };
        let original = super::screens(std::path::Path::new(&o));
        assert!(!original.is_empty(), "the original's run has screens");
        let (note, claims) = super::compare(&original, &super::screens(std::path::Path::new(&b)));
        println!("{note}");
        for c in &claims {
            println!("{} {} {}", if c.passed { "PASS" } else { "FAIL" }, c.title, c.detail);
        }
        assert!(claims.iter().any(|c| c.title.contains("tab opens")), "every tab gets a claim");
        let (note, claims) = super::compare_taps(&super::taps(std::path::Path::new(&o)), &super::taps(std::path::Path::new(&b)));
        println!("{note}");
        for c in &claims {
            println!("{} {} {}", if c.passed { "PASS" } else { "FAIL" }, c.title, c.detail);
        }
    }
}
