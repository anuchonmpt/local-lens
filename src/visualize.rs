use super::{blue, green, muted, orange, purple, text_color, words, Resource, Target};
use eframe::egui::{
    self,
    epaint::{CubicBezierShape, Mesh},
    text::LayoutJob,
    Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};

use crate::theme::{border, panel, selection, side};

const TILE: Vec2 = Vec2::new(196.0, 54.0);
const WORKLOAD: Color32 = Color32::from_rgb(142, 151, 255);
fn topology_groups() -> [(&'static str, Color32); 5] {
    [
        ("Ingress", orange()),
        ("Service", blue()),
        ("Workload", WORKLOAD),
        ("Pod", purple()),
        ("Node", green()),
    ]
}
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, PartialEq)]
enum Mode {
    #[default]
    Map,
    Topology,
    Traffic,
}

pub struct Visualize {
    mode: Mode,
    camera: Camera,
    traffic: Traffic,
    resource_query: String,
}

impl Default for Visualize {
    fn default() -> Self {
        Self {
            mode: Mode::Map,
            camera: Camera::default(),
            traffic: Traffic::default(),
            resource_query: String::new(),
        }
    }
}

impl Visualize {
    pub fn show_traffic(&mut self) {
        self.mode = Mode::Traffic;
    }

    pub fn reset_scope(&mut self) {
        self.camera = Camera::default();
        self.traffic = Traffic::default();
        self.resource_query.clear();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        resources: &[&Resource],
        all_resources: &[Resource],
        target: Option<&Target>,
        namespace: Option<&str>,
    ) -> Option<Resource> {
        let previous = self.mode;
        egui::Frame::new()
            .fill(panel())
            .stroke(Stroke::new(1.0_f32, border()))
            .corner_radius(10)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Map, "Resource map");
                    ui.selectable_value(&mut self.mode, Mode::Traffic, "Live traffic");
                    ui.selectable_value(&mut self.mode, Mode::Topology, "3D topology");
                    if self.mode == Mode::Map {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.resource_query)
                                .desired_width(170.0)
                                .hint_text("Search resources…"),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Fit view").clicked() {
                            self.camera.pan = Vec2::ZERO;
                            self.camera.zoom = 1.0;
                            if self.mode == Mode::Map {
                                self.resource_query.clear();
                            }
                        }
                        if self.mode == Mode::Map {
                            return;
                        }
                        if ui.button("+").on_hover_text("Zoom in").clicked() {
                            self.camera.zoom_by(1.25);
                        }
                        ui.monospace(format!("{:.0}%", self.camera.zoom * 100.0));
                        if ui.button("−").on_hover_text("Zoom out").clicked() {
                            self.camera.zoom_by(0.8);
                        }
                    });
                });
            });
        ui.add_space(8.0);
        if previous != self.mode {
            self.camera = Camera::default();
            self.resource_query.clear();
        }
        if self.mode == Mode::Map {
            let graph = Graph::topology(resources);
            return Self::resource_map(ui, &graph, namespace, &self.resource_query);
        }
        if self.mode == Mode::Topology {
            let graph = Graph::topology(resources);
            ui.horizontal_wrapped(|ui| {
                for (kind, color) in topology_groups() {
                    let count = graph
                        .nodes
                        .iter()
                        .filter(|item| item.color == color)
                        .count();
                    if count > 0 {
                        ui.colored_label(color, "●");
                        ui.label(RichText::new(format!("{kind}  {count}")).color(text_color()));
                        ui.add_space(8.0);
                    }
                }
            });
            return Self::draw_graph(
                &mut self.camera,
                &mut self.resource_query,
                ui,
                &graph,
                "No topology resources in this namespace.",
            );
        }

        let Some((target, source)) = target.zip(prometheus_service(all_resources)) else {
            self.traffic = Traffic::default();
            let available = ui.available_size();
            egui::Frame::new().fill(side()).stroke(Stroke::new(1.0_f32, border())).corner_radius(6)
                .show(ui, |ui| {
                    ui.set_min_size(available - Vec2::splat(2.0));
                    ui.add_space((available.y * 0.36).max(12.0));
                    ui.horizontal(|ui| {
                        let card_width = 420.0_f32.min(ui.available_width());
                        ui.add_space(((ui.available_width() - card_width) * 0.5).max(0.0));
                        egui::Frame::new().fill(panel()).stroke(Stroke::new(1.0_f32, border()))
                            .corner_radius(7).inner_margin(18).show(ui, |ui| {
                                ui.colored_label(orange(), RichText::new("Traffic source unavailable").size(18.0).strong());
                                ui.label("Live HTTP traffic requires a Prometheus service collecting Istio metrics.");
                            });
                    });
                });
            return None;
        };
        self.traffic.poll(target, source, namespace);
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                muted(),
                format!(
                    "Istio HTTP · {}/{} · Requests/s over 1 minute",
                    source.namespace, source.name
                ),
            );
            if let Some(updated) = self.traffic.updated {
                ui.label(format!("Updated {}s ago", updated.elapsed().as_secs()));
            }
        });
        let (flows, empty) = match &self.traffic.result {
            Some(Ok(flows)) => (
                flows.as_slice(),
                "No active Istio HTTP flows in this namespace.",
            ),
            Some(Err(error)) => {
                ui.collapsing("Could not read traffic metrics", |ui| {
                    ui.colored_label(orange(), error);
                });
                (&[][..], "Traffic query failed")
            }
            None => (&[][..], "Waiting for traffic metrics…"),
        };
        let graph = Graph::traffic(flows, resources);
        Self::draw_graph(
            &mut self.camera,
            &mut self.resource_query,
            ui,
            &graph,
            empty,
        )
    }

    fn resource_map(
        ui: &mut egui::Ui,
        graph: &Graph<'_>,
        namespace: Option<&str>,
        query: &str,
    ) -> Option<Resource> {
        let query = query.trim().to_lowercase();
        let matches = |item: &Item<'_>| {
            [item.name, item.scope]
                .iter()
                .any(|text| text.to_lowercase().contains(&query))
        };
        let edges: Vec<_> = graph
            .links
            .iter()
            .filter(|edge| {
                let from = &graph.nodes[edge.from];
                let to = &graph.nodes[edge.to];
                from.kind == "Service" && to.kind == "Pod" && (matches(from) || matches(to))
            })
            .collect();
        let sources: BTreeSet<_> = edges.iter().map(|edge| edge.from).collect();
        let targets: BTreeSet<_> = edges.iter().map(|edge| edge.to).collect();
        egui::Frame::new()
            .fill(side())
            .stroke(Stroke::new(1.0_f32, border()))
            .corner_radius(6)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(namespace.unwrap_or("All namespaces"));
                    ui.separator();
                    ui.colored_label(muted(), "Selected relationships");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(muted(), "Service selectors");
                    });
                });
                ui.separator();
                if edges.is_empty() {
                    ui.label(if query.is_empty() {
                        "No matching Service selectors in this namespace."
                    } else {
                        "No relationships match your search."
                    });
                    return None;
                }
                let graph_height = (ui.available_height() - 40.0).max(120.0);
                let selection = egui::ScrollArea::vertical()
                    .max_height(graph_height)
                    .id_salt("service-resource-map")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let height =
                            (sources.len().max(targets.len()) as f32 * 48.0 + 58.0).max(280.0);
                        let (area, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), height),
                            Sense::hover(),
                        );
                        let mut positions = HashMap::new();
                        let service_width = (area.width() * 0.21).max(100.0);
                        let pod_width = (area.width() * 0.285).max(110.0);
                        let left = area.left() + area.width() * 0.077;
                        let right = area.left() + area.width() * 0.566;
                        for (column, x, width, tile_height, name) in [
                            (&sources, left, service_width, 55.0, "Services"),
                            (&targets, right, pod_width, 39.0, "Pods"),
                        ] {
                            ui.painter().text(
                                egui::pos2(x, area.top() + 8.0),
                                Align2::LEFT_TOP,
                                name,
                                FontId::proportional(13.0),
                                muted(),
                            );
                            for (position, id) in column.iter().enumerate() {
                                let spacing = (height - 47.0) / column.len().max(1) as f32;
                                let y = area.top() + 37.0 + (position as f32 + 0.5) * spacing;
                                positions.insert(
                                    *id,
                                    Rect::from_min_size(
                                        egui::pos2(x, y - tile_height * 0.5),
                                        Vec2::new(width, tile_height),
                                    ),
                                );
                            }
                        }
                        for edge in edges {
                            let from = positions[&edge.from].right_center();
                            let to = positions[&edge.to].left_center();
                            let middle = egui::lerp(from.x..=to.x, 0.45);
                            ui.painter().add(egui::Shape::line(
                                vec![
                                    from,
                                    egui::pos2(middle, from.y),
                                    egui::pos2(middle, to.y),
                                    to,
                                ],
                                Stroke::new(
                                    1.0_f32,
                                    crate::theme::tone(
                                        Color32::from_rgb(155, 175, 195),
                                        Color32::from_rgb(117, 141, 166),
                                    ),
                                ),
                            ));
                            ui.painter()
                                .circle_filled(egui::pos2(middle, from.y), 2.0, muted());
                        }
                        let mut selected = None;
                        for (id, tile) in positions {
                            let item = &graph.nodes[id];
                            let Some(resource) = item.detail else {
                                continue;
                            };
                            let warning = resource.kind == "Pod"
                                && !matches!(resource.status.as_str(), "Running" | "Succeeded");
                            let hit = ui
                                .interact(tile, ui.id().with(id), Sense::click())
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(format!("{}/{}", item.scope, item.name));
                            hit.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, item.name)
                            });
                            if hit.clicked() {
                                selected = Some(resource.clone());
                            }
                            let fill = if hit.hovered() || hit.has_focus() {
                                selection()
                            } else if warning {
                                crate::theme::warning_fill()
                            } else {
                                panel()
                            };
                            ui.painter().rect(
                                tile,
                                6.0,
                                fill,
                                Stroke::new(1.0_f32, border()),
                                egui::StrokeKind::Inside,
                            );
                            let painter = ui
                                .painter()
                                .with_clip_rect(tile.shrink2(Vec2::new(3.0, 1.0)));
                            let service = item.kind == "Service";
                            crate::nav_icon(
                                &painter,
                                tile.left_center() + Vec2::new(22.0, 0.0),
                                if service { "network" } else { "cluster" },
                                if warning { orange() } else { blue() },
                            );
                            let color = if warning { orange() } else { text_color() };
                            let caption = tile.left_center()
                                + Vec2::new(40.0, if service { -8.0 } else { 0.0 });
                            painter.text(
                                caption,
                                Align2::LEFT_CENTER,
                                item.name,
                                if service {
                                    crate::theme::semibold(13.0)
                                } else {
                                    FontId::proportional(13.0)
                                },
                                color,
                            );
                            if service {
                                let ports = crate::service_ports(&resource.raw)
                                    .map(|(number, port)| {
                                        format!(
                                            "{number}/{}",
                                            port["protocol"].as_str().unwrap_or("TCP")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let summary =
                                    format!("{} · {}", crate::service_type(&resource.raw), ports);
                                painter.text(
                                    caption + Vec2::new(0.0, 18.0),
                                    Align2::LEFT_CENTER,
                                    summary,
                                    FontId::monospace(11.0),
                                    muted(),
                                );
                            } else {
                                painter.circle_filled(
                                    tile.right_center() - Vec2::new(14.0, 0.0),
                                    3.0,
                                    if warning { orange() } else { green() },
                                );
                            }
                        }
                        selected
                    })
                    .inner;
                ui.separator();
                ui.horizontal(|ui| {
                    for (icon, label, color) in [
                        ("network", "Service", blue()),
                        ("cluster", "Pod", blue()),
                        ("warning", "Needs attention", orange()),
                    ] {
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::new(15.0, 19.0), Sense::hover());
                        crate::nav_icon(ui.painter(), rect.center(), icon, color);
                        ui.colored_label(muted(), RichText::new(label).size(12.0));
                    }
                });
                selection
            })
            .inner
    }

    fn draw_graph(
        camera: &mut Camera,
        resource_query: &mut String,
        ui: &mut egui::Ui,
        graph: &Graph<'_>,
        empty: &str,
    ) -> Option<Resource> {
        let mut selected = None;
        if graph.nodes.iter().any(|node| node.detail.is_some()) {
            egui::ComboBox::from_id_salt("graph-resource-finder")
                .selected_text("Find a resource")
                .width(208.0)
                .show_ui(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(resource_query)
                            .hint_text("Filter by name, kind, or namespace"),
                    );
                    let query = resource_query.trim().to_lowercase();
                    let mut matches = 0;
                    for item in &graph.nodes {
                        let Some(resource) = item.detail else {
                            continue;
                        };
                        if ![item.name, item.kind, item.scope]
                            .iter()
                            .any(|text| text.to_lowercase().contains(&query))
                        {
                            continue;
                        }
                        matches += 1;
                        let label = format!("{} · {} · {}", item.kind, item.name, item.scope);
                        if ui.selectable_label(false, label).clicked() {
                            selected = Some(resource.clone());
                            ui.close_menu();
                        }
                    }
                    if matches == 0 {
                        ui.label("No matching resources.");
                    }
                });
            ui.add_space(6.0);
        }
        let clicked = camera.draw(ui, graph, empty);
        selected.or(clicked)
    }
}

