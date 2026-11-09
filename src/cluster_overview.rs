use super::{
    card, detail_metrics, egui, event_for, event_message, event_object, is_master, now,
    relative_age, resource_quantity, text, Align, Color32, Resource, RichText, Sample, Sense,
    Snapshot, Stroke, Vec2, BLUE, BORDER, GREEN, MUTED, ORANGE, PURPLE, REFRESH, SIDE,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

const RANGES: [(&str, u64); 8] = [
    ("1h", 3600),
    ("2h", 7200),
    ("4h", 14400),
    ("24h", 86400),
    ("48h", 172800),
    ("1 week", 604800),
    ("1 month", 2678400),
    ("2 months", 5356800),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Workers,
    ControlPlane,
}

impl Scope {
    const ALL: [Self; 3] = [Self::All, Self::Workers, Self::ControlPlane];

    fn name(self) -> &'static str {
        match self {
            Self::All => "All nodes",
            Self::Workers => "Worker nodes",
            Self::ControlPlane => "Control plane",
        }
    }

    fn includes(self, node: &Resource) -> bool {
        match self {
            Self::All => true,
            Self::Workers => !is_master(node),
            Self::ControlPlane => is_master(node),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Totals {
    usage: Option<f64>,
    requests: Option<f64>,
    limits: Option<f64>,
    allocatable: Option<f64>,
    capacity: Option<f64>,
}

#[derive(Default)]
struct Group {
    nodes: usize,
    totals: [Totals; 3],
    history: Vec<Sample>,
}

pub(super) enum Action<'a> {
    Open(&'a Resource),
    Events,
}

pub(super) struct Overview {
    groups: [Group; 3],
    scope: Scope,
    range: u64,
    memory: bool,
    warning_sort: Option<(usize, bool)>,
}

impl Default for Overview {
    fn default() -> Self {
        Self {
            groups: Default::default(),
            scope: Scope::Workers,
            range: 3600,
            memory: false,
            warning_sort: None,
        }
    }
}

impl Overview {
    pub(super) fn clear(&mut self) {
        self.groups = Default::default();
    }

    pub(super) fn record(&mut self, snapshot: &Snapshot) {
        let nodes = snapshot.kind("Node");
        let pods = snapshot.kind("Pod");
        let complete_pods = !snapshot
            .errors
            .iter()
            .any(|error| error.starts_with("Pods:"));
        for (group, scope) in self.groups.iter_mut().zip(Scope::ALL) {
            let nodes: Vec<_> = nodes
                .iter()
                .copied()
                .filter(|node| scope.includes(node))
                .collect();
            let node_names: HashSet<_> = nodes.iter().map(|node| node.name.as_str()).collect();
            let pods: Vec<_> = pods
                .iter()
                .copied()
                .filter(|pod| {
                    node_names.contains(pod.node.as_str())
                        && !matches!(
                            pod.raw.pointer("/status/phase").and_then(Value::as_str),
                            Some("Succeeded" | "Failed")
                        )
                })
                .collect();
            group.nodes = nodes.len();
            group.totals = summarize(&nodes, &pods, complete_pods);
            let sample = Sample {
                stamp: snapshot.stamp,
                cpu: group.totals[0].usage,
                memory: group.totals[1].usage,
            };
            if let Some(previous) = group
                .history
                .last_mut()
                .filter(|sample| sample.stamp == snapshot.stamp)
            {
                *previous = sample;
            } else {
                group.history.push(sample);
            }
            let earliest = snapshot.stamp.saturating_sub(RANGES[RANGES.len() - 1].1);
            let expired = group
                .history
                .partition_point(|point| point.stamp < earliest);
            group.history.drain(..expired);
        }
    }

    pub(super) fn show<'a>(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &'a Snapshot,
        demo: bool,
    ) -> Option<Action<'a>> {
        let mut index = Scope::ALL
            .iter()
            .position(|scope| *scope == self.scope)
            .unwrap_or(0);
        if self.groups[index].nodes == 0 {
            index = self
                .groups
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, group)| group.nodes > 0)
                .map_or(0, |(index, _)| index);
        }
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("overview-node-scope")
                .selected_text(Scope::ALL[index].name())
                .show_ui(ui, |ui| {
                    for (candidate, scope) in Scope::ALL.into_iter().enumerate() {
                        if ui
                            .add_enabled(
                                self.groups[candidate].nodes > 0,
                                egui::Button::new(scope.name()).selected(index == candidate),
                            )
                            .clicked()
                        {
                            self.scope = scope;
                            index = candidate;
                        }
                    }
                });
            let range_label = RANGES
                .iter()
                .find(|(_, range)| *range == self.range)
                .map_or("1h", |(name, _)| *name);
            egui::ComboBox::from_id_salt("overview-range")
                .selected_text(range_label)
                .show_ui(ui, |ui| {
                    for (name, range) in RANGES {
                        ui.selectable_value(&mut self.range, range, name);
                    }
                });
            ui.selectable_value(&mut self.memory, false, "CPU");
            ui.selectable_value(&mut self.memory, true, "Memory");
            ui.colored_label(
                MUTED,
                if demo {
                    "Demo data"
                } else {
                    "History collected since connecting"
                },
            );
        });
        let group = &self.groups[index];
        let end = now();
        let start = end.saturating_sub(self.range);
        let points: Vec<_> = group
            .history
            .iter()
            .filter(|point| (start..=end).contains(&point.stamp))
            .filter_map(|point| {
                Some((
                    point.stamp,
                    if self.memory {
                        point.memory?
                    } else {
                        point.cpu?
                    },
                ))
            })
            .collect();
        let trend = |ui: &mut egui::Ui| {
            card(ui, |ui| {
                detail_metrics::chart(ui, &points, start, end, REFRESH, self.memory, 198.0)
            });
        };
        let summaries = |ui: &mut egui::Ui| {
            ui.columns(3, |cols| {
                for (index, col) in cols.iter_mut().enumerate() {
                    card(col, |ui| summary_card(ui, index, group.totals[index]));
                }
            });
        };
        if ui.available_width() >= 1060.0 {
            ui.columns(2, |cols| {
                trend(&mut cols[0]);
                summaries(&mut cols[1]);
            });
        } else {
            trend(ui);
            summaries(ui);
        }
        ui.add_space(8.0);
        warnings(ui, snapshot, &mut self.warning_sort)
    }
}

