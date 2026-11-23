//! An iOS build is photographed in the iOS Simulator: it can't run on the Mac with a
//! `--screenshot` option the way a desktop program does. The check finds the Simulator build
//! (`build/*-iphonesimulator/*.app` from the "SwiftUI (iOS)" stack's build, or
//! `build/Build/Products/*-iphonesimulator/*.app` from one with a derived-data folder), installs
//! it on a booted iPhone simulator (or boots one), starts it, waits for it to draw and saves the
//! screen as a PNG.

use craft_anything_platform::process::Process;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the app gets to draw its first screen before it's photographed.
const SETTLE: Duration = Duration::from_secs(6);

/// The newest Simulator build of the app in `app_dir`, if there is one.
pub fn simulator_app(app_dir: &Path) -> Option<PathBuf> {
    let build = app_dir.join("build");
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let configs = [build.clone(), build.join("Build").join("Products")].into_iter().flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten());
    for config in configs {
        if !config.file_name().to_string_lossy().ends_with("-iphonesimulator") {
            continue;
        }
        for app in std::fs::read_dir(config.path()).into_iter().flatten().flatten() {
            let p = app.path();
            if p.extension().is_some_and(|e| e == "app") && p.join("Info.plist").is_file() {
                let when = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((when, p));
            }
        }
    }
    found.into_iter().max_by_key(|(when, _)| *when).map(|(_, p)| p)
}

fn simctl(args: &[&str], timeout: Duration) -> Result<String, String> {
    let out = Process::new("xcrun").arg("simctl").args(args).timeout(timeout).label("iOS Simulator").run().map_err(|e| e.message)?;
    if out.success { Ok(out.stdout_text()) } else { Err(format!("simctl {} failed: {}", args.first().copied().unwrap_or(""), out.end_text().trim())) }
}

/// A booted iPhone simulator's id, booting an available one when none is running.
fn iphone() -> Result<String, String> {
    let list = simctl(&["list", "devices", "available", "-j"], Duration::from_secs(60))?;
    let v: serde_json::Value = serde_json::from_str(&list).map_err(|e| e.to_string())?;
    let devices: Vec<&serde_json::Value> = v["devices"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.iter())
        .filter(|(runtime, _)| runtime.contains("iOS"))
        .flat_map(|(_, list)| list.as_array().into_iter().flatten())
        .filter(|d| d["name"].as_str().is_some_and(|n| n.starts_with("iPhone")))
        .collect();
    if let Some(d) = devices.iter().find(|d| d["state"] == "Booted") {
        return d["udid"].as_str().map(str::to_string).ok_or_else(|| "a booted iPhone has no id".into());
    }
    let udid = devices.first().and_then(|d| d["udid"].as_str()).ok_or("no iPhone simulator is installed (Xcode → Settings → Components)")?;
    simctl(&["boot", udid], Duration::from_secs(180))?;
    simctl(&["bootstatus", udid, "-b"], Duration::from_secs(300))?;
    Ok(udid.to_string())
}

fn bundle_id(app: &Path) -> Result<String, String> {
    let raw = std::fs::read(app.join("Info.plist")).map_err(|e| e.to_string())?;
    let out = Process::new("plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-", "-"])
        .stdin_bytes(raw)
        .timeout(Duration::from_secs(30))
        .run()
        .map_err(|e| e.message)?;
    let id = out.stdout_text().trim().to_string();
    if out.success && !id.is_empty() { Ok(id) } else { Err("the build's Info.plist has no CFBundleIdentifier".into()) }
}

/// Install and start `app` on an iPhone simulator and save its screen to `shot`. Returns the PNG.
pub fn screenshot(app: &Path, shot: &Path) -> Result<Vec<u8>, String> {
    let udid = iphone()?;
    let id = bundle_id(app)?;
    let app_s = app.display().to_string();
    simctl(&["install", &udid, &app_s], Duration::from_secs(180))?;
    // A fresh start, so the picture shows the app as it opens.
    let _ = simctl(&["terminate", &udid, &id], Duration::from_secs(30));
    simctl(&["launch", &udid, &id], Duration::from_secs(60))?;
    std::thread::sleep(SETTLE);
    if let Some(dir) = shot.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let shot_s = shot.display().to_string();
    simctl(&["io", &udid, "screenshot", &shot_s], Duration::from_secs(60))?;
    std::fs::read(shot).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_simulator_builds_are_found_and_the_newest_wins() {
        let d = tempfile::tempdir().unwrap();
        assert!(simulator_app(d.path()).is_none());
        for (config, name) in [
            ("Build/Products/Debug-iphoneos", "Device.app"),
            ("Build/Products/Debug-iphonesimulator", "Old.app"),
            ("Debug-iphonesimulator", "Symroot.app"),
            ("Build/Products/Release-iphonesimulator", "New.app"),
        ] {
            let app = d.path().join("build").join(config).join(name);
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("Info.plist"), b"x").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::create_dir_all(d.path().join("build/Debug-iphonesimulator/NoPlist.app")).unwrap();
        let found = simulator_app(d.path()).unwrap();
        assert!(found.ends_with("Release-iphonesimulator/New.app"), "{found:?}");
        // The stack's own build (SYMROOT=build) is found without a derived-data folder.
        let own = tempfile::tempdir().unwrap();
        let app = own.path().join("build/Debug-iphonesimulator/Own.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("Info.plist"), b"x").unwrap();
        assert_eq!(simulator_app(own.path()), Some(app));
    }
}

/// Photographs a real Simulator build when `IOS_SIM_APP_DIR` names a folder holding
/// `build/*-iphonesimulator/*.app` (needs Xcode and an iPhone simulator).
#[cfg(test)]
mod live {
    #[test]
    fn a_real_simulator_build_is_photographed() {
        let Some(dir) = std::env::var_os("IOS_SIM_APP_DIR") else { return };
        let app = super::simulator_app(std::path::Path::new(&dir)).expect("a Simulator build");
        let out = tempfile::tempdir().unwrap();
        let shot = out.path().join("shot.png");
        let png = super::screenshot(&app, &shot).expect("photographed");
        assert!(png.starts_with(b"\x89PNG") && png.len() > 10_000, "a real picture: {} bytes", png.len());
        if let Some(keep) = std::env::var_os("IOS_SIM_SHOT") {
            std::fs::copy(&shot, keep).unwrap();
        }
    }
}