struct Item<'a> {
    name: &'a str,
    scope: &'a str,
    kind: &'a str,
    detail: Option<&'a Resource>,
    point: Point,
    color: Color32,
}

#[derive(Clone, Copy)]
struct Point {
    x: f32,
    y: f32,
    z: f32,
}

struct Link {
    from: usize,
    to: usize,
    rate: Option<f64>,
}

#[derive(Default)]
struct Graph<'a> {
    nodes: Vec<Item<'a>>,
    links: Vec<Link>,
}

impl<'a> Graph<'a> {
    fn topology(resources: &[&'a Resource]) -> Self {
        let mut graph = Self::default();
        let by_uid: HashMap<_, _> = resources
            .iter()
            .filter_map(|resource| {
                let uid = resource.raw.pointer("/metadata/uid")?.as_str()?;
                Some((uid, *resource))
            })
            .collect();
        let root_owner = |resource: &'a Resource| {
            let mut current = resource;
            for _ in 0..4 {
                let owner = current
                    .raw
                    .pointer("/metadata/ownerReferences")
                    .and_then(Value::as_array)
                    .and_then(|owners| owners.iter().find(|owner| owner["controller"] == true))
                    .and_then(|owner| owner["uid"].as_str())
                    .and_then(|uid| by_uid.get(uid));
                match owner {
                    Some(owner) if owner.namespace == current.namespace => current = owner,
                    _ => break,
                }
            }
            current
        };
        let used_nodes: BTreeSet<_> = resources
            .iter()
            .filter(|r| r.kind == "Pod")
            .map(|r| r.node.as_str())
            .collect();
        let mut indices = HashMap::new();
        let mut column = 0;
        for (kind, color) in topology_groups() {
            let items: Vec<_> = resources
                .iter()
                .copied()
                .filter(|resource| {
                    if kind == "Workload" {
                        matches!(
                            resource.kind.as_str(),
                            "Deployment"
                                | "ReplicaSet"
                                | "StatefulSet"
                                | "DaemonSet"
                                | "Job"
                                | "CronJob"
                        ) && std::ptr::eq(*resource, root_owner(resource))
                    } else {
                        resource.kind == kind
                            && (kind != "Node" || used_nodes.contains(resource.name.as_str()))
                    }
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            let len = items.len();
            for (row, resource) in items.into_iter().enumerate() {
                let index = graph.nodes.len();
                indices.insert(
                    (
                        resource.kind.as_str(),
                        resource.namespace.as_str(),
                        resource.name.as_str(),
                    ),
                    index,
                );
                graph.nodes.push(Item {
                    name: &resource.name,
                    scope: &resource.namespace,
                    kind: &resource.kind,
                    detail: Some(resource),
                    color,
                    point: point(column, row, len),
                });
            }
            column += graph_columns(len);
        }

        let mut links = BTreeSet::new();
        for (index, item) in graph.nodes.iter().enumerate() {
            let Some(resource) = item.detail else {
                continue;
            };
            match item.kind {
                "Ingress" => {
                    let default = resource.raw.pointer("/spec/defaultBackend");
                    let routes = resource
                        .raw
                        .pointer("/spec/rules")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .flat_map(|rule| {
                            rule.pointer("/http/paths")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                        })
                        .filter_map(|route| route.get("backend"));
                    for backend in default.into_iter().chain(routes) {
                        if let Some(target) = backend
                            .pointer("/service/name")
                            .and_then(Value::as_str)
                            .and_then(|name| indices.get(&("Service", item.scope, name)))
                        {
                            links.insert((index, *target));
                        }
                    }
                }
                "Service" => {
                    let Some(selector) = resource
                        .raw
                        .pointer("/spec/selector")
                        .and_then(Value::as_object)
                        .filter(|selector| !selector.is_empty())
                    else {
                        continue;
                    };
                    for (pod_index, pod) in
                        graph.nodes.iter().enumerate().filter(|(_, item)| {
                            item.kind == "Pod" && item.scope == resource.namespace
                        })
                    {
                        if pod
                            .detail
                            .and_then(|pod| pod.raw.pointer("/metadata/labels"))
                            .is_some_and(|labels| {
                                selector
                                    .iter()
                                    .all(|(key, value)| labels.get(key) == Some(value))
                            })
                        {
                            links.insert((index, pod_index));
                        }
                    }
                }
                "Pod" => {
                    if let Some(node) = indices.get(&("Node", "", resource.node.as_str())) {
                        links.insert((index, *node));
                    }
                    let owner = root_owner(resource);
                    if let Some(parent) =
                        indices.get(&(owner.kind.as_str(), item.scope, owner.name.as_str()))
                    {
                        if *parent != index {
                            links.insert((*parent, index));
                        }
                    }
                }
                _ => {}
            }
        }
        graph.links = links
            .into_iter()
            .map(|(from, to)| Link {
                from,
                to,
                rate: None,
            })
            .collect();
        graph
    }

    fn traffic(flows: &'a [Flow], resources: &[&'a Resource]) -> Self {
        let mut graph = Self::default();
        let mut indices = HashMap::new();
        let mut first_column = 0;
        for (column, kind, color) in [(0, "Workload", purple()), (1, "Service", blue())] {
            let entries: BTreeSet<_> = flows
                .iter()
                .map(|flow| {
                    if column == 0 {
                        (&flow.source_namespace, &flow.source)
                    } else {
                        (&flow.destination_namespace, &flow.destination)
                    }
                })
                .collect();
            let len = entries.len();
            for (row, (namespace, name)) in entries.into_iter().enumerate() {
                indices.insert((column, namespace, name), graph.nodes.len());
                let detail = resources.iter().copied().find(|r| {
                    r.namespace == *namespace
                        && r.name == *name
                        && if column == 0 {
                            matches!(
                                r.kind.as_str(),
                                "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "Pod"
                            )
                        } else {
                            r.kind == "Service"
                        }
                });
                graph.nodes.push(Item {
                    name,
                    scope: namespace,
                    kind,
                    detail,
                    point: point(first_column, row, len),
                    color,
                });
            }
            first_column += graph_columns(len);
        }
        graph.links = flows
            .iter()
            .map(|flow| Link {
                from: indices[&(0, &flow.source_namespace, &flow.source)],
                to: indices[&(1, &flow.destination_namespace, &flow.destination)],
                rate: Some(flow.requests_per_second),
            })
            .collect();
        graph
    }
}

fn graph_columns(count: usize) -> usize {
    count.div_ceil(12).clamp(1, 4)
}

fn point(first_column: usize, index: usize, count: usize) -> Point {
    let rows = count.div_ceil(graph_columns(count));
    let column = index / rows;
    let row_count = (count - column * rows).min(rows);
    let row = index % rows;
    Point {
        x: (first_column + column) as f32 * 242.0,
        y: 26.0 + (first_column % 3) as f32 * 14.0,
        z: (row as f32 - row_count.saturating_sub(1) as f32 / 2.0) * 101.0,
    }
}

struct Camera {
    yaw: f32,
    pitch: f32,
    zoom: f32,
    pan: Vec2,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: -0.10,
            pitch: 0.86,
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }
}

