use eframe::egui;
use semver::Version;
use serde_json::Value;
use std::{
    ffi::CString,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "anuchonmpt/local-lens";
const RELEASES: &str = "https://github.com/anuchonmpt/local-lens/releases";
const APP_NAME: &str = "Cluster Observatory.app";
const BUNDLE_ID: &str = "local.lenser.cluster-observatory";
const CHANGELOG: &str = include_str!("../CHANGELOG.md");

#[derive(Clone)]
struct Release {
    version: Version,
    url: String,
    sha256: String,
    notes: String,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Checking,
    Current,
    Available(Release),
    Installing,
    Installed(PathBuf),
    Error(String),
}

#[derive(Default)]
pub struct Updates {
    visible: bool,
    show_history: bool,
    state: State,
    result: Option<mpsc::Receiver<State>>,
}

impl Updates {
    pub fn open(&mut self) {
        self.visible = true;
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.result {
            match receiver.try_recv() {
                Ok(state) => {
                    self.state = state;
                    self.result = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.state = State::Error("The update task stopped unexpectedly.".into());
                    self.result = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !self.visible {
            return;
        }

        let mut visible = self.visible;
        egui::Window::new("Software Updates")
            .open(&mut visible)
            .resizable(false)
            .collapsible(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label(format!("Running version: {VERSION}"));
                match &self.state {
                    State::Idle => {}
                    State::Current => {
                        ui.label("You're up to date.");
                    }
                    State::Error(message) => {
                        ui.label(message);
                    }
                    State::Checking | State::Installing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if matches!(self.state, State::Checking) {
                                "Checking GitHub…"
                            } else {
                                "Downloading and installing the update…"
                            });
                        });
                    }
                    State::Available(release) => {
                        ui.label(format!("Version {} is available.", release.version));
                        if !release.notes.trim().is_empty() {
                            ui.collapsing("What's New", |ui| {
                                show_changelog(ui, "release-notes", &release.notes)
                            });
                        }
                        if ui.button("Download and Install").clicked() {
                            let release = release.clone();
                            self.start(ctx, State::Installing, move || {
                                let bundle = running_bundle()?;
                                install(&release, &bundle, VERSION)?;
                                Ok(State::Installed(bundle))
                            });
                        }
                    }
                    State::Installed(bundle) => {
                        ui.label("Update installed. Restart to use the new version.");
                        if ui.button("Restart App").clicked() {
                            match restart(bundle) {
                                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(error) => {
                                    ui.label(format!("Could not restart: {error}"));
                                }
                            }
                        }
                    }
                }
                ui.horizontal(|ui| {
                    let can_check =
                        matches!(self.state, State::Idle | State::Current | State::Error(_));
                    if ui
                        .add_enabled(can_check, egui::Button::new("Check for Updates"))
                        .clicked()
                    {
                        self.start(ctx, State::Checking, || {
                            Ok(check(VERSION)?.map_or(State::Current, State::Available))
                        });
                    }
                    ui.hyperlink_to("GitHub Releases", RELEASES);
                });
                if ui
                    .selectable_label(self.show_history, "Version History")
                    .clicked()
                {
                    self.show_history = !self.show_history;
                }
                if self.show_history {
                    show_changelog(ui, "version-history", CHANGELOG);
                }
            });
        self.visible = visible;
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        state: State,
        task: impl FnOnce() -> Result<State, String> + Send + 'static,
    ) {
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        self.state = state;
        self.result = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(task().unwrap_or_else(State::Error));
            ctx.request_repaint();
        });
    }
}

fn show_changelog(ui: &mut egui::Ui, id: &str, notes: &str) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(220.0)
        .show(ui, |ui| {
            for line in notes.lines() {
                if let Some(heading) = line.strip_prefix("## ") {
                    ui.strong(heading);
                } else if let Some(item) = line.strip_prefix("- ") {
                    ui.label(format!("• {item}"));
                } else if !line.is_empty() && !line.starts_with("# ") {
                    ui.label(line);
                }
            }
        });
}

fn run(command: &mut Command, action: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{action}: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{action}: {}", error.trim()));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{action}: {error}"))
}

fn curl() -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
    ]);
    command
}

fn check(current: &str) -> Result<Option<Release>, String> {
    let response = run(
        curl().args([
            "--max-time",
            "25",
            "--header",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ]),
        "Could not check GitHub Releases",
    )?;
    let release: Value = serde_json::from_str(&response).map_err(|error| error.to_string())?;
    let tag = release["tag_name"]
        .as_str()
        .and_then(|tag| tag.strip_prefix('v'))
        .ok_or("GitHub returned an invalid release tag.")?;
    let version = Version::parse(tag).map_err(|_| "GitHub returned an invalid release version.")?;
    let current = Version::parse(current).map_err(|error| error.to_string())?;
    if version <= current || !version.pre.is_empty() {
        return Ok(None);
    }

    let name = format!("Cluster-Observatory-{version}-macOS.dmg");
    let asset = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| asset["name"] == name.as_str() && asset["state"] == "uploaded")
        .ok_or("The latest release does not have a macOS installer yet.")?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("GitHub did not provide a valid installer checksum.")?;
    let url = format!("{RELEASES}/download/v{version}/{name}");
    Ok(Some(Release {
        version,
        url,
        sha256: sha256.to_ascii_lowercase(),
        notes: release["body"].as_str().unwrap_or_default().to_owned(),
    }))
}

