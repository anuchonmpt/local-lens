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
    Skipped(Version),
    Installing,
    Restarting,
    RestartRequired(String),
    Error(String),
}

#[derive(Default)]
pub struct Updates {
    state: State,
    result: Option<mpsc::Receiver<State>>,
    show_result: bool,
    skip_file: Option<PathBuf>,
    skipped: Option<Version>,
}

impl Updates {
    pub fn load(settings_dir: Option<&Path>) -> Self {
        let skip_file = settings_dir.map(|dir| dir.join("skipped-update-version"));
        let skipped = skip_file
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|version| Version::parse(version.trim()).ok());
        Self {
            skip_file,
            skipped,
            ..Self::default()
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.result else { return };
        self.state = match receiver.try_recv() {
            Ok(State::Available(release)) if self.skipped.as_ref() == Some(&release.version) => {
                State::Skipped(release.version)
            }
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

    pub fn check_for_updates(&mut self, ctx: &egui::Context) {
        if self.result.is_none() {
            self.start(ctx, State::Checking, || {
                Ok(check(VERSION)?.map_or(State::Current, State::Available))
            });
        }
    }

    pub fn footer(&mut self, ui: &mut egui::Ui) -> bool {
        let available = matches!(self.state, State::Available(_));
        let (color, hint) = if available {
            (crate::GREEN, "Update available · Version menu")
        } else if matches!(self.state, State::Checking) {
            (crate::MUTED, "Checking for updates…")
        } else {
            (crate::MUTED, "Version menu")
        };
        let label = concat!("v", env!("CARGO_PKG_VERSION"));
        let version =
            ui.painter()
                .layout_no_wrap(label.into(), egui::FontId::proportional(11.0), color);
        let padding = if available {
            egui::vec2(7.0, 3.0)
        } else {
            egui::Vec2::ZERO
        };
        let arrow_space = if available { 15.0 } else { 0.0 };
        let size = version.size() + 2.0 * padding + egui::vec2(arrow_space, 0.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
        });
        if available {
            let fill = if response.hovered() {
                egui::Color32::from_rgb(39, 75, 52)
            } else {
                egui::Color32::from_rgb(29, 57, 40)
            };
            ui.painter().rect_filled(rect, 5.0, fill);
            ui.painter().arrow(
                rect.right_center() + egui::vec2(-10.0, 4.0),
                egui::vec2(0.0, -8.0),
                egui::Stroke::new(1.25_f32, color),
            );
        }
        ui.painter().galley(rect.min + padding, version, color);
        let response = response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(hint);
        let menu = ui.make_persistent_id("version-menu");
        if response.clicked() {
            ui.memory_mut(|memory| memory.toggle_popup(menu));
        }
        egui::popup::popup_above_or_below_widget(
            ui,
            menu,
            &response,
            egui::AboveOrBelow::Above,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(155.0);
                let can_check = !matches!(
                    self.state,
                    State::Installing | State::Restarting | State::RestartRequired(_)
                );
                let check_label = if self.show_result && matches!(self.state, State::Checking) {
                    "Checking…"
                } else {
                    "Check Update"
                };
                if ui
                    .add_enabled(
                        can_check && check_label == "Check Update",
                        egui::Button::new(check_label),
                    )
                    .clicked()
                {
                    ui.memory_mut(|memory| memory.close_popup());
                    self.show_result = true;
                    self.check_for_updates(ui.ctx());
                }
                if ui.button("View Changelog").clicked() {
                    ui.memory_mut(|memory| memory.close_popup());
                    return true;
                }
                false
            },
        )
        .unwrap_or(false)
    }

    pub fn dialog(&mut self, ctx: &egui::Context) {
        if !self.show_result || matches!(self.state, State::Checking | State::Restarting) {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("update-result")).show(ctx, |ui| {
            ui.set_width(420.0);
            if let State::Available(release) = &self.state {
                ui.heading(format!("Version v{} is available", release.version));
                ui.label("Update Cluster Observatory now?");
                if !release.notes.trim().is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            show_changelog(ui, &release.notes);
                        });
                }
                ui.add_space(10.0);
                let (update, skip, cancel) = ui
                    .horizontal(|ui| {
                        let update = egui::Button::new(
                            egui::RichText::new("Update")
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(32, 113, 68))
                        .min_size(egui::vec2(100.0, 32.0));
                        (
                            ui.add(update).clicked(),
                            ui.button("Skip Version").clicked(),
                            ui.button("Cancel").clicked(),
                        )
                    })
                    .inner;
                if update {
                    self.install_update(ctx, release.clone());
                } else if skip {
                    let version = release.version.clone();
                    self.state = match self.skip_version(&version) {
                        Ok(()) => {
                            self.show_result = false;
                            State::Skipped(version)
                        }
                        Err(error) => {
                            State::Error(format!("Could not save skipped version: {error}"))
                        }
                    };
                }
                return cancel;
            }