impl Camera {
    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.3, 12.0);
    }

    fn project(&self, p: Point) -> (Vec2, f32) {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let x = p.x * cos_yaw - p.z * sin_yaw;
        let z = p.x * sin_yaw + p.z * cos_yaw;
        (
            Vec2::new(x, z * sin_pitch - p.y * cos_pitch),
            z * cos_pitch + p.y * sin_pitch,
        )
    }

    fn draw(&mut self, ui: &mut egui::Ui, graph: &Graph<'_>, empty: &str) -> Option<Resource> {
        let (rect, drag) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(170.0)),
            Sense::drag(),
        );
        let painter = ui.painter().with_clip_rect(rect);
        let mut backdrop = Mesh::default();
        for (corner, color) in [
            (rect.left_top(), side()),
            (rect.right_top(), panel()),
            (rect.right_bottom(), side()),
            (rect.left_bottom(), panel()),
        ] {
            backdrop.colored_vertex(corner, color);
        }
        backdrop.add_triangle(0, 1, 2);
        backdrop.add_triangle(0, 2, 3);
        painter.add(backdrop);
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0_f32, border()),
            egui::StrokeKind::Inside,
        );
        if graph.nodes.is_empty() {
            let center = rect.center();
            painter.circle_filled(center - Vec2::new(0.0, 24.0), 16.0, border());
            painter.circle_stroke(
                center - Vec2::new(0.0, 24.0),
                5.0,
                Stroke::new(1.5_f32, blue()),
            );
            painter.text(
                center + Vec2::new(0.0, 12.0),
                Align2::CENTER_CENTER,
                empty,
                FontId::proportional(15.0),
                text_color(),
            );
            return None;
        }
        if drag.dragged() {
            let delta = drag.drag_delta();
            if ui.input(|input| input.modifiers.shift) {
                self.pan += delta;
            } else {
                self.yaw -= delta.x * 0.008;
                self.pitch = (self.pitch + delta.y * 0.008).clamp(0.27, 1.43);
            }
        }
        if ui.rect_contains_pointer(rect) {
            let factor = ui.input(|input| {
                let pinch = input.zoom_delta();
                if pinch != 1.0 {
                    pinch
                } else {
                    (input.smooth_scroll_delta.y * 0.005).exp()
                }
            });
            self.zoom_by(factor);
        }
        let projected: Vec<_> = graph
            .nodes
            .iter()
            .map(|node| self.project(node.point))
            .collect();
        let mut bounds = Rect::NOTHING;
        for (point, _) in &projected {
            bounds.extend_with(pos(*point));
        }
        // Fit tiles within the drawable region, excluding the footer and its padding.
        let graph_rect = Rect::from_min_max(rect.min, rect.max - Vec2::new(0.0, 30.0));
        let usable = graph_rect.shrink(12.0).size();
        let fit = |space: f32, span: f32, tile: f32| {
            let proportional = space / (span + tile);
            if span > 0.0 {
                proportional.min((space - tile * 0.25).max(1.0) / span)
            } else {
                proportional
            }
        };
        let scale = fit(usable.x, bounds.width(), TILE.x)
            .min(fit(usable.y, bounds.height(), TILE.y))
            .min(1.15)
            * self.zoom;
        let to_screen = |point: Vec2| {
            graph_rect.center() + self.pan + (point - bounds.center().to_vec2()) * scale
        };
        let floor = |point| to_screen(self.project(point).0);
        let width = graph
            .nodes
            .iter()
            .map(|node| node.point.x)
            .fold(0.0, f32::max);
        let depth = graph
            .nodes
            .iter()
            .map(|node| node.point.z.abs())
            .fold(0.0, f32::max);
        let grid = Stroke::new(1.0_f32, border().gamma_multiply(0.50));
        for x in (0..=(width / 72.0).ceil() as i32 + 2).map(|index| index as f32 * 72.0 - 72.0) {
            painter.line_segment(
                [
                    floor(Point {
                        x,
                        y: 0.0,
                        z: -depth - 54.0,
                    }),
                    floor(Point {
                        x,
                        y: 0.0,
                        z: depth + 54.0,
                    }),
                ],
                grid,
            );
        }
        for index in -(depth / 96.0).ceil() as i32 - 1..=(depth / 96.0).ceil() as i32 + 1 {
            let z = index as f32 * 96.0;
            painter.line_segment(
                [
                    floor(Point {
                        x: -72.0,
                        y: 0.0,
                        z,
                    }),
                    floor(Point {
                        x: width + 72.0,
                        y: 0.0,
                        z,
                    }),
                ],
                grid,
            );
        }
        let centers: Vec<_> = projected
            .iter()
            .map(|(point, _)| to_screen(*point))
            .collect();
        let tile_scale = scale.clamp(0.25, 1.4);
        let tiles: Vec<_> = centers
            .iter()
            .map(|center| Rect::from_center_size(*center, TILE * tile_scale))
            .collect();
        let mut order: Vec<_> = (0..graph.nodes.len()).collect();
        order.sort_by(|a, b| projected[*a].1.total_cmp(&projected[*b].1));
        let hovered = ui
            .input(|input| input.pointer.hover_pos())
            .filter(|_| !drag.dragged() && ui.rect_contains_pointer(graph_rect))
            .and_then(|pointer| {
                order
                    .iter()
                    .rev()
                    .copied()
                    .find(|index| tiles[*index].contains(pointer))
            });
        let mut related = vec![hovered.is_none(); graph.nodes.len()];
        if let Some(index) = hovered {
            related[index] = true;
        }

        let time = ui.input(|input| input.time as f32);
        for link in &graph.links {
            let a = centers[link.from];
            let b = centers[link.to];
            let focused = hovered == Some(link.from) || hovered == Some(link.to);
            if focused {
                related[link.from] = true;
                related[link.to] = true;
            }
            if !Rect::from_two_pos(a, b).expand(18.0).intersects(rect) {
                continue;
            }
            let color = graph.nodes[link.from].color;
            let strength = if focused {
                0.95
            } else if hovered.is_some() {
                0.13
            } else {
                0.44
            };
            let width = link
                .rate
                .map_or(1.4, |rate| (1.6 + rate.ln_1p() as f32).min(5.0));
            let stroke = Stroke::new(
                if focused { width + 0.7 } else { width },
                color.gamma_multiply(strength),
            );
            let curve = CubicBezierShape::from_points_stroke(
                [
                    a,
                    Pos2::new(a.lerp(b, 0.55).x, a.y),
                    Pos2::new(a.lerp(b, 0.45).x, b.y),
                    b,
                ],
                false,
                Color32::TRANSPARENT,
                stroke,
            );
            if a.distance(b) > 55.0 && (focused || hovered.is_none()) {
                let tip = curve.sample(0.64);
                let direction = (tip - curve.sample(0.60)).normalized();
                let normal = Vec2::new(-direction.y, direction.x);
                painter.add(egui::Shape::line(
                    vec![
                        tip - direction * 7.0 + normal * 3.5,
                        tip,
                        tip - direction * 7.0 - normal * 3.5,
                    ],
                    stroke,
                ));
            }
            if let Some(rate) = link.rate {
                let phase = (time * (0.27 + (rate.ln_1p() as f32).min(2.0) * 0.15)).fract();
                painter.circle_filled(curve.sample(phase), 3.0, color.gamma_multiply(strength));
                if focused || (hovered.is_none() && graph.links.len() <= 8 && scale > 0.65) {
                    painter.text(
                        curve.sample(0.5) - Vec2::new(0.0, 8.0),
                        Align2::CENTER_BOTTOM,
                        format!("{rate:.2} req/s"),
                        FontId::monospace(10.0),
                        text_color(),
                    );
                }
                ui.ctx().request_repaint_after(Duration::from_millis(33));
            }
            painter.add(curve);
        }
        let mut selected = None;
        for index in order {
            let item = &graph.nodes[index];
            let center = centers[index];
            let tile = tiles[index];
            let hit = tile.intersect(graph_rect);
            if !hit.is_positive() {
                continue;
            }
            let response = ui.interact(
                hit,
                ui.id().with((item.kind, item.scope, item.name)),
                if item.detail.is_some() {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    item.detail.is_some(),
                    item.name,
                )
            });
            let accent = item
                .color
                .gamma_multiply(if related[index] { 1.0 } else { 0.42 });
            let ground = floor(Point {
                y: 0.0,
                ..item.point
            });
            painter.line_segment(
                [ground, center],
                Stroke::new(1.0_f32, accent.gamma_multiply(0.45)),
            );
            painter.circle_filled(ground, 2.0, accent.gamma_multiply(0.60));
            painter.rect_filled(
                tile.translate(Vec2::new(0.0, 3.0)),
                9.0,
                Color32::from_black_alpha(84),
            );
            painter.rect_filled(
                tile,
                9.0,
                if hovered == Some(index) {
                    selection()
                } else {
                    side()
                },
            );
            painter.rect_stroke(
                tile,
                9.0,
                Stroke::new(1.0_f32, accent.gamma_multiply(0.68)),
                egui::StrokeKind::Inside,
            );

            if tile_scale < 0.72 {
                let mut title = LayoutJob::simple_singleline(
                    item.name.into(),
                    FontId::proportional(10.0),
                    text_color(),
                );
                title.wrap.max_width = (tile.width() - 8.0).max(1.0);
                title.wrap.max_rows = 1;
                title.wrap.break_anywhere = true;
                let label = painter.with_clip_rect(hit);
                let galley = label.layout_job(title);
                label.galley(center - galley.size() * 0.5, galley, text_color());
            } else {
                let icon = Pos2::new(tile.left() + 23.0 * tile_scale, center.y);
                painter.circle_filled(icon, 12.0 * tile_scale, accent.gamma_multiply(0.18));
                painter.text(
                    icon,
                    Align2::CENTER_CENTER,
                    item.kind.chars().next().unwrap_or('?'),
                    FontId::monospace(11.0 * tile_scale),
                    accent,
                );
                let x = tile.left() + 43.0 * tile_scale;
                let label = painter.with_clip_rect(hit);
                label.text(
                    Pos2::new(x, center.y - 9.0 * tile_scale),
                    Align2::LEFT_CENTER,
                    item.kind,
                    FontId::proportional(11.0 * tile_scale),
                    accent,
                );
                let mut title = LayoutJob::simple_singleline(
                    item.name.into(),
                    FontId::proportional(13.5 * tile_scale),
                    text_color().gamma_multiply(if related[index] { 1.0 } else { 0.58 }),
                );
                title.wrap.max_width = (tile.right() - x - 10.0 * tile_scale).max(1.0);
                title.wrap.max_rows = 1;
                title.wrap.break_anywhere = true;
                let galley = label.layout_job(title);
                let y = center.y + 8.0 * tile_scale - galley.size().y * 0.5;
                label.galley(Pos2::new(x, y), galley, text_color());
            }
            if let Some(resource) = item.detail {
                let status = super::status_color(&resource.status);
                painter.circle_filled(
                    tile.right_top() + Vec2::new(-10.0, 10.0) * tile_scale,
                    2.5 * tile_scale,
                    status,
                );
                if response.clicked() {
                    selected = Some(resource.clone());
                }
            }
            response.on_hover_text(format!("{} · {}\n{}", item.kind, item.name, item.scope));
        }
        let footer = Rect::from_min_max(
            Pos2::new(rect.left(), rect.bottom() - 30.0),
            rect.right_bottom(),
        );
        painter.rect_filled(footer, 0.0, panel());
        painter.line_segment(
            [footer.left_top(), footer.right_top()],
            Stroke::new(1.0_f32, border()),
        );
        painter.text(
            footer.left_center() + Vec2::new(12.0, 0.0),
            Align2::LEFT_CENTER,
            format!(
                "{} resources  ·  {} connections",
                graph.nodes.len(),
                graph.links.len()
            ),
            FontId::proportional(11.0),
            muted(),
        );
        painter.text(
            footer.right_center() - Vec2::new(12.0, 0.0),
            Align2::RIGHT_CENTER,
            "Drag to rotate  ·  Shift-drag to pan  ·  Scroll to zoom",
            FontId::proportional(11.0),
            muted(),
        );
        selected
    }
}