fn summarize(nodes: &[&Resource], pods: &[&Resource], complete_pods: bool) -> [Totals; 3] {
    let node_total = |read: &dyn Fn(&Resource) -> Option<f64>| {
        if nodes.is_empty() {
            return None;
        }
        nodes.iter().map(|node| read(node)).sum::<Option<f64>>()
    };
    ["cpu", "memory", "pods"].map(|resource| {
        let pods_metric = resource == "pods";
        let usage = match resource {
            "cpu" => node_total(&|node| node.cpu),
            "memory" => node_total(&|node| node.memory),
            _ => (complete_pods && !nodes.is_empty()).then_some(pods.len() as f64),
        };
        let requests = (!pods_metric && complete_pods && !nodes.is_empty()).then(|| {
            pods.iter()
                .filter_map(|pod| pod_budget(&pod.raw, "requests", resource))
                .sum()
        });
        let limits = (!pods_metric && complete_pods && !nodes.is_empty()).then(|| {
            pods.iter()
                .filter_map(|pod| pod_budget(&pod.raw, "limits", resource))
                .sum()
        });
        Totals {
            usage,
            requests,
            limits,
            allocatable: node_total(&|node| {
                resource_quantity(&node.raw["status"]["allocatable"][resource], resource)
            }),
            capacity: node_total(&|node| {
                resource_quantity(&node.raw["status"]["capacity"][resource], resource)
            }),
        }
    })
}

