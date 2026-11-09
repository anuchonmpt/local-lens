use super::{
    blue, border, egui, muted, now, purple, resource_details, side, visualize, words, Align2,
    Color32, FontId, Pos2, Rect, Resource, RichText, Sample, Sense, Snapshot, Stroke, Target, Vec2,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::mpsc,
    time::{Duration, Instant},
};

const RANGES: [(&str, u64); 4] = [("15m", 900), ("1h", 3600), ("6h", 21600), ("24h", 86400)];
const RETENTION: u64 = 86400;

#[derive(Default)]
pub(super) struct Metrics {
    history: HashMap<String, VecDeque<Sample>>,
    range_index: Option<usize>,
    memory: bool,
    remote: Remote,
}

#[derive(Default)]
struct Remote {
    key: Option<(String, String, String, u64)>,
    pending: Option<mpsc::Receiver<Result<Vec<Sample>, String>>>,
    result: Option<Result<Vec<Sample>, String>>,
    requested: Option<Instant>,
}

#[derive(Default)]
struct WorkloadUsage {
    cpu: Option<f64>,
    memory: Option<f64>,
    count: usize,
}

impl WorkloadUsage {
    fn add(&mut self, pod: &Resource) {
        if self.count == 0 {
            self.cpu = pod.cpu;
            self.memory = pod.memory;
        } else {
            self.cpu = self.cpu.zip(pod.cpu).map(|(total, value)| total + value);
            self.memory = self
                .memory
                .zip(pod.memory)
                .map(|(total, value)| total + value);
        }
        self.count += 1;
    }
}

fn uid(resource: &Resource) -> Option<&str> {
    resource
        .raw
        .pointer("/metadata/uid")?
        .as_str()
        .filter(|uid| !uid.is_empty())
}

fn controller_uid(resource: &Resource) -> Option<&str> {
    resource
        .raw
        .pointer("/metadata/ownerReferences")?
        .as_array()?
        .iter()
        .find(|owner| owner["controller"] == true)?["uid"]
        .as_str()
}

impl Metrics {
    pub(super) fn reset(&mut self) {
        self.history.clear();
        self.remote = Remote::default();
    }