fn pos(vector: Vec2) -> Pos2 {
    Pos2::new(vector.x, vector.y)
}

#[derive(Debug)]
struct Flow {
    source: String,
    source_namespace: String,
    destination: String,
    destination_namespace: String,
    requests_per_second: f64,
}

#[derive(Default)]
struct Traffic {
    endpoint: String,
    pending: Option<Receiver<Result<Vec<Flow>, String>>>,
    result: Option<Result<Vec<Flow>, String>>,
    last_poll: Option<Instant>,
    updated: Option<Instant>,
}

impl Traffic {
    fn poll(&mut self, target: &Target, service: &Resource, namespace: Option<&str>) {
        let port = prometheus_port(service).unwrap_or_default();
        let endpoint = format!(
            "/api/v1/namespaces/{}/services/http:{}:{port}/proxy/api/v1/query",
            service.namespace, service.name
        );
        if self.endpoint != endpoint {
            *self = Self {
                endpoint: endpoint.clone(),
                ..Default::default()
            };
        }
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(result) => {
                    self.updated = result.is_ok().then(Instant::now);
                    self.result = Some(result);
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.result = Some(Err("Traffic query worker stopped".into()));
                }
            }
        }
        if self
            .last_poll
            .is_some_and(|last| last.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.last_poll = Some(Instant::now());
        let target = target.clone();
        let namespace = namespace.map(str::to_owned);
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let query = prometheus_query(namespace.as_deref());
            let url = format!("{endpoint}?query={}&timeout=8s", encode_query(&query));
            let result = target
                .run(
                    &words(&["get", "--raw", &url, "--request-timeout=12s"]),
                    None,
                )
                .and_then(|output| parse_flows(&output, namespace.as_deref()));
            let _ = tx.send(result);
        });
    }
}

