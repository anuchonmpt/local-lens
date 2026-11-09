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
    Restarting,
    RestartRequired(String),
    Error(String),
}

#[derive(Default)]
pub struct Updates {
    visible: bool,
    state: State,
    result: Option<mpsc::Receiver<State>>,
}

impl Updates {
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.result else { return };
        self.state = match receiver.try_recv() {
            Ok(state) => state,
            Err(mpsc::TryRecvError::Disconnected) => {
                State::Error("The update task stopped unexpectedly.".into())
            }
            Err(mpsc::TryRecvError::Empty) => return,
        };
        self.result = None;
        if matches!(self.state, State::Restarting) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let (update_label, update_hint) = match &self.state {
            State::Available(release) => (
                format!("Update to v{}", release.version),
                "Install the update and reopen the app.",
            ),
            State::Installing => ("Updating…".into(), "Downloading and installing the update."),
            State::Restarting => ("Restarting…".into(), "Opening the new version."),
            State::RestartRequired(_) => (
                "Restart needed".into(),
                "The update was installed. Quit and reopen the app.",
            ),
            State::Current => ("Update".into(), "You're up to date."),
            _ => ("Update".into(), "Check for a newer version first."),
        };
        let available = matches!(self.state, State::Available(_));
        if ui
            .add_enabled(available, egui::Button::new(update_label))
            .on_hover_text(update_hint)
            .on_disabled_hover_text(update_hint)
            .clicked()
        {
            if let State::Available(release) = &self.state {
                let release = release.clone();
                self.start(ui.ctx(), State::Installing, move || {
                    let bundle = running_bundle()?;
                    install(&release, &bundle, VERSION)?;
                    Ok(match restart(&bundle) {
                        Ok(()) => State::Restarting,
                        Err(error) => State::RestartRequired(format!(
                            "The update was installed. Quit and reopen the app. ({error})"
                        )),
                    })
                });
            }
        }

        let (check_label, check_hint) = match &self.state {
            State::Checking => ("Checking…", "Checking GitHub Releases."),
            State::Current => ("Up to date", "You're up to date. Click to check again."),
            State::Error(error) => ("Retry", error.as_str()),
            State::RestartRequired(error) => ("Check", error.as_str()),
            _ => ("Check", "Check for updates."),
        };
        let can_check = !matches!(self.state, State::Restarting | State::RestartRequired(_))
            && self.result.is_none();
        if ui
            .add_enabled(can_check, egui::Button::new(check_label))
            .on_hover_text(check_hint)
            .on_disabled_hover_text(check_hint)
            .clicked()
        {
            self.check_for_updates(ui.ctx());
        }
    }

    pub fn check_for_updates(&mut self, ctx: &egui::Context) {
        self.start(ctx, State::Checking, || {
            Ok(check(VERSION)?.map_or(State::Current, State::Available))
        });
    }

    pub fn footer(&mut self, ui: &mut egui::Ui) {
        let (label, color, hint) = if matches!(self.state, State::Available(_)) {
            (
                concat!("v", env!("CARGO_PKG_VERSION"), " ↑"),
                crate::GREEN,
                "Update available · View changelog",
            )
        } else {
            (
                concat!("v", env!("CARGO_PKG_VERSION")),
                crate::MUTED,
                "View changelog",
            )
        };
        let version = egui::RichText::new(label).size(11.0).color(color);
        if ui
            .add(egui::Button::new(version).small().frame(false))
            .on_hover_text(hint)
            .clicked()
        {
            self.visible = true;
        }
    }

    pub fn show_history(&mut self, ctx: &egui::Context) {
        if !self.visible {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("changelog")).show(ctx, |ui| {
            ui.set_width(480.0);
            let close = ui
                .horizontal(|ui| {
                    ui.heading("Changelog");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.button("×").on_hover_text("Close changelog").clicked()
                    })
                    .inner
                })
                .inner;
            ui.horizontal(|ui| self.controls(ui));
            egui::ScrollArea::vertical()
                .max_height(ctx.screen_rect().height() * 0.65)
                .show(ui, |ui| {
                    if let State::Available(release) = &self.state {
                        if !release.notes.trim().is_empty() {
                            ui.collapsing(format!("What's New in v{}", release.version), |ui| {
                                show_changelog(ui, &release.notes);
                            });
                        }
                    }
                    show_changelog(ui, CHANGELOG);
                });
            ui.separator();
            ui.hyperlink_to("GitHub Releases", RELEASES);
            close
        });
        if modal.inner || modal.should_close() {
            self.visible = false;
        }
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

fn show_changelog(ui: &mut egui::Ui, notes: &str) {
    for line in notes.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            ui.add_space(10.0);
            ui.strong(heading);
            ui.separator();
        } else if let Some(item) = line.strip_prefix("- ") {
            ui.horizontal_top(|ui| {
                ui.label("•");
                ui.add(egui::Label::new(item).wrap());
            });
        } else if !line.is_empty() && !line.starts_with("# ") {
            ui.label(line);
        }
    }
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
    let asset_id = asset["id"]
        .as_u64()
        .ok_or("GitHub did not provide an installer ID.")?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/assets/{asset_id}");
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
                "--header",
                "Accept: application/octet-stream",
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
    fn footer_opens_update_controls_and_marks_an_available_update() {
        let ctx = egui::Context::default();
        let mut updates = Updates {
            state: State::Current,
            ..Default::default()
        };
        let frame = |updates: &mut Updates, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.horizontal(|ui| updates.footer(ui));
                    });
                    updates.show_history(ctx);
                },
            )
        };
        let click = |updates: &mut Updates, label| {
            let pos = frame(updates, vec![])
                .shapes
                .into_iter()
                .find_map(|shape| match shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .expect("update button label");
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(updates, vec![egui::Event::PointerMoved(pos), button(true)]);
            frame(updates, vec![button(false)]);
        };

        assert!(!frame(&mut updates, vec![]).shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "Update")
        }));
        click(&mut updates, concat!("v", env!("CARGO_PKG_VERSION")));
        assert!(updates.visible);
        click(&mut updates, "Update");
        assert!(matches!(updates.state, State::Current));
        click(&mut updates, "×");
        assert!(!updates.visible);

        updates.state = State::Available(Release {
            version: Version::new(1, 0, 0),
            url: String::new(),
            sha256: String::new(),
            notes: String::new(),
        });
        click(&mut updates, concat!("v", env!("CARGO_PKG_VERSION"), " ↑"));
        assert!(updates.visible);
        click(&mut updates, "Update to v1.0.0");
        assert!(matches!(updates.state, State::Installing));
    }

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