    pub(super) fn record(&mut self, snapshot: &Snapshot) {
        let mut workload_usage = HashMap::<&str, WorkloadUsage>::new();
        let by_uid: HashMap<_, _> = snapshot
            .resources
            .iter()
            .filter_map(|resource| Some((uid(resource)?, resource)))
            .collect();
        for pod in snapshot.resources.iter().filter(|item| item.kind == "Pod") {
            if !matches!(
                pod.raw.pointer("/status/phase").and_then(Value::as_str),
                Some("Succeeded" | "Failed")
            ) {
                if let Some(owner_id) = controller_uid(pod) {
                    workload_usage.entry(owner_id).or_default().add(pod);
                    if let Some(parent_id) = by_uid
                        .get(owner_id)
                        .filter(|owner| matches!(owner.kind.as_str(), "ReplicaSet" | "Job"))
                        .and_then(|owner| controller_uid(owner))
                        .filter(|id| *id != owner_id)
                    {
                        workload_usage.entry(parent_id).or_default().add(pod);
                    }
                }
            }
        }
        for (id, resource) in by_uid {
            let (cpu, memory) = if matches!(resource.kind.as_str(), "Pod" | "Node") {
                (resource.cpu, resource.memory)
            } else if let Some(usage) = workload_usage.get(id) {
                (usage.cpu, usage.memory)
            } else {
                continue;
            };
            if cpu.is_none() && memory.is_none() {
                continue;
            }
            let series = self.history.entry(id.to_owned()).or_default();
            if series
                .back()
                .is_some_and(|sample| sample.stamp >= snapshot.stamp)
            {
                continue;
            }
            series.push_back(Sample {
                stamp: snapshot.stamp,
                cpu,
                memory,
            });
        }
        let cutoff = snapshot.stamp.saturating_sub(RETENTION);
        self.history.retain(|_, series| {
            while series
                .front()
                .is_some_and(|point| point.stamp < cutoff || series.len() > 1441)
            {
                series.pop_front();
            }
            !series.is_empty()
        });
    }

    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        resource: &Resource,
        target: Option<&Target>,
    ) {
        resource_details::section(ui, "Metrics");
        if resource.kind == "Pod" {
            let history = uid(resource).and_then(|id| self.history.get(id));
            ui.columns(2, |columns| {
                for (column, (name, value, color, memory)) in columns.iter_mut().zip([
                    ("CPU", resource.cpu, blue(), false),
                    ("Memory", resource.memory, purple(), true),
                ]) {
                    egui::Frame::new()
                        .fill(side())
                        .stroke(Stroke::new(1.0_f32, border()))
                        .corner_radius(5)
                        .inner_margin(8)
                        .show(column, |ui| {
                            ui.set_min_height(77.0);
                            ui.colored_label(muted(), RichText::new(name).size(11.0));
                            let value = value.map_or_else(
                                || "—".into(),
                                |value| {
                                    if memory {
                                        format!("{:.0} MiB", value * 1024.0)
                                    } else {
                                        format!("{:.0}m", value * 1000.0)
                                    }
                                },
                            );
                            ui.label(RichText::new(value).font(crate::theme::semibold(16.0)));
                            let points: Vec<_> = history
                                .into_iter()
                                .flatten()
                                .rev()
                                .take(15)
                                .filter_map(
                                    |sample| if memory { sample.memory } else { sample.cpu },
                                )
                                .collect();
                            let points = if target.is_none() && points.is_empty() {
                                if memory { vec![0.81, 0.69, 0.86, 0.65, 0.78, 0.84, 0.71, 0.75] }
                                else { vec![0.59, 0.81, 0.57, 0.96, 0.52, 0.81, 0.58, 0.86] }
                            } else { points };
                            if points.len() > 1 {
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), 24.0),
                                    Sense::hover(),
                                );
                                let max = points.iter().copied().fold(0.001, f64::max);
                                let last = (points.len() - 1) as f32;
                                let path = points
                                    .into_iter()
                                    .rev()
                                    .enumerate()
                                    .map(|(i, value)| {
                                        egui::pos2(
                                            egui::lerp(rect.x_range(), i as f32 / last),
                                            egui::lerp(
                                                rect.y_range(),
                                                1.0 - (value / max) as f32 * 0.85,
                                            ),
                                        )
                                    })
                                    .collect();
                                ui.painter()
                                    .add(egui::Shape::line(path, Stroke::new(1.2_f32, color)));
                            }
                        });
                }
            });
            if target.is_none() { ui.colored_label(muted(), RichText::new("Sample history · last 15 min").size(12.0)); }
            egui::CollapsingHeader::new("Metric history")
                .show(ui, |ui| self.show_history(ui, snapshot, resource, target));
            return;
        }
        self.show_history(ui, snapshot, resource, target);
    }

    fn show_history(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        resource: &Resource,
        target: Option<&Target>,
    ) {
        let mut range_index = self.range_index.unwrap_or(1);
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("detail-metrics-range")
                .selected_text(RANGES[range_index].0)
                .show_ui(ui, |ui| {
                    for (index, (label, _)) in RANGES.iter().enumerate() {
                        ui.selectable_value(&mut range_index, index, *label);
                    }
                });
            ui.selectable_value(&mut self.memory, false, "CPU");
            ui.selectable_value(&mut self.memory, true, "Memory");
        });
        self.range_index = Some(range_index);
        let range = RANGES[range_index].1;
        let end = now();
        let start = end.saturating_sub(range);
        let source = target.zip(prometheus_service(snapshot));
        if let Some((target, (endpoint, port))) = &source {
            self.remote
                .poll(target, endpoint, port, resource, start, end);
        }
        let metric = |sample: &Sample| {
            if self.memory {
                sample.memory
            } else {
                sample.cpu
            }
        };
        let local: Vec<_> = uid(resource)
            .and_then(|id| self.history.get(id))
            .into_iter()
            .flatten()
            .filter(|sample| (start..=end).contains(&sample.stamp))
            .filter_map(|sample| Some((sample.stamp, metric(sample)?)))
            .collect();
        let remote: Vec<_> = source
            .as_ref()
            .and(self.remote.result.as_ref())
            .and_then(|result| result.as_ref().ok())
            .into_iter()
            .flatten()
            .filter(|sample| (start..=end).contains(&sample.stamp))
            .filter_map(|sample| Some((sample.stamp, metric(sample)?)))
            .collect();
        let (points, caption, step) = if !remote.is_empty() {
            (remote, "Prometheus", remote_step(range))
        } else if target.is_none() {
            let sample = if self.memory {
                resource.memory
            } else {
                resource.cpu
            };
            (
                sample.map(|value| (end, value)).into_iter().collect(),
                "Demo sample",
                super::REFRESH,
            )
        } else if !local.is_empty() {
            (
                local,
                "Kubernetes Metrics Server · collected during this connection",
                super::REFRESH,
            )
        } else if source.is_some() && self.remote.pending.is_some() {
            (local, "Loading Prometheus history…", super::REFRESH)
        } else {
            (local, "Waiting for metrics samples", super::REFRESH)
        };
        ui.label(RichText::new(caption).small().color(muted()));
        if source.is_some() && caption != "Prometheus" {
            if let Some(Err(error)) = &self.remote.result {
                ui.label(
                    RichText::new("Prometheus history unavailable")
                        .small()
                        .color(muted()),
                )
                .on_hover_text(error);
            }
        }
        chart(ui, &points, start, end, step, self.memory, 162.0);
    }
}