fn prometheus_service(resources: &[Resource]) -> Option<&Resource> {
    resources
        .iter()
        .filter(|r| {
            r.kind == "Service"
                && r.raw.pointer("/spec/type").and_then(Value::as_str) != Some("ExternalName")
                && (r.name == "prometheus"
                    || r.raw
                        .pointer("/metadata/labels/app.kubernetes.io~1name")
                        .and_then(Value::as_str)
                        == Some("prometheus"))
                && prometheus_port(r).is_some()
        })
        .min_by_key(|r| (r.namespace != "istio-system", &r.namespace, &r.name))
}

pub(super) fn prometheus_port(service: &Resource) -> Option<String> {
    let ports = service.raw.pointer("/spec/ports")?.as_array()?;
    let port = ports.iter().find(|p| p["port"] == 9090).or_else(|| {
        ports
            .iter()
            .find(|p| matches!(p["name"].as_str(), Some("http" | "http-web" | "web")))
    })?;
    port["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| port["port"].as_u64().map(|number| number.to_string()))
}

fn prometheus_query(namespace: Option<&str>) -> String {
    let mut filters =
        r#"reporter="source",source_workload!="unknown",destination_service_name!="unknown""#
            .to_owned();
    if let Some(namespace) = namespace {
        let quoted = serde_json::to_string(namespace).expect("string");
        filters.push_str(&format!(
            ",source_workload_namespace={quoted},destination_service_namespace={quoted}"
        ));
    }
    format!("sum by (source_workload,source_workload_namespace,destination_service_name,destination_service_namespace) (rate(istio_requests_total{{{filters}}}[1m]))")
}

pub(super) fn encode_query(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("string");
        }
    }
    encoded
}