fn pod_budget(pod: &Value, field: &str, resource: &str) -> Option<f64> {
    let spec = &pod["spec"];
    let read =
        |container: &Value| resource_quantity(&container["resources"][field][resource], resource);
    let overhead = resource_quantity(&spec["overhead"][resource], resource).unwrap_or(0.0);
    if let Some(pod_value) = resource_quantity(&spec["resources"][field][resource], resource) {
        return Some(pod_value + overhead);
    }
    let mut app_total = 0.0;
    let mut seen = false;
    for container in spec["containers"].as_array().into_iter().flatten() {
        if let Some(value) = read(container) {
            app_total += value;
            seen = true;
        }
    }
    let mut sidecars = 0.0;
    let mut init_peak: f64 = 0.0;
    for container in spec["initContainers"].as_array().into_iter().flatten() {
        let value = read(container);
        seen |= value.is_some();
        let value = value.unwrap_or(0.0);
        if container["restartPolicy"] == "Always" {
            sidecars += value;
            init_peak = init_peak.max(sidecars);
        } else {
            init_peak = init_peak.max(sidecars + value);
        }
    }
    (seen || (field == "requests" && overhead > 0.0))
        .then_some((app_total + sidecars).max(init_peak) + overhead)
}

fn summary_card(ui: &mut egui::Ui, index: usize, totals: Totals) {
    let heading = ["CPU", "MEMORY", "PODS"][index];
    ui.vertical_centered(|ui| {
        ui.strong(heading);
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 104.0), Sense::hover());
    let painter = ui.painter();
    let usage_color = if index == 2 { GREEN } else { PURPLE };
    let denominator = totals
        .capacity
        .or(totals.allocatable)
        .filter(|value| *value > 0.0);
    let tracks = [
        (totals.usage, usage_color),
        (totals.requests, GREEN),
        (totals.limits, BLUE),
    ];
    for (ring, &(value, color)) in tracks
        .iter()
        .take(if index == 2 { 1 } else { 3 })
        .enumerate()
    {
        let radius = 44.0 - ring as f32 * 7.0;
        painter.circle_stroke(rect.center(), radius, Stroke::new(4.0_f32, BORDER));
        if let Some((value, total)) = value.zip(denominator) {
            let fraction = (value / total).clamp(0.0, 1.0) as f32;
            if fraction == 0.0 {
                continue;
            }
            let segments = (fraction * 100.0).ceil() as usize;
            let points = (0..=segments)
                .map(|segment| {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * fraction * segment as f32 / segments as f32;
                    rect.center() + radius * Vec2::angled(angle)
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(4.0_f32, color)));
        }
    }
    let format = |value: Option<f64>| {
        value.map_or_else(
            || "—".to_owned(),
            |value| match index {
                0 => format!("{value:.2} cores"),
                1 => format!("{value:.1} GiB"),
                _ => format!("{value:.0}"),
            },
        )
    };
    let labels = [
        ("Usage", totals.usage, usage_color),
        ("Requests", totals.requests, GREEN),
        ("Limits", totals.limits, BLUE),
        (
            "Allocatable Capacity",
            totals.allocatable,
            Color32::from_rgb(113, 125, 143),
        ),
        ("Capacity", totals.capacity, Color32::from_rgb(82, 92, 104)),
    ];
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        for (label, value, color) in labels {
            if index == 2 && matches!(label, "Requests" | "Limits") {
                continue;
            }
            let line = ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, "■");
                ui.label(RichText::new(format!("{label}: {}", format(value))).size(12.0));
            });
            if label == "Limits" {
                line.response
                    .on_hover_text("Sum of configured Pod limits on these nodes.");
            }
        }
    });
}

struct Warning<'a> {
    resource: &'a Resource,
    object_target: Option<&'a Resource>,
    message: String,
    object: String,
    kind: String,
    stamp: Option<OffsetDateTime>,
}

