use crate::{containers, pod_args, ui_icons, Resource, Stream, Target};
use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const CHROME: Color32 = Color32::from_rgb(24, 27, 32);
const SURFACE: Color32 = Color32::from_rgb(16, 19, 23);
const EDGE: Color32 = Color32::from_rgb(56, 63, 73);
const INK: Color32 = Color32::from_rgb(232, 237, 243);
const LOG_TIMESTAMP: Color32 = Color32::from_rgb(178, 190, 204);
const ACCENT: Color32 = Color32::from_rgb(146, 198, 252);

pub(super) fn clean_log_text(output: &str) -> std::borrow::Cow<'_, str> {
    if !output.contains("\x1b[") {
        return output.into();
    }
    let mut clean = String::with_capacity(output.len());
    let mut rest = output;
    while let Some((plain, escape)) = rest.split_once("\x1b[") {
        clean.push_str(plain);
        let parameters = escape
            .bytes()
            .take_while(|byte| (0x20..=0x3f).contains(byte))
            .count();
        rest = if escape
            .as_bytes()
            .get(parameters)
            .is_some_and(|byte| (0x40..=0x7e).contains(byte))
        {
            &escape[parameters + 1..]
        } else {
            escape
        };
    }
    clean.push_str(rest);
    clean.into()
}

pub(super) fn log_level(message: &str) -> Option<(&str, Color32)> {
    let token = message.split_whitespace().next()?;
    let level = if token.starts_with('[') {
        let (_, end) = token.split_once(']')?;
        &token[..token.len() - end.len()]
    } else {
        token.split(['[', ':']).next()?
    };
    let color = match level.trim_matches(['[', ']']).to_ascii_uppercase().as_str() {
        "ERROR" | "ERR" | "FATAL" | "PANIC" => Color32::from_rgb(251, 135, 148),
        "WARN" | "WARNING" => Color32::from_rgb(248, 191, 111),
        "INFO" => Color32::from_rgb(119, 208, 192),
        "DEBUG" | "TRACE" => LOG_TIMESTAMP,
        _ => return None,
    };
    Some((level, color))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    Recent,
    #[default]
    Follow,
    Previous,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Range {
    #[default]
    All,
    FifteenMinutes,
    Hour,
    Day,
}

impl Range {
    fn label(self) -> &'static str {
        match self {
            Self::All => "Last 200 lines",
            Self::FifteenMinutes => "Last 15 minutes",
            Self::Hour => "Last hour",
            Self::Day => "Last 24 hours",
        }
    }

    fn flag(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::FifteenMinutes => Some("--since=15m"),
            Self::Hour => Some("--since=1h"),
            Self::Day => Some("--since=24h"),
        }
    }
}

pub enum Event {
    Close,
    Pod,
    Namespace,
}

pub struct PodLogs {
    pub pod: Resource,
    pub container: String,
    pub stream: Option<Result<Stream, String>>,
    pub expanded: bool,
    mode: Mode,
    range: Range,
    query: String,
    filter: bool,
    case: bool,
    regex: bool,
    compiled_query: Result<Option<regex::Regex>, String>,
    timestamps: bool,
    names: bool,
    wrap: bool,
    match_index: usize,
    follow_scroll: bool,
    first_stamp: Option<OffsetDateTime>,
    export_notice: Option<String>,
}

impl PodLogs {
    pub fn new(pod: Resource, container: String) -> Self {
        Self {
            pod,
            container,
            stream: None,
            expanded: false,
            mode: Mode::Follow,
            range: Range::All,
            query: String::new(),
            filter: true,
            case: false,
            regex: false,
            compiled_query: Ok(None),
            timestamps: true,
            names: false,
            wrap: false,
            match_index: 0,
            follow_scroll: true,
            first_stamp: None,
            export_notice: None,
        }
    }

    pub fn follow(&mut self, target: Option<&Target>) {
        self.load(target, Mode::Follow);
    }

    fn args(&self, mode: Mode) -> Vec<String> {
        let mut args = pod_args(
            "logs",
            &self.pod,
            &self.container,
            &["--tail=200", "--timestamps=true"],
        );
        match mode {
            Mode::Recent => {}
            Mode::Follow => args.push("--follow".into()),
            Mode::Previous => args.push("--previous".into()),
        }
        if mode != Mode::Follow {
            args.push("--request-timeout=15s".into());
        }
        if let Some(since) = self.range.flag() {
            args.push(since.into());
        }
        args
    }