fn parse_flows(output: &str, namespace: Option<&str>) -> Result<Vec<Flow>, String> {
    let value: Value = serde_json::from_str(output).map_err(|error| error.to_string())?;
    if value["status"] != "success" || value["data"]["resultType"] != "vector" {
        return Err(value["error"]
            .as_str()
            .unwrap_or("Expected a Prometheus instant vector")
            .into());
    }
    let samples = value["data"]["result"]
        .as_array()
        .ok_or("Missing Prometheus results")?;
    let flows = samples
        .iter()
        .filter_map(|sample| {
            let labels = &sample["metric"];
            let source = labels["source_workload"].as_str()?;
            let source_namespace = labels["source_workload_namespace"].as_str()?;
            let destination = labels["destination_service_name"].as_str()?;
            let destination_namespace = labels["destination_service_namespace"].as_str()?;
            let rate: f64 = sample["value"][1].as_str()?.parse().ok()?;
            if !rate.is_finite()
                || rate <= 0.0
                || [source, source_namespace, destination, destination_namespace]
                    .into_iter()
                    .any(|value| value.is_empty() || value == "unknown")
                || namespace.is_some_and(|ns| ns != source_namespace || ns != destination_namespace)
            {
                return None;
            }
            Some(Flow {
                source: source.into(),
                source_namespace: source_namespace.into(),
                destination: destination.into(),
                destination_namespace: destination_namespace.into(),
                requests_per_second: rate,
            })
        })
        .collect();
    Ok(flows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn topology_uses_selectors_and_owner_uids_in_the_same_namespace() {
        let mut resources = vec![
            Resource::parse(
                json!({"kind":"Service", "metadata":{"name":"api","namespace":"a"}, "spec":{"selector":{"app":"api"}}}),
            ),
            Resource::parse(
                json!({"kind":"Deployment", "metadata":{"name":"api","namespace":"a","uid":"workload-1"}}),
            ),
            Resource::parse(
                json!({"kind":"ReplicaSet", "metadata":{"name":"api-1","namespace":"a","uid":"replica-1",
                "ownerReferences":[{"uid":"workload-1","controller":true}]}}),
            ),
            Resource::parse(
                json!({"kind":"Pod", "metadata":{"name":"a-pod","namespace":"a","labels":{"app":"api"},
                "ownerReferences":[{"uid":"replica-1","controller":true}]}}),
            ),
            Resource::parse(
                json!({"kind":"Pod", "metadata":{"name":"b-pod","namespace":"b","labels":{"app":"api"}}}),
            ),
            Resource::parse(
                json!({"kind":"Ingress", "metadata":{"name":"ingress","namespace":"a"},
                "spec":{"defaultBackend":{"service":{"name":"api"}}}}),
            ),
        ];
        resources[3].node = "worker".into();
        resources.push(Resource::parse(
            json!({"kind":"Node","metadata":{"name":"worker"}}),
        ));
        let graph = Graph::topology(&resources.iter().collect::<Vec<_>>());
        let links: BTreeSet<_> = graph
            .links
            .iter()
            .map(|link| {
                let a = &graph.nodes[link.from];
                let b = &graph.nodes[link.to];
                ((a.kind, a.name), (b.kind, b.name))
            })
            .collect();
        assert_eq!(
            links,
            BTreeSet::from([
                (("Ingress", "ingress"), ("Service", "api")),
                (("Service", "api"), ("Pod", "a-pod")),
                (("Deployment", "api"), ("Pod", "a-pod")),
                (("Pod", "a-pod"), ("Node", "worker")),
            ])
        );
    }

    #[test]
    fn traffic_displays_only_measured_positive_rates_in_scope() {
        let sample = |destination_namespace: &str, rate: &str| {
            json!({
                "metric":{"source_workload":"api","source_workload_namespace":"a",
                    "destination_service_name":"db","destination_service_namespace":destination_namespace},
                "value":[42, rate],
            })
        };
        let response = json!({"status":"success","data":{"resultType":"vector",
            "result":[sample("a", "2.5"), sample("a", "0"), sample("a", "NaN"), sample("b", "3")]}});
        let scoped = parse_flows(&response.to_string(), Some("a")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].requests_per_second, 2.5);
        assert_eq!(parse_flows(&response.to_string(), None).unwrap().len(), 2);
        assert!(parse_flows(r#"{"status":"error","error":"forbidden"}"#, None).is_err());
    }
}