fn prometheus_service(snapshot: &Snapshot) -> Option<(&Resource, String)> {
    snapshot
        .resources
        .iter()
        .filter(|resource| {
            if resource.kind != "Service" || super::service_type(&resource.raw) == "ExternalName" {
                return false;
            }
            let label = resource
                .raw
                .pointer("/metadata/labels/app.kubernetes.io~1name")
                .and_then(Value::as_str);
            resource.name != "prometheus-operated"
                && (resource.name == "prometheus"
                    || resource.name.ends_with("-prometheus")
                    || label == Some("prometheus"))
        })
        .filter_map(|resource| Some((resource, visualize::prometheus_port(resource)?)))
        .min_by_key(|(resource, _)| {
            let priority = match resource.namespace.as_str() {
                "lens-metrics" => 0,
                "monitoring" => 1,
                "istio-system" => 3,
                _ => 2,
            };
            (priority, &resource.namespace, &resource.name)
        })
}

fn remote_step(range: u64) -> u64 {
    (range / 240).max(super::STEP)
}

impl Remote {
    fn poll(
        &mut self,
        target: &Target,
        service: &Resource,
        port: &str,
        resource: &Resource,
        start: u64,
        end: u64,
    ) {
        let query = query(resource);
        let path = format!(
            "/api/v1/namespaces/{}/services/http:{}:{port}/proxy/api/v1/query_range",
            service.namespace, service.name
        );
        let range = end.saturating_sub(start);
        let key = (
            path,
            query,
            uid(resource).unwrap_or_default().to_owned(),
            range,
        );
        if self.key.as_ref() != Some(&key) {
            *self = Self {
                key: Some(key.clone()),
                ..Default::default()
            };
        }
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(result) => self.result = Some(result),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("Metrics worker stopped".into()))
                }
            }
            self.pending = None;
        }
        if self
            .requested
            .is_some_and(|requested| requested.elapsed() < Duration::from_secs(super::REFRESH))
        {
            return;
        }
        self.requested = Some(Instant::now());
        let step = remote_step(range);
        // Align query steps so refreshes don't shift historical sample timestamps.
        let end = end / step * step;
        let created = resource
            .created
            .and_then(|time| u64::try_from(time.unix_timestamp()).ok())
            .unwrap_or(0);
        let start = start.max(created).div_ceil(step) * step;
        if start > end {
            return;
        }
        let url = format!(
            "{}?query={}&start={start}&end={end}&step={step}&timeout=8s",
            key.0,
            visualize::encode_query(&key.1)
        );
        let target = target.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let result = target
                .run(
                    &words(&["get", "--raw", &url, "--request-timeout=12s"]),
                    None,
                )
                .and_then(|output| parse_range(&output));
            let _ = tx.send(result);
        });
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("string")
}

