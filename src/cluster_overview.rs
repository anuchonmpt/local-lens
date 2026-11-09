use super::{
    blue, border, card, detail_metrics, egui, event_for, event_message, event_object, green,
    is_master, muted, now, orange, purple, relative_age, resource_quantity, side, text, Align,
    Color32, Resource, RichText, Sample, Sense, Snapshot, Stroke, Vec2, REFRESH,
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
    Pods,
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
        let warnings = collect_warnings(snapshot);
        let mut action = None;
        let mut jump = false;
        let pods = snapshot.kind("Pod");
        let nodes = snapshot.kind("Node");
        if let Some(first) = warnings.first() {
            let target = first.object_target.unwrap_or(first.resource);
            let pod_warnings = warnings
                .iter()
                .filter(|warning| {
                    warning
                        .object_target
                        .is_some_and(|object| object.kind == "Pod")
                })
                .count();
            let heading = match (pod_warnings, warnings.len()) {
                (1, 1) => "1 pod needs attention".into(),
                (pods, count) if pods == count => format!("{pods} pods need attention"),
                (_, count) => format!("{count} cluster warnings"),
            };
            egui::Frame::new()
                .fill(crate::theme::warning_fill())
                .corner_radius(7)
                .inner_margin(egui::Margin::symmetric(20, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (icon, _) = ui.allocate_exact_size(Vec2::new(22.0, 46.0), Sense::hover());
                        crate::nav_icon(ui.painter(), icon.center(), "warning", orange());
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.colored_label(orange(), RichText::new(heading).font(crate::theme::semibold(17.0)));
                            let summary = if demo && target.kind == "Pod" { "payments in production is restarting." } else { &first.message };
                            ui.colored_label(orange(), RichText::new(summary).size(13.0));
                        });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let label = if target.kind == "Pod" {
                                "Inspect affected pod"
                            } else {
                                "Inspect warning"
                            };
                            if ui.link(RichText::new(label).color(orange())).clicked() {
                                action = Some(Action::Open(target));
                            }
                            if warnings.len() > 1
                                && ui
                                    .link(RichText::new("View warnings").color(orange()))
                                    .clicked()
                            {
                                jump = true;
                            }
                        });
                    });
                });
            ui.add_space(11.0);
        }
        let group = &self.groups[0];
        let count = |kind: &str, value: usize, total: usize| {
            if snapshot.errors.iter().any(|error| error.starts_with(kind)) {
                "—".into()
            } else {
                format!("{value} / {total}")
            }
        };
        let ready = nodes.iter().filter(|node| node.status == "Ready").count();
        let running = pods
            .iter()
            .filter(|pod| pod.raw["status"]["phase"] == "Running")
            .count();
        let percent = |index: usize| {
            ratio(group.totals[index].usage, group.totals[index].allocatable)
                .map_or_else(|| "—".into(), |value| format!("{:.1}%", value * 100.0))
        };
        let capacity = |index: usize, unit: &str| {
            group.totals[index].allocatable.map_or_else(
                || "Allocatable capacity unavailable".into(),
                |value| format!("Of {value:.0} allocatable {unit}"),
            )
        };
        card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.columns(4, |columns| {
                for (column, (label, value, caption, color)) in columns.iter_mut().zip([
                    (
                        "Nodes ready",
                        count("Nodes:", ready, nodes.len()),
                        "Cluster-wide".into(),
                        crate::text_color(),
                    ),
                    (
                        "Pods running",
                        count("Pods:", running, pods.len()),
                        "Across all namespaces".into(),
                        crate::text_color(),
                    ),
                    ("CPU utilization", percent(0), capacity(0, "cores"), blue()),
                    (
                        "Memory utilization",
                        percent(1),
                        capacity(1, "GiB"),
                        purple(),
                    ),
                ]) {
                    column.colored_label(muted(), RichText::new(label).size(13.0));
                    column.colored_label(color, RichText::new(value).font(crate::theme::semibold(24.0)));
                    column.colored_label(muted(), RichText::new(caption).size(12.0));
                }
            });
        });
        ui.add_space(11.0);
        let end = now();
        let start = end.saturating_sub(self.range);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Resource trends").font(crate::theme::semibold(18.0)));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let range = RANGES
                        .iter()
                        .find(|(_, value)| *value == self.range)
                        .map_or("1h", |(label, _)| *label);
                    egui::ComboBox::from_id_salt("overview-range")
                        .selected_text(range)
                        .show_ui(ui, |ui| {
                            for (label, range) in RANGES {
                                ui.selectable_value(&mut self.range, range, label);
                            }
                        });
                    ui.colored_label(purple(), "Memory");
                    ui.colored_label(blue(), "CPU");
                });
            });
            ui.colored_label(
                muted(),
                if demo {
                    "Illustrative history"
                } else {
                    "Usage as a share of allocatable capacity"
                },
            );
            trend_plot(ui, group, start, end, demo);
        });
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            card(&mut columns[0], |ui| {
                ui.set_min_height(135.0);
                ui.label(RichText::new("Node utilization").font(crate::theme::semibold(17.0)));
                for node in &nodes {
                    let usage = ratio(
                        node.cpu,
                        resource_quantity(&node.raw["status"]["allocatable"]["cpu"], "cpu"),
                    );
                    if usage_row(
                        ui,
                        &node.name,
                        usage,
                        usage.map_or_else(|| "—".into(), |value| format!("{:.0}%", value * 100.0)),
                    )
                    .clicked()
                    {
                        action = Some(Action::Open(node));
                    }
                }
                if nodes.is_empty() {
                    ui.colored_label(muted(), "No nodes available.");
                }
            });
            card(&mut columns[1], |ui| {
                ui.set_min_height(135.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pods by namespace").font(crate::theme::semibold(17.0)));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.link(RichText::new("View pods").color(blue())).clicked() {
                            action = Some(Action::Pods);
                        }
                    });
                });
                let mut by_namespace = std::collections::BTreeMap::new();
                for pod in &pods {
                    *by_namespace.entry(pod.namespace.as_str()).or_insert(0) += 1;
                }
                let mut by_namespace: Vec<_> = by_namespace.into_iter().collect();
                by_namespace.sort_by_key(|(namespace, count)| (std::cmp::Reverse(*count), *namespace));
                for (namespace, count) in by_namespace {
                    usage_row(
                        ui,
                        namespace,
                        Some(count as f64 / pods.len() as f64),
                        format!("{count} pods"),
                    );
                }
                if pods.is_empty() {
                    ui.colored_label(muted(), "No pods available.");
                }
            });
        });
        egui::CollapsingHeader::new("Capacity details").show(ui, |ui| self.capacity_details(ui));
        let warning_action = show_warnings(ui, snapshot, warnings, &mut self.warning_sort, jump);
        action.or(warning_action)
    }

    fn capacity_details(&mut self, ui: &mut egui::Ui) {
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
            ui.selectable_value(&mut self.memory, false, "CPU");
            ui.selectable_value(&mut self.memory, true, "Memory");
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
        let coverage = match (points.first(), points.last()) {
            (Some(first), Some(last)) if points.len() > 1 => {
                let seconds = last.0.saturating_sub(first.0);
                let span = if seconds < 60 {
                    format!("{seconds}s")
                } else {
                    format!("{}m", seconds / 60)
                };
                format!("{span} of history · {} samples", points.len())
            }
            (Some(_), _) => "1 sample · collecting history".into(),
            _ => "No samples in this range".into(),
        };
        ui.colored_label(muted(), coverage);
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
    }
}