fn node_warning(condition: &Value) -> bool {
    condition["status"] == "True"
        && !matches!(
            condition["type"].as_str(),
            Some("Ready" | "HostUpgrades" | "SchedulingDisabled")
        )
}

fn pod_warning_is_relevant(pod: &Resource) -> bool {
    pod.raw["spec"]["priority"].as_i64().unwrap_or_default() >= 500_000
        || pod.status != "Running"
        || pod.raw["status"]["conditions"]
            .as_array()
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition["type"] == "Ready" && condition["status"] != "True")
            })
}

fn collect_warnings(snapshot: &Snapshot) -> Vec<Warning<'_>> {
    let mut warnings = Vec::new();
    for node in snapshot.kind("Node") {
        for condition in node.raw["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|condition| node_warning(condition))
        {
            let message = condition["message"]
                .as_str()
                .filter(|msg| !msg.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "{}: {}",
                        text(condition, "/type"),
                        text(condition, "/status")
                    )
                });
            warnings.push(Warning {
                resource: node,
                object_target: Some(node),
                message,
                object: node.name.clone(),
                kind: node.kind.clone(),
                stamp: node.created,
            });
        }
    }

    let by_uid: HashMap<_, _> = snapshot
        .resources
        .iter()
        .filter_map(|item| Some((item.raw["metadata"]["uid"].as_str()?, item)))
        .collect();
    let mut seen = HashSet::new();
    for event in snapshot
        .kind("Event")
        .into_iter()
        .filter(|event| event.status == "Warning")
    {
        let object = event_object(event);
        let uid = object["uid"].as_str().filter(|uid| !uid.is_empty());
        let name = object["name"].as_str().unwrap_or_default();
        let kind = object["kind"].as_str().unwrap_or_default();
        let namespace = object["namespace"].as_str().unwrap_or(&event.namespace);
        let key = match uid {
            Some(uid) => (uid, "", "", ""),
            None => (
                "",
                namespace,
                kind,
                if name.is_empty() { &event.name } else { name },
            ),
        };
        if !seen.insert(key) {
            continue;
        }
        let target = match uid {
            Some(uid) => by_uid.get(uid).copied(),
            None => snapshot
                .resources
                .iter()
                .find(|item| event_for(event, item)),
        };
        if kind == "Pod" && !target.is_some_and(pod_warning_is_relevant) {
            continue;
        }
        warnings.push(Warning {
            resource: event,
            object_target: target,
            message: event_message(event).to_owned(),
            object: name.to_owned(),
            kind: event.kind.clone(),
            stamp: event.created,
        });
    }
    warnings
}

const WARNING_WIDTHS: [f32; 4] = [0.65, 0.17, 0.10, 0.08];