    fn load(&mut self, target: Option<&Target>, mode: Mode) {
        self.mode = mode;
        self.first_stamp = None;
        self.match_index = 0;
        self.follow_scroll = mode == Mode::Follow;
        self.export_notice = None;
        self.stream = target
            .filter(|_| !self.container.is_empty())
            .map(|target| Stream::start(target, &self.args(mode)));
    }

    fn choose_container(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        for name in containers(&self.pod) {
            changed |= ui
                .selectable_value(&mut self.container, name.clone(), name)
                .changed();
        }
        changed
    }

    pub fn show(&mut self, ui: &mut egui::Ui, target: Option<&Target>) -> Option<Event> {
        ui.scope(|ui| {
            let visuals = ui.visuals_mut();
            *visuals = egui::Visuals::dark();
            visuals.override_text_color = Some(INK);
            visuals.extreme_bg_color = SURFACE;
            visuals.hyperlink_color = ACCENT;
            visuals.window_fill = CHROME;
            visuals.selection.bg_fill = Color32::from_rgb(36, 58, 82);
            for widget in [&mut visuals.widgets.inactive, &mut visuals.widgets.hovered, &mut visuals.widgets.active, &mut visuals.widgets.open] {
                widget.weak_bg_fill = Color32::from_rgb(32, 37, 44);
                widget.bg_fill = widget.weak_bg_fill;
                widget.bg_stroke = Stroke::new(1.0_f32, EDGE);
                widget.corner_radius = 4.into();
            }
            visuals.widgets.hovered.bg_stroke.color = ACCENT;
            let mut event = None;
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 124.0).clamp(160.0, 358.0);
                let (tab, _) = ui.allocate_exact_size(egui::vec2(width, 32.0), egui::Sense::hover());
                let visuals = ui.painter().with_clip_rect(tab.shrink2(egui::vec2(1.0, 0.0)));
                visuals.rect(tab, 4.0, Color32::from_rgb(32, 37, 44), Stroke::new(1.0_f32, EDGE), egui::StrokeKind::Inside);
                ui_icons::draw(&visuals, tab.left_center() + egui::vec2(17.0, 0.0), "logs", Stroke::new(1.3_f32, Color32::from_rgb(161, 222, 188)));
                visuals.text(tab.left_center() + egui::vec2(34.0, 0.0), egui::Align2::LEFT_CENTER,
                    format!("{}/{}", self.pod.namespace, self.pod.name), FontId::monospace(12.0), LOG_TIMESTAMP);
                ui.menu_button("+", |ui| {
                    if self.choose_container(ui) {
                        self.follow(target);
                        ui.close_menu();
                    }
                }).response.on_hover_text("Choose a log container");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_icons::button(ui, "close", "Close logs", true, false).clicked() { event = Some(Event::Close); }
                    let (icon, label) = if self.expanded { ("restore", "Restore log dock") } else { ("expand", "Expand logs") };
                    if ui_icons::button(ui, icon, label, true, self.expanded).clicked() { self.expanded = !self.expanded; }
                    if self.mode == Mode::Follow && matches!(&self.stream, Some(Ok(stream)) if stream.status.is_none()) {
                        ui.colored_label(Color32::from_rgb(161, 222, 188), "Live");
                    }
                });
            });
            if event.is_some() { return event; }

            let mut jump = false;
            let mut step = 0_i32;
            let mut query_changed = false;
            let mut copy = false;
            #[cfg(target_os = "macos")]
            let mut download = false;
            let mut counter = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let mut changed = false;
                egui::ComboBox::from_id_salt("log-container")
                    .width(166.0).selected_text(&self.container).show_ui(ui, |ui| { changed = self.choose_container(ui); });
                if changed { self.follow(target); }
                ui.separator();
                if ui_icons::button(ui, "filter", "Show matching lines only", true, self.filter).clicked() {
                    self.filter = !self.filter;
                }
                if ui.selectable_label(self.case, "Aa").on_hover_text("Match case").clicked() {
                    self.case = !self.case;
                    query_changed = true;
                }
                if ui.selectable_label(self.regex, ".*").on_hover_text("Regular expression").clicked() {
                    self.regex = !self.regex;
                    query_changed = true;
                }
                let search_width = (ui.available_width() - 440.0).clamp(130.0, 270.0);
                let search = ui.add_sized([search_width, 28.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text("Search in logs…").font(FontId::proportional(12.0)));
                query_changed |= search.changed();
                if search.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                }
                counter = Some(ui.allocate_space(egui::vec2(66.0, 20.0)).1);
                if ui_icons::button(ui, "up", "Previous match", !self.query.is_empty(), false).clicked() { step = -1; }
                if ui_icons::button(ui, "down", "Next match", !self.query.is_empty(), false).clicked() { step = 1; }
                ui.separator();
                if ui_icons::button(ui, "time", "Show timestamps", true, self.timestamps).clicked() { self.timestamps = !self.timestamps; }
                if ui_icons::button(ui, "previous", "Previous container logs", !self.container.is_empty(), self.mode == Mode::Previous).clicked() {
                    self.load(target, if self.mode == Mode::Previous { Mode::Follow } else { Mode::Previous });
                }
                if ui_icons::button(ui, "tag", "Show Pod and container names", true, self.names).clicked() { self.names = !self.names; }
                if ui_icons::button(ui, "wrap", "Wrap lines", true, self.wrap).clicked() { self.wrap = !self.wrap; }
                #[cfg(target_os = "macos")]
                if ui_icons::button(ui, "download", "Download retained logs", true, false).clicked() { download = true; }
                ui.label(RichText::new(if target.is_none() && self.range == Range::All { "All sample logs" } else { self.range.label() })
                    .size(12.0).color(LOG_TIMESTAMP));
                let range_menu = egui::menu::menu_custom_button(ui, egui::Button::new("").min_size(egui::vec2(28.0, 26.0)), |ui| {
                    for range in [Range::All, Range::FifteenMinutes, Range::Hour, Range::Day] {
                        if ui.selectable_label(self.range == range, range.label()).clicked() {
                            self.range = range;
                            self.load(target, self.mode);
                            ui.close_menu();
                        }
                    }
                }).response.on_hover_text("Time range");
                ui_icons::draw(ui.painter(), range_menu.rect.center(), "calendar", Stroke::new(1.3_f32, LOG_TIMESTAMP));
                range_menu.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Time range"));
                if ui_icons::button(ui, "latest", "Jump to latest log", true, false).clicked() { jump = true; }
                let session = egui::menu::menu_custom_button(ui, egui::Button::new("").min_size(egui::vec2(28.0, 26.0)), |ui| {
                    for (label, mode) in [("Recent", Mode::Recent), ("Follow", Mode::Follow), ("Previous", Mode::Previous)] {
                        if ui.add_enabled(!self.container.is_empty(), egui::Button::new(label).selected(self.mode == mode)).clicked() {
                            self.load(target, mode);
                            ui.close_menu();
                        }
                    }
                    if let Some(Ok(stream)) = &mut self.stream {
                        if ui.add_enabled(stream.status.is_none(), egui::Button::new("Stop")).clicked() {
                            stream.stop();
                            ui.close_menu();
                        }
                    }
                    if ui.button("Copy").clicked() { copy = true; ui.close_menu(); }
                }).response.on_hover_text("Log session");
                ui_icons::draw(ui.painter(), session.rect.center(), "more", Stroke::new(1.3_f32, LOG_TIMESTAMP));
                session.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Log session"));
            });

            let output = if target.is_none() { self.sample().to_owned() } else {
                self.stream.as_ref().and_then(|result| result.as_ref().ok()).map(Stream::output).unwrap_or_default()
            };
            let output = clean_log_text(&output);
            if copy { ui.ctx().copy_text(output.to_string()); }
            #[cfg(target_os = "macos")]
            if download && !output.is_empty() {
                let suggested = format!("{}-{}.log", self.pod.name, self.container);
                if let Some(path) = rfd::FileDialog::new().set_file_name(&suggested).save_file() {
                    self.export_notice = std::fs::write(path, output.as_bytes()).err().map(|error| format!("Could not save logs: {error}"));
                }
            }

            let lines: Vec<_> = output.lines().map(split_timestamp).collect();
            self.first_stamp = self.first_stamp.or_else(|| lines.iter().find_map(|(stamp, _)| stamp.and_then(|stamp| OffsetDateTime::parse(stamp, &Rfc3339).ok())));
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Displaying logs from Namespace:").size(12.0).color(LOG_TIMESTAMP));
                if ui.link(RichText::new(&self.pod.namespace).size(12.0).color(ACCENT).underline()).clicked() { event = Some(Event::Namespace); }
                ui.label(RichText::new("for Pod:").size(12.0).color(LOG_TIMESTAMP));
                if ui.link(RichText::new(&self.pod.name).size(12.0).color(ACCENT).underline()).clicked() { event = Some(Event::Pod); }
                if let Some(stamp) = self.first_stamp {
                    ui.label(RichText::new(format!("· Logs from {}", display_start(stamp, time::UtcOffset::local_offset_at(stamp).unwrap_or(time::UtcOffset::UTC)))).size(12.0).color(LOG_TIMESTAMP));
                }
                if self.mode == Mode::Previous { ui.label(RichText::new("· Previous instance").size(12.0).color(LOG_TIMESTAMP)); }
                if target.is_none() { ui.label(RichText::new("· Sample data").size(12.0).color(LOG_TIMESTAMP)); }
            });
            if query_changed {
                self.match_index = 0;
                self.compiled_query = matcher(&self.query, self.case, self.regex);
            }
            let (search, search_error) = match &self.compiled_query {
                Ok(value) => (value.as_ref(), None), Err(error) => (None, Some(error.as_str())),
            };
            let matches: Vec<_> = search.as_ref().map(|search| {
                output.lines().enumerate().filter_map(|(index, line)| search.is_match(line).then_some(index)).collect()
            }).unwrap_or_default();
            let mut scroll_to = None;
            if !matches.is_empty() {
                self.match_index = self.match_index.min(matches.len() - 1);
                if step != 0 || query_changed {
                    if !query_changed { self.match_index = (self.match_index as i32 + step).rem_euclid(matches.len() as i32) as usize; }
                    scroll_to = Some(matches[self.match_index]);
                    self.follow_scroll = false;
                }
            }
            if let Some(rect) = counter {
                let count = if matches.is_empty() { 0 } else { self.match_index + 1 };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("{count} / {}", matches.len()), FontId::monospace(11.0), LOG_TIMESTAMP);
            }
            if jump { self.follow_scroll = self.mode == Mode::Follow; }
            if let Some(error) = search_error { ui.colored_label(Color32::LIGHT_RED, error); }
            if let Some(error) = &self.export_notice { ui.colored_label(Color32::LIGHT_RED, error); }
            let display = lines.iter().enumerate().filter(|(index, _)| !self.filter || search.is_none() || matches.binary_search(index).is_ok());
            let visible: Vec<_> = display.collect();
            let status = match &self.stream {
                Some(Err(error)) => Some(error.as_str()),
                Some(Ok(stream)) => stream.status.as_deref(),
                None => None,
            };
            if let Some(error) = status.filter(|_| matches!(&self.stream, Some(Err(_)))) {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
            egui::Frame::new().fill(SURFACE).stroke(Stroke::new(1.0_f32, EDGE)).corner_radius(5).inner_margin(10)
                .show(ui, |ui| {
                    let scroll = if self.wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
                    scroll.id_salt(("pod-log-output", &self.pod.namespace, &self.pod.name, &self.container, self.wrap))
                        .auto_shrink([false, false]).stick_to_bottom(self.follow_scroll).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            if visible.is_empty() {
                                let empty = if !output.is_empty() { "No log lines match the filter." }
                                    else if matches!(&self.stream, Some(Ok(stream)) if stream.status.is_none()) { "Waiting for log output…" }
                                    else { "This container returned no log output." };
                                ui.colored_label(LOG_TIMESTAMP, empty);
                            }
                            if !visible.is_empty() {
                                let name = self.names.then(|| format!("{}/{}", self.pod.name, self.container));
                                let mut job = egui::text::LayoutJob::default();
                                let mut scroll_char = None;
                                for (offset, (index, (timestamp, message))) in visible.into_iter().enumerate() {
                                    if offset > 0 { job.append("\n", 0.0, line_format(INK, Color32::TRANSPARENT)); }
                                    if scroll_to == Some(index) { scroll_char = Some(job.text.chars().count()); }
                                    let highlighted = matches.binary_search(&index).is_ok();
                                    let active = highlighted && matches.get(self.match_index) == Some(&index);
                                    append_log_line(&mut job, timestamp.filter(|_| self.timestamps), message,
                                        name.as_deref(), highlighted, active);
                                }
                                if self.wrap { job.wrap.max_width = ui.available_width(); job.wrap.break_anywhere = true; }
                                let galley = ui.fonts(|fonts| fonts.layout_job(job));
                                let scroll_rect = scroll_char.map(|index| {
                                    let cursor = galley.from_ccursor(egui::text::CCursor { index, prefer_next_row: true });
                                    galley.pos_from_cursor(&cursor)
                                });
                                let response = ui.add(egui::Label::new(galley).extend().selectable(true));
                                if let Some(rect) = scroll_rect {
                                    ui.scroll_to_rect(rect.translate(response.rect.min.to_vec2()), Some(egui::Align::Center));
                                }
                            }
                            if let Some(status) = status.filter(|_| !matches!(&self.stream, Some(Err(_)))) { ui.colored_label(LOG_TIMESTAMP, status); }
                            if jump { ui.scroll_to_cursor(Some(egui::Align::BOTTOM)); }
                        });
                });
            event
        }).inner
    }

    fn sample(&self) -> &'static str {
        if self.container.starts_with("wait-") {
            "2026-11-09T07:22:02Z INFO Waiting for redis:6379\n2026-11-09T07:22:03Z INFO Redis is reachable"
        } else if self.mode == Mode::Previous {
            "2026-11-09T07:24:27Z INFO Starting service\n2026-11-09T07:24:28Z WARN Connection timed out\n2026-11-09T07:24:28Z ERROR Startup aborted with exit code 1"
        } else if self.pod.status == "CrashLoopBackOff" {
            "2026-11-09T07:25:44Z INFO Starting payments service on :8080\n2026-11-09T07:25:44Z INFO Loaded configuration\n2026-11-09T07:25:45Z ERROR Database connection timed out\n2026-11-09T07:25:45Z ERROR Startup aborted with exit code 1"
        } else {
            "2026-11-09T06:46:21Z INFO Service initialized\n2026-11-09T06:46:22Z INFO Readiness check passed"
        }
    }
}