            ui.heading("Check Update");
            match &self.state {
                State::Current => {
                    ui.label(format!("You're up to date on v{VERSION}."));
                }
                State::Skipped(version) => {
                    ui.label(format!("Version v{version} was skipped."));
                }
                State::Installing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Downloading and installing the update…");
                    });
                    return false;
                }
                State::RestartRequired(message) | State::Error(message) => {
                    ui.label(message);
                }
                _ => {}
            }
            ui.button("OK").clicked()
        });
        if !matches!(self.state, State::Installing) && (modal.inner || modal.should_close()) {
            self.show_result = false;
        }
    }

    fn skip_version(&mut self, version: &Version) -> io::Result<()> {
        if let Some(path) = &self.skip_file {
            let dir = path
                .parent()
                .expect("skip file is under the settings directory");
            fs::create_dir_all(dir)?;
            let file = tempfile::NamedTempFile::new_in(dir)?;
            fs::write(file.path(), version.to_string())?;
            file.persist(path)?;
        }
        self.skipped = Some(version.clone());
        Ok(())
    }

    fn install_update(&mut self, ctx: &egui::Context, release: Release) {
        self.start(ctx, State::Installing, move || {
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

    pub fn show_settings(&self, ui: &mut egui::Ui) {
        ui.label(format!("Installed version v{VERSION}"));
        ui.add_space(12.0);
        ui.heading("Changelog");
        if let State::Available(release) = &self.state {
            if !release.notes.trim().is_empty() {
                ui.collapsing(format!("What's New in v{}", release.version), |ui| {
                    show_changelog(ui, &release.notes);
                });
            }
        }
        show_changelog(ui, CHANGELOG);
        ui.separator();
        ui.hyperlink_to("GitHub Releases", RELEASES);
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
    fn version_menu_checks_and_distinguishes_cancel_from_skip() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let mut updates = Updates::load(Some(dir.path()));
        let changelog_open = std::cell::Cell::new(false);
        let frame = |updates: &mut Updates, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    updates.poll(ctx);
                    egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if updates.footer(ui) {
                                changelog_open.set(true);
                            }
                        });
                    });
                    updates.dialog(ctx);
                },
            )
        };
        let click = |updates: &mut Updates, label| {
            frame(updates, vec![]);
            let pos = frame(updates, vec![])
                .shapes
                .into_iter()
                .find_map(|shape| match shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing button: {label}"));
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(updates, vec![egui::Event::PointerMoved(pos), button(true)]);
            frame(updates, vec![button(false)]);
        };
        let version_label = concat!("v", env!("CARGO_PKG_VERSION"));
        let begin_check = |updates: &mut Updates| {
            let (sender, receiver) = mpsc::channel();
            updates.state = State::Checking;
            updates.result = Some(receiver);
            click(updates, version_label);
            click(updates, "Check Update");
            sender
        };
        let release = |major| {
            State::Available(Release {
                version: Version::new(major, 0, 0),
                url: String::new(),
                sha256: String::new(),
                notes: String::new(),
            })
        };

        click(&mut updates, version_label);
        click(&mut updates, "View Changelog");
        assert!(changelog_open.get());

        begin_check(&mut updates).send(release(1)).unwrap();
        click(&mut updates, "Cancel");
        assert!(matches!(updates.state, State::Available(_)));
        assert!(!updates.show_result);
        assert!(frame(&mut updates, vec![]).shapes.iter().any(|shape| {
            matches!(shape.shape, egui::Shape::LineSegment { points: [from, to], stroke }
                if stroke.color == crate::GREEN && from.x == to.x && from.y > to.y)
        }));

        begin_check(&mut updates).send(release(1)).unwrap();
        click(&mut updates, "Skip Version");
        assert!(matches!(updates.state, State::Skipped(_)));
        assert!(!updates.show_result);

        let mut updates = Updates::load(Some(dir.path()));
        begin_check(&mut updates).send(release(1)).unwrap();
        click(&mut updates, "OK");
        assert!(matches!(updates.state, State::Skipped(_)));

        begin_check(&mut updates).send(release(2)).unwrap();
        click(&mut updates, "Update");
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