fn warnings<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    sort: &mut Option<(usize, bool)>,
) -> Option<Action<'a>> {
    let mut warnings = collect_warnings(snapshot);
    if let Some((column, descending)) = *sort {
        warnings.sort_by(|left, right| {
            let order = match column {
                0 => left.message.cmp(&right.message),
                1 => left.object.cmp(&right.object),
                2 => left.kind.cmp(&right.kind),
                _ => right.stamp.cmp(&left.stamp),
            };
            if descending {
                order.reverse()
            } else {
                order
            }
        });
    }
    let mut action = None;
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Warnings: {}", warnings.len()))
                    .strong()
                    .size(16.0)
                    .color(if warnings.is_empty() { MUTED } else { ORANGE }),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.link("All events →").clicked() {
                    action = Some(Action::Events);
                }
            });
        });
        let partial_events = snapshot
            .errors
            .iter()
            .any(|error| error.starts_with("Events:"));
        if partial_events {
            ui.colored_label(MUTED, "Some cluster events are unavailable.");
        }
        if warnings.is_empty() {
            if !partial_events {
                ui.colored_label(MUTED, "No warnings reported.");
            }
            return;
        }
        ui.separator();
        let width = (ui.available_width() - 24.0).max(1.0);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.horizontal(|ui| {
                for (index, (label, fraction)) in ["Message", "Object", "Type", "Age"]
                    .into_iter()
                    .zip(WARNING_WIDTHS)
                    .enumerate()
                {
                    let caption = match *sort {
                        Some((column, descending)) if column == index => {
                            format!("{label} {}", if descending { "↓" } else { "↑" })
                        }
                        _ => label.into(),
                    };
                    if warning_cell(
                        ui,
                        width * fraction,
                        index,
                        egui::Button::new(caption).frame(false),
                    )
                    .clicked()
                    {
                        *sort = Some((
                            index,
                            matches!(*sort, Some((column, false)) if column == index),
                        ));
                    }
                }
            });
        });
        ui.separator();
        let height = 26.0;
        egui::ScrollArea::vertical()
            .id_salt("overview-warnings")
            .max_height(350.0)
            .show_rows(ui, height, warnings.len(), |ui, range| {
                for warning in &warnings[range] {
                    let age = relative_age(warning.stamp);
                    if let Some(resource) = warning_row(
                        ui,
                        [&warning.message, &warning.object, &warning.kind, &age],
                        Some(warning.resource),
                        warning.object_target,
                    ) {
                        action = Some(Action::Open(resource));
                    }
                }
            });
    });
    action
}

fn warning_cell(
    ui: &mut egui::Ui,
    width: f32,
    column: usize,
    widget: impl egui::Widget,
) -> egui::Response {
    let size = Vec2::new(width, 26.0);
    if column == 0 {
        let layout = egui::Layout::centered_and_justified(egui::Direction::LeftToRight)
            .with_main_align(Align::Min);
        ui.allocate_ui_with_layout(size, layout, |ui| ui.add(widget))
            .inner
    } else {
        ui.add_sized(size, widget)
    }
}

fn warning_row<'a>(
    ui: &mut egui::Ui,
    cells: [&str; 4],
    message_target: Option<&'a Resource>,
    object_target: Option<&'a Resource>,
) -> Option<&'a Resource> {
    let width = (ui.available_width() - 24.0).max(1.0);
    let mut selected = None;
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
            ui.horizontal(|ui| {
                for (index, (cell, fraction)) in cells.into_iter().zip(WARNING_WIDTHS).enumerate() {
                    let target = match index {
                        0 => message_target,
                        1 => object_target,
                        _ => None,
                    };
                    let is_link = index == 1 && target.is_some();
                    let mut caption = RichText::new(cell);
                    if is_link {
                        caption = caption.color(BLUE).underline();
                    }
                    if message_target.is_none() {
                        caption = caption.strong();
                    }
                    let mut label = egui::Label::new(caption).truncate();
                    if target.is_some() {
                        label = label.sense(Sense::click());
                    }
                    let response = warning_cell(ui, width * fraction, index, label);
                    if is_link {
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Link, true, cell)
                        });
                    }
                    let response = if target.is_some() {
                        response.on_hover_cursor(egui::CursorIcon::PointingHand)
                    } else {
                        response
                    };
                    if response.on_hover_text(cell).clicked() {
                        selected = target;
                    }
                }
            })
            .response
        })
        .inner;
    if message_target.is_some() {
        ui.painter().hline(
            response.rect.x_range(),
            response.rect.bottom(),
            Stroke::new(1.0_f32, SIDE),
        );
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_node_usage_is_not_reported_as_zero_or_a_partial_total() {
        let mut snapshot = super::super::demo();
        let mut overview = Overview::default();
        overview.record(&snapshot);
        assert!(overview.groups[0].totals[0].usage.is_some());
        snapshot
            .resources
            .iter_mut()
            .find(|resource| resource.kind == "Node")
            .unwrap()
            .cpu = None;
        overview.record(&snapshot);
        assert_eq!(overview.groups[0].totals[0].usage, None);
        assert_eq!(overview.groups[0].totals[0].capacity, Some(24.0));
    }
}