fn ratio(usage: Option<f64>, capacity: Option<f64>) -> Option<f64> {
    let capacity = capacity.filter(|value| *value > 0.0)?;
    Some(usage? / capacity)
}

fn trend_plot(ui: &mut egui::Ui, group: &Group, start: u64, end: u64, demo: bool) {
    let series = [false, true].map(|memory| {
        let total = group.totals[usize::from(memory)];
        if demo {
            const CPU: [f64; 18] = [
                0.90, 1.02, 0.97, 1.19, 1.06, 0.78, 0.73, 0.81, 0.76, 1.07, 1.10, 0.96, 0.80, 0.69,
                0.79, 1.02, 0.86, 1.00,
            ];
            const MEMORY: [f64; 18] = [
                0.94, 0.97, 0.99, 1.04, 1.07, 1.04, 1.01, 0.97, 0.99, 0.96, 0.96, 0.99, 1.03, 1.05,
                1.08, 1.04, 1.01, 1.00,
            ];
            let samples = if memory { MEMORY } else { CPU };
            if let Some(value) = ratio(total.usage, total.allocatable) {
                return samples
                    .iter()
                    .enumerate()
                    .map(|(i, multiplier)| (i as f64 / 17.0, value * multiplier))
                    .collect::<Vec<_>>();
            }
        }
        let span = end.saturating_sub(start).max(1) as f64;
        group
            .history
            .iter()
            .filter(|sample| (start..=end).contains(&sample.stamp))
            .map(|sample| {
                (
                    (sample.stamp - start) as f64 / span,
                    ratio(
                        if memory { sample.memory } else { sample.cpu },
                        total.allocatable,
                    )
                    .unwrap_or(f64::NAN),
                )
            })
            .collect::<Vec<_>>()
    });
    let max = series
        .iter()
        .flatten()
        .map(|(_, value)| *value)
        .fold(1.0, f64::max);
    let max = (max * 4.0).ceil() / 4.0;
    let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 184.0), Sense::hover());
    let plot = egui::Rect::from_min_max(
        area.min + Vec2::new(48.0, 9.0),
        area.max - Vec2::new(18.0, 24.0),
    );
    let painter = ui.painter();
    for mark in 0..=4 {
        let y = egui::lerp(plot.y_range(), 1.0 - mark as f32 / 4.0);
        painter.hline(plot.x_range(), y, Stroke::new(1.0_f32, border()));
        painter.text(
            egui::pos2(plot.left() - 9.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}%", max * mark as f64 * 25.0),
            egui::FontId::proportional(11.0),
            muted(),
        );
        let remaining = end.saturating_sub(start) / 60 * (4 - mark) / 4;
        let tick = if remaining == 0 {
            "Now".into()
        } else if remaining >= 1440 {
            format!("−{}d", remaining / 1440)
        } else if remaining >= 60 && remaining % 60 == 0 {
            format!("−{}h", remaining / 60)
        } else {
            format!("−{remaining}m")
        };
        painter.text(
            egui::pos2(
                egui::lerp(plot.x_range(), mark as f32 / 4.0),
                plot.bottom() + 10.0,
            ),
            egui::Align2::CENTER_TOP,
            tick,
            egui::FontId::proportional(11.0),
            muted(),
        );
    }
    if !series.iter().flatten().any(|(_, value)| value.is_finite()) {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "No utilization samples in this range",
            egui::FontId::proportional(13.0),
            muted(),
        );
    }
    for (points, color) in series.into_iter().zip([blue(), purple()]) {
        for segment in points
            .split(|(_, value)| !value.is_finite())
            .filter(|segment| !segment.is_empty())
        {
            let points: Vec<_> = segment
                .iter()
                .map(|(x, y)| {
                    egui::pos2(
                        egui::lerp(plot.x_range(), *x as f32),
                        egui::lerp(plot.y_range(), 1.0 - (*y / max) as f32),
                    )
                })
                .collect();
            if points.len() == 1 {
                painter.circle_filled(points[0], 2.5, color);
            } else {
                painter.add(egui::Shape::line(points, Stroke::new(1.6_f32, color)));
            }
        }
    }
}