fn display_start(timestamp: OffsetDateTime, offset: time::UtcOffset) -> String {
    let stamp = timestamp.to_offset(offset);
    let zone = if offset.is_utc() {
        "UTC".into()
    } else {
        let seconds = offset.whole_seconds().unsigned_abs();
        let sign = if offset.is_negative() { '-' } else { '+' };
        let minutes = (seconds / 60) % 60;
        if minutes == 0 {
            format!("GMT{sign}{}", seconds / 3600)
        } else {
            format!("GMT{sign}{}:{minutes:02}", seconds / 3600)
        }
    };
    let hour = (stamp.hour() + 11) % 12 + 1;
    let meridiem = if stamp.hour() >= 12 { "PM" } else { "AM" };
    let month = stamp.month().to_string();
    format!(
        "{}, {}, {}:{:02}:{:02} {meridiem} {zone}",
        format_args!("{} {}", &month[..3], stamp.day()),
        stamp.year(),
        hour,
        stamp.minute(),
        stamp.second()
    )
}

fn split_timestamp(line: &str) -> (Option<&str>, &str) {
    if let Some((stamp, message)) = line.split_once(' ') {
        if stamp.as_bytes().get(10) == Some(&b'T') && OffsetDateTime::parse(stamp, &Rfc3339).is_ok()
        {
            return (Some(stamp), message);
        }
    }
    (None, line)
}