fn running_bundle() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    executable
        .ancestors()
        .nth(3)
        .filter(|bundle| {
            bundle
                .extension()
                .is_some_and(|extension| extension == "app")
        })
        .filter(|bundle| bundle.join("Contents/MacOS/cluster-observatory") == executable)
        .map(Path::to_path_buf)
        .ok_or_else(|| "Open the installed Cluster Observatory.app to install updates.".into())
}

fn plist(bundle: &Path, key: &str) -> Result<String, String> {
    run(
        Command::new("/usr/libexec/PlistBuddy")
            .arg("-c")
            .arg(format!("Print :{key}"))
            .arg(bundle.join("Contents/Info.plist")),
        "Could not read app information",
    )
    .map(|value| value.trim().to_owned())
}

fn install(release: &Release, bundle: &Path, current: &str) -> Result<(), String> {
    if plist(bundle, "CFBundleIdentifier")? != BUNDLE_ID {
        return Err("This app bundle has an unexpected identifier.".into());
    }
    let parent = bundle
        .parent()
        .ok_or("Could not locate the app's enclosing folder.")?;
    // Stage on the same volume so the final swap never exposes a half-copied app.
    let stage = tempfile::Builder::new().prefix(".cluster-observatory-update-")
        .tempdir_in(parent)
        .map_err(|error| format!(
            "Could not write beside the app. Copy it from the disk image to a writable Applications folder and reopen it. ({error})"
        ))?;
    let download = tempfile::tempdir().map_err(|error| error.to_string())?;
    let dmg = download.path().join("update.dmg");
    run(
        curl()
            .args([
                "--max-time",
                "300",
                "--max-filesize",
                "157286400",
                "--output",
            ])
            .arg(&dmg)
            .arg(&release.url),
        "Could not download the update",
    )?;
    let digest = run(
        Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(&dmg),
        "Could not check the installer checksum",
    )?;
    if digest.split_whitespace().next() != Some(release.sha256.as_str()) {
        return Err("The downloaded installer did not match GitHub's checksum.".into());
    }

    let mount = download.path().join("mounted");
    fs::create_dir(&mount).map_err(|error| error.to_string())?;
    let replacement = stage.path().join(APP_NAME);
    run(
        Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-readonly",
                "-nobrowse",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&dmg),
        "Could not open the disk image",
    )?;
    let copied = run(
        Command::new("/usr/bin/ditto")
            .arg(mount.join(APP_NAME))
            .arg(&replacement),
        "Could not stage the updated app",
    );
    let detached = run(
        Command::new("/usr/bin/hdiutil").arg("detach").arg(&mount),
        "Could not eject the disk image",
    );
    copied?;
    detached?;

    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&replacement),
        "Could not verify the updated app's code signature",
    )?;
    if plist(&replacement, "CFBundleIdentifier")? != BUNDLE_ID
        || plist(&replacement, "CFBundleShortVersionString")? != release.version.to_string()
    {
        return Err("The app in the disk image does not match the selected release.".into());
    }
    if plist(bundle, "CFBundleShortVersionString")? != current {
        return Err("The installed app changed while updating. Quit and reopen it.".into());
    }
    swap_bundles(&replacement, bundle)
}

fn swap_bundles(replacement: &Path, installed: &Path) -> Result<(), String> {
    let replacement =
        CString::new(replacement.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    let installed =
        CString::new(installed.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    // Both NUL-terminated paths remain valid for the duration of this call.
    let result =
        unsafe { libc::renamex_np(replacement.as_ptr(), installed.as_ptr(), libc::RENAME_SWAP) };
    if result != 0 {
        return Err(format!(
            "Could not replace the app: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn restart(bundle: &Path) -> io::Result<()> {
    Command::new("/bin/sh")
        .args([
            "-c",
            r#"
            for attempt in $(/usr/bin/jot 100); do
                if ! kill -0 "$1" 2>/dev/null; then
                    exec /usr/bin/open -n "$2"
                fi
                /bin/sleep 0.1
            done
        "#,
            "restart",
        ])
        .arg(std::process::id().to_string())
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "downloads the public release and mounts the macOS installer"]
    fn installs_latest_github_release_into_a_temporary_bundle() {
        let previous = "0.1.0";
        let release = check(previous)
            .unwrap()
            .expect("expected a newer public release");
        assert!(check(&release.version.to_string()).unwrap().is_none());
        assert!(!release.notes.trim().is_empty());

        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join(APP_NAME);
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(
            bundle.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
            <key>CFBundleShortVersionString</key><string>{previous}</string>
            </dict></plist>"#
            ),
        )
        .unwrap();
        install(&release, &bundle, previous).unwrap();

        assert_eq!(
            plist(&bundle, "CFBundleShortVersionString").unwrap(),
            release.version.to_string()
        );
        assert!(bundle.join("Contents/MacOS/cluster-observatory").is_file());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