fn usage_row(
    ui: &mut egui::Ui,
    label: &str,
    fraction: Option<f64>,
    value: String,
) -> egui::Response {
    ui.horizontal(|ui| {
        let width = ui.available_width();
        let response = ui
            .add_sized(
                [width * 0.25, 22.0],
                egui::Label::new(label).truncate().sense(Sense::click()),
            )
            .on_hover_text(label);
        let (track, _) =
            ui.allocate_exact_size(Vec2::new((width * 0.53).max(1.0), 7.0), Sense::hover());
        ui.painter()
            .rect_filled(track, 3.5, crate::theme::selection());
        if let Some(value) = fraction {
            let fill = egui::Rect::from_min_size(
                track.min,
                Vec2::new(track.width() * value.clamp(0.0, 1.0) as f32, track.height()),
            );
            ui.painter().rect_filled(fill, 3.5, blue());
        }
        ui.colored_label(muted(), RichText::new(value).size(12.0));
        response
    })
    .inner
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
    let usage_color = if index == 2 { green() } else { purple() };
    let denominator = totals
        .capacity
        .or(totals.allocatable)
        .filter(|value| *value > 0.0);
    let tracks = [
        (totals.usage, usage_color),
        (totals.requests, green()),
        (totals.limits, blue()),
    ];
    for (ring, &(value, color)) in tracks
        .iter()
        .take(if index == 2 { 1 } else { 3 })
        .enumerate()
    {
        let radius = 44.0 - ring as f32 * 7.0;
        painter.circle_stroke(rect.center(), radius, Stroke::new(4.0_f32, border()));
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
        ("Requests", totals.requests, green()),
        ("Limits", totals.limits, blue()),
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

fn show_warnings<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    mut warnings: Vec<Warning<'a>>,
    sort: &mut Option<(usize, bool)>,
    jump: bool,
) -> Option<Action<'a>> {
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
            let heading = ui.label(
                RichText::new(format!("Warnings: {}", warnings.len()))
                    .strong()
                    .size(16.0)
                    .color(if warnings.is_empty() {
                        muted()
                    } else {
                        orange()
                    }),
            );
            if jump {
                heading.scroll_to_me(Some(Align::Min));
            }
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
            ui.colored_label(muted(), "Some cluster events are unavailable.");
        }
        if warnings.is_empty() {
            if !partial_events {
                ui.colored_label(muted(), "No warnings reported.");
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
                        caption = caption.color(blue()).underline();
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
            Stroke::new(1.0_f32, side()),
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