fn matcher(query: &str, case: bool, regex: bool) -> Result<Option<regex::Regex>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    let pattern = if regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case)
        .build()
        .map(Some)
        .map_err(|error| {
            format!(
                "Invalid regular expression: {}",
                error
                    .to_string()
                    .lines()
                    .last()
                    .unwrap_or_default()
                    .trim_start_matches("error: ")
            )
        })
}

fn line_format(color: Color32, background: Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: FontId::monospace(12.0),
        line_height: Some(19.0),
        color,
        background,
        ..Default::default()
    }
}

fn append_log_line(
    job: &mut egui::text::LayoutJob,
    timestamp: Option<&str>,
    mut message: &str,
    name: Option<&str>,
    matched: bool,
    active: bool,
) {
    let background = if active {
        Color32::from_rgb(43, 66, 95)
    } else if matched {
        Color32::from_rgb(36, 46, 58)
    } else {
        Color32::TRANSPARENT
    };
    let format = |color| line_format(color, background);
    if let Some(name) = name {
        job.append(&format!("[{name}]  "), 0.0, format(ACCENT));
    }
    if let Some(timestamp) = timestamp {
        job.append(&format!("{timestamp}  "), 0.0, format(LOG_TIMESTAMP));
    }
    if let Some((level, color)) = log_level(message) {
        let start = message.len() - message.trim_start().len();
        let end = start + level.len();
        job.append(&message[..start], 0.0, format(INK));
        job.append(&message[start..end], 0.0, format(color));
        message = &message[end..];
    }
    job.append(message, 0.0, format(INK));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_treats_literals_and_regex_differently() {
        assert!(matcher("DB.error", false, false)
            .unwrap()
            .unwrap()
            .is_match("db.error"));
        assert!(!matcher("DB.error", true, false)
            .unwrap()
            .unwrap()
            .is_match("db.error"));
        assert!(!matcher("DB.error", false, false)
            .unwrap()
            .unwrap()
            .is_match("DB error"));
        assert!(matcher("ERROR|WARN", true, true)
            .unwrap()
            .unwrap()
            .is_match("WARN timeout"));
        assert!(matcher("[", true, true).is_err());
    }

    #[test]
    fn previous_and_range_keep_namespace_and_container() {
        let pod = Resource::parse(
            serde_json::json!({"kind":"Pod", "metadata":{"name":"sample","namespace":"demo"}}),
        );
        let mut logs = PodLogs::new(pod, "app".into());
        logs.range = Range::Hour;
        assert_eq!(
            logs.args(Mode::Previous),
            [
                "logs",
                "sample",
                "-n",
                "demo",
                "-c",
                "app",
                "--tail=200",
                "--timestamps=true",
                "--previous",
                "--request-timeout=15s",
                "--since=1h"
            ]
        );
        assert!(!logs
            .args(Mode::Follow)
            .iter()
            .any(|arg| arg == "--previous" || arg.starts_with("--request-timeout=")));
    }

    #[cfg(unix)]
    #[test]
    fn live_and_previous_sessions_read_output_from_a_scoped_kubectl_process() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let fixture = tempfile::tempdir().unwrap();
        let executable = fixture.path().join("kubectl");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' "$*"
printf '%s\n' '2026-11-09T07:25:44Z INFO output from fixture'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = Target {
            exe: executable.to_string_lossy().into_owned(),
            config: "fixture.yaml".into(),
            context: "test-context".into(),
        };
        let pod = Resource::parse(
            serde_json::json!({"kind":"Pod","metadata":{"name":"payments","namespace":"test"}}),
        );
        let mut logs = PodLogs::new(pod, "worker".into());

        for mode in [Mode::Recent, Mode::Follow, Mode::Previous] {
            logs.load(Some(&target), mode);
            let stream = logs.stream.as_mut().unwrap().as_mut().unwrap();
            let until = Instant::now() + Duration::from_secs(3);
            let output = loop {
                stream.poll();
                let output = stream.output();
                if output.contains("output from fixture") || Instant::now() >= until {
                    break output;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(output.starts_with(
                "--kubeconfig fixture.yaml --context test-context logs payments -n test -c worker"
            ));
            assert_eq!(output.contains("--follow"), mode == Mode::Follow);
            assert_eq!(output.contains("--previous"), mode == Mode::Previous);
            assert!(output.contains("2026-11-09T07:25:44Z INFO output from fixture"));
        }
    }

    #[test]
    fn log_start_uses_actual_timestamp_and_labels_the_timezone() {
        let (stamp, message) = split_timestamp("2026-11-09T14:25:44+07:00 INFO Started");
        assert_eq!(message, "INFO Started");
        let stamp = OffsetDateTime::parse(stamp.unwrap(), &Rfc3339).unwrap();
        assert_eq!(
            display_start(stamp, time::UtcOffset::UTC),
            "Nov 9, 2026, 7:25:44 AM UTC"
        );
        assert_eq!(
            display_start(stamp, time::UtcOffset::from_hms(7, 0, 0).unwrap()),
            "Nov 9, 2026, 2:25:44 PM GMT+7"
        );
        assert_eq!(split_timestamp("2026-11-09TgarbageZ INFO Started").0, None);
    }
}
