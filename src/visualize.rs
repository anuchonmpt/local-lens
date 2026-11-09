use super::{words, Resource, Target, BLUE, GREEN, MUTED, ORANGE, PURPLE, TEXT};
use eframe::egui::{
    self,
    epaint::{CubicBezierShape, Mesh},
    text::LayoutJob,
    Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};

const CANVAS: Color32 = Color32::from_rgb(14, 20, 31);
const BORDER: Color32 = Color32::from_rgb(46, 60, 79);
const TILE: Vec2 = Vec2::new(196.0, 54.0);
const WORKLOAD: Color32 = Color32::from_rgb(142, 151, 255);
const TOPOLOGY_GROUPS: [(&str, Color32); 5] = [
    ("Ingress", ORANGE),
    ("Service", BLUE),
    ("Workload", WORKLOAD),
    ("Pod", PURPLE),
    ("Node", GREEN),
];
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

#[derive(Default, PartialEq)]
enum Mode {
    #[default]
    Topology,
    Traffic,
}

pub struct Visualize {
    mode: Mode,
    camera: Camera,
    traffic: Traffic,
}

impl Default for Visualize {
    fn default() -> Self {
        Self {
            mode: Mode::Topology,
            camera: Camera::default(),
            traffic: Traffic::default(),
        }
    }
}

impl Visualize {
    pub fn reset_scope(&mut self) {
        self.camera = Camera::default();
        self.traffic = Traffic::default();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        resources: &[&Resource],
        all_resources: &[Resource],
        target: Option<&Target>,
        namespace: Option<&str>,
    ) -> Option<Resource> {
        let previous = self.mode == Mode::Traffic;
        egui::Frame::new()
            .fill(CANVAS)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(10)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Topology, "3D Topology");
                    ui.selectable_value(&mut self.mode, Mode::Traffic, "Live Traffic Mesh");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Fit view").clicked() {
                            self.camera = Camera::default();
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
        if previous != (self.mode == Mode::Traffic) {
            self.camera = Camera::default();
        }
        if self.mode == Mode::Topology {
            let graph = Graph::topology(resources);
            ui.horizontal_wrapped(|ui| {
                for (kind, color) in TOPOLOGY_GROUPS {
                    let count = graph
                        .nodes
                        .iter()
                        .filter(|item| item.color == color)
                        .count();
                    if count > 0 {
                        ui.colored_label(color, "●");
                        ui.label(RichText::new(format!("{kind}  {count}")).color(TEXT));
                        ui.add_space(8.0);
                    }
                }
            });
            return self
                .camera
                .draw(ui, &graph, "No topology resources in this namespace.");
        }

        let Some((target, source)) = target.zip(prometheus_service(all_resources)) else {
            self.traffic = Traffic::default();
            ui.colored_label(ORANGE, "Traffic source unavailable");
            ui.label("Live HTTP traffic requires a Prometheus service collecting Istio metrics.");
            return self.camera.draw(ui, &Graph::default(), "No traffic source");
        };
        self.traffic.poll(target, source, namespace);
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                MUTED,
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
                    ui.colored_label(ORANGE, error);
                });
                (&[][..], "Traffic query failed")
            }
            None => (&[][..], "Waiting for traffic metrics…"),
        };
        let graph = Graph::traffic(flows, resources);
        self.camera.draw(ui, &graph, empty)
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
        for (kind, color) in TOPOLOGY_GROUPS {
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
        for (column, kind, color) in [(0, "Workload", PURPLE), (1, "Service", BLUE)] {
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
            (rect.left_top(), Color32::from_rgb(24, 36, 54)),
            (rect.right_top(), CANVAS),
            (rect.right_bottom(), Color32::from_rgb(19, 27, 43)),
            (rect.left_bottom(), CANVAS),
        ] {
            backdrop.colored_vertex(corner, color);
        }
        backdrop.add_triangle(0, 1, 2);
        backdrop.add_triangle(0, 2, 3);
        painter.add(backdrop);
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0_f32, BORDER),
            egui::StrokeKind::Inside,
        );
        if graph.nodes.is_empty() {
            let center = rect.center();
            painter.circle_filled(center - Vec2::new(0.0, 24.0), 16.0, BORDER);
            painter.circle_stroke(
                center - Vec2::new(0.0, 24.0),
                5.0,
                Stroke::new(1.5_f32, BLUE),
            );
            painter.text(
                center + Vec2::new(0.0, 12.0),
                Align2::CENTER_CENTER,
                empty,
                FontId::proportional(15.0),
                TEXT,
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
        let padded = bounds.expand2(Vec2::new(TILE.x / 2.0 + 20.0, TILE.y / 2.0 + 42.0));
        let scale = (rect.width() / padded.width())
            .min(rect.height() / padded.height())
            .min(1.15)
            * self.zoom;
        let to_screen =
            |point: Vec2| rect.center() + self.pan + (point - bounds.center().to_vec2()) * scale;
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
        let grid = Stroke::new(1.0_f32, BORDER.gamma_multiply(0.50));
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
        let graph_rect = Rect::from_min_max(rect.min, rect.max - Vec2::new(0.0, 30.0));
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
                        TEXT,
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
                    Color32::from_rgb(34, 48, 69)
                } else {
                    Color32::from_rgb(21, 31, 47)
                },
            );
            painter.rect_stroke(
                tile,
                9.0,
                Stroke::new(1.0_f32, accent.gamma_multiply(0.68)),
                egui::StrokeKind::Inside,
            );

            let icon = Pos2::new(tile.left() + 23.0 * tile_scale, center.y);
            painter.circle_filled(icon, 12.0 * tile_scale, accent.gamma_multiply(0.18));
            if scale >= 0.38 {
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
                    TEXT.gamma_multiply(if related[index] { 1.0 } else { 0.58 }),
                );
                title.wrap.max_width = (tile.right() - x - 10.0 * tile_scale).max(1.0);
                title.wrap.max_rows = 1;
                title.wrap.break_anywhere = true;
                let galley = label.layout_job(title);
                let y = center.y + 8.0 * tile_scale - galley.size().y * 0.5;
                label.galley(Pos2::new(x, y), galley, TEXT);
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
        painter.rect_filled(footer, 0.0, CANVAS);
        painter.line_segment(
            [footer.left_top(), footer.right_top()],
            Stroke::new(1.0_f32, BORDER),
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
            MUTED,
        );
        painter.text(
            footer.right_center() - Vec2::new(12.0, 0.0),
            Align2::RIGHT_CENTER,
            "Drag to rotate  ·  Shift-drag to pan  ·  Scroll to zoom",
            FontId::proportional(11.0),
            MUTED,
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
    port["name"].as_str().filter(|name| !name.is_empty()).map(str::to_owned)
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