fn query(resource: &Resource) -> String {
    let name = quote(&resource.name);
    let namespace = quote(&resource.namespace);
    let (cpu, memory) = if resource.kind == "Node" {
        // Both the Lens scrape config and kube-prometheus-stack label root cgroups by node.
        let selectors = |metric: &str, counter: bool| {
            ["node", "kubernetes_io_hostname", "instance"]
                .into_iter()
                .map(|label| {
                    let series = format!(r#"{metric}{{id="/",{label}={name}}}"#);
                    if counter {
                        format!("rate({series}[5m])")
                    } else {
                        series
                    }
                })
                .collect::<Vec<_>>()
                .join(" or ")
        };
        (
            format!(
                "max({})",
                selectors("container_cpu_usage_seconds_total", true)
            ),
            format!(
                "max({})",
                selectors("container_memory_working_set_bytes", false)
            ),
        )
    } else {
        let mut filter = format!(r#"namespace={namespace},pod!="",container!="",container!="POD""#);
        let owner_filter = if resource.kind == "Pod" {
            filter.push_str(&format!(",pod={name}"));
            String::new()
        } else {
            let kind = quote(&resource.kind);
            let direct = format!(
                r#"kube_pod_owner{{namespace={namespace},owner_kind={kind},owner_name={name},owner_is_controller="true"}}"#
            );
            let owners = match resource.kind.as_str() {
                "Deployment" | "CronJob" => {
                    let (pod_owner, metric, label) = if resource.kind == "Deployment" {
                        ("ReplicaSet", "kube_replicaset_owner", "replicaset")
                    } else {
                        ("Job", "kube_job_owner", "job_name")
                    };
                    format!(
                        r#"kube_pod_owner{{namespace={namespace},owner_kind="{pod_owner}",owner_is_controller="true"}} and on(namespace,owner_name) label_replace({metric}{{namespace={namespace},owner_kind={kind},owner_name={name}}}, "owner_name", "$1", "{label}", "(.+)")"#
                    )
                }
                _ => direct,
            };
            format!(" and on(namespace,pod) ({owners})")
        };
        (
            format!("sum(max by(namespace,pod,container,cpu)(rate(container_cpu_usage_seconds_total{{{filter}}}[5m])){owner_filter})"),
            format!("sum(max by(namespace,pod,container)(container_memory_working_set_bytes{{{filter}}}){owner_filter})"),
        )
    };
    format!(
        r#"label_replace(({cpu}),"usage","cpu","__name__",".*") or label_replace(({memory}),"usage","memory","__name__",".*")"#
    )
}

fn parse_range(output: &str) -> Result<Vec<Sample>, String> {
    let value: Value = serde_json::from_str(output).map_err(|error| error.to_string())?;
    if value["status"] != "success" || value["data"]["resultType"] != "matrix" {
        return Err(value["error"]
            .as_str()
            .unwrap_or("Expected Prometheus range data")
            .to_owned());
    }
    let rows = value["data"]["result"]
        .as_array()
        .ok_or("Missing Prometheus range data")?;
    let mut points = BTreeMap::new();
    for row in rows {
        let memory = match row["metric"]["usage"].as_str() {
            Some("cpu") => false,
            Some("memory") => true,
            _ => continue,
        };
        for pair in row["values"].as_array().into_iter().flatten() {
            let Some(stamp) = pair[0]
                .as_f64()
                .filter(|stamp| stamp.is_finite() && *stamp >= 0.0)
            else {
                continue;
            };
            let stamp = stamp as u64;
            let Some(value) = pair[1]
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
            else {
                continue;
            };
            let sample = points.entry(stamp).or_insert(Sample {
                stamp,
                cpu: None,
                memory: None,
            });
            if memory {
                sample.memory = Some(value / 1024_f64.powi(3));
            } else {
                sample.cpu = Some(value);
            }
        }
    }
    Ok(points.into_values().collect())
}

pub(super) fn chart(
    ui: &mut egui::Ui,
    points: &[(u64, f64)],
    start: u64,
    end: u64,
    step: u64,
    memory: bool,
    height: f32,
) {
    let unit = if memory { "MiB" } else { "cores" };
    let title = if memory { "Memory Usage" } else { "CPU Usage" };
    let color = if memory { purple() } else { blue() };
    let format_value = |value: f64| {
        if memory {
            format!("{:.1} {unit}", value * 1024.0)
        } else {
            format!("{value:.3} {unit}")
        }
    };
    ui.horizontal_wrapped(|ui| {
        super::legend(ui, color, title);
        if let Some((_, value)) = points.last() {
            ui.label(RichText::new(format_value(*value)).strong());
        }
    });
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let plot = Rect::from_min_max(
        rect.min + Vec2::new(8.0, 10.0),
        rect.max - Vec2::new(60.0, 23.0),
    );
    if !plot.is_positive() {
        return;
    }
    let painter = ui.painter();
    painter.rect_filled(plot, 3.0, side());
    let peak = points
        .iter()
        .map(|(_, value)| *value)
        .fold(if memory { 1.0 / 1024.0 } else { 0.001 }, f64::max);
    let ceiling = (peak / 4.0).log10().floor();
    let magnitude = 10_f64.powf(ceiling);
    let tick = ((peak / 4.0 / magnitude).ceil()) * magnitude;
    let scale = 4.0 * tick;
    for line in 0..=4 {
        let y = plot.bottom() - plot.height() * line as f32 / 4.0;
        painter.hline(plot.x_range(), y, Stroke::new(1.0_f32, border()));
        painter.text(
            Pos2::new(plot.right() + 5.0, y),
            Align2::LEFT_CENTER,
            format_value(tick * f64::from(line)),
            FontId::proportional(10.0),
            muted(),
        );
    }
    let range = end.saturating_sub(start).max(1);
    let x = |stamp: u64| {
        plot.left() + plot.width() * (stamp.saturating_sub(start) as f32 / range as f32)
    };
    for index in 0..=4 {
        let elapsed = range * (4 - index) / 4;
        let label = match elapsed {
            0 => "Now".into(),
            3600.. if elapsed % 3600 == 0 => format!("−{}h", elapsed / 3600),
            _ => format!("−{}m", elapsed / 60),
        };
        let align = if index == 0 {
            Align2::LEFT_TOP
        } else {
            Align2::CENTER_TOP
        };
        painter.text(
            Pos2::new(x(end - elapsed), plot.bottom() + 5.0),
            align,
            label,
            FontId::proportional(10.0),
            muted(),
        );
    }
    if points.is_empty() {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "No samples in this range",
            FontId::proportional(12.0),
            muted(),
        );
        return;
    }
    let half_width = (plot.width() * step as f32 / range as f32 * 0.42).max(1.0);
    let clipped = painter.with_clip_rect(plot);
    for &(stamp, value) in points {
        let x = x(stamp);
        let y = plot.bottom() - ((plot.height() * (value / scale) as f32).max(1.0));
        clipped.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - half_width, y),
                Pos2::new(x + half_width, plot.bottom()),
            ),
            0.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 65),
        );
        clipped.line_segment(
            [Pos2::new(x - half_width, y), Pos2::new(x + half_width, y)],
            Stroke::new(1.5_f32, color),
        );
    }
    if let Some(pos) = response.hover_pos().filter(|pos| plot.contains(*pos)) {
        let hovered = start + (((pos.x - plot.left()) / plot.width()) * range as f32) as u64;
        if let Some(&(stamp, value)) = points
            .iter()
            .min_by_key(|(stamp, _)| stamp.abs_diff(hovered))
        {
            if stamp.abs_diff(hovered) <= step {
                painter.vline(x(stamp), plot.y_range(), Stroke::new(1.0_f32, muted()));
                response.on_hover_text(format!(
                    "{} · {}m ago",
                    format_value(value),
                    end.saturating_sub(stamp) / 60
                ));
            }
        }
    }
}
