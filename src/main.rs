use eframe::egui::{self, Align, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, sync::{mpsc, Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

mod cluster_overview;
mod kubernetes;
mod workloads_overview;
mod detail_metrics;
#[cfg(target_os = "macos")]
mod process_usage;
#[cfg(target_os = "macos")]
mod updates;
mod resource_details;
mod visualize;
use kubernetes::{LocalContexts, Stream, Target};

const BG: Color32 = Color32::from_rgb(24, 26, 28);
const PANEL: Color32 = Color32::from_rgb(31, 33, 35);
const SIDE: Color32 = Color32::from_rgb(38, 41, 43);
const BORDER: Color32 = Color32::from_rgb(45, 47, 49);
const BLUE: Color32 = Color32::from_rgb(61, 144, 206);
const GREEN: Color32 = Color32::from_rgb(76, 175, 80);
const ORANGE: Color32 = Color32::from_rgb(255, 152, 0);
const PURPLE: Color32 = Color32::from_rgb(201, 61, 206);
const MUTED: Color32 = Color32::from_rgb(170, 172, 174);
const TEXT: Color32 = Color32::from_rgb(241, 243, 245);
const REFRESH: u64 = 60;
const ALL_NAMESPACES: &str = "All namespaces";
const STEP: u64 = 60;
fn is_master(node: &Resource) -> bool {
    node.raw.pointer("/metadata/labels/node-role.kubernetes.io~1control-plane").is_some()
        || node.raw.pointer("/metadata/labels/node-role.kubernetes.io~1master").is_some()
}
fn text(v: &Value, pointer: &str) -> String { v.pointer(pointer).and_then(Value::as_str).unwrap_or("").to_owned() }
fn num(v: &Value, pointer: &str) -> u64 { v.pointer(pointer).and_then(Value::as_u64).unwrap_or(0) }
fn array(v: &Value, pointer: &str) -> Vec<Value> { v.pointer(pointer).and_then(Value::as_array).cloned().unwrap_or_default() }
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
fn quantity(s: &str, memory: bool) -> f64 {
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-').unwrap_or(s.len());
    let n = s[..split].parse::<f64>().unwrap_or(0.0);
    let suffix = &s[split..];
    if memory { n * match suffix { "Ki" => 1024.0, "Mi" => 1024.0_f64.powi(2), "Gi" => 1024.0_f64.powi(3), "Ti" => 1024.0_f64.powi(4), "k" | "K" => 1e3, "M" => 1e6, "G" => 1e9, _ => 1.0 } / 1024.0_f64.powi(3) }
    else { n * match suffix { "n" => 1e-9, "u" => 1e-6, "m" => 1e-3, _ => 1.0 } }
}
fn resource_quantity(value: &Value, resource: &str) -> Option<f64> {
    let value = value.as_str()?;
    let memory = resource == "memory";
    // Kubernetes also accepts DecimalExponent quantities such as 1e9.
    let amount = match value.parse::<f64>() {
        Ok(number) if memory => number / 1024_f64.powi(3),
        Ok(number) => number,
        Err(_) => quantity(value, memory),
    };
    amount.is_finite().then_some(amount)
}

fn status_color(s: &str) -> Color32 { if ["Running", "Ready", "Active", "Available", "Bound", "Normal"].contains(&s) { GREEN } else if s == "Unavailable" { Color32::from_rgb(241, 94, 94) } else if ["Pending", "Warning", "CrashLoopBackOff", "NotReady"].contains(&s) { ORANGE } else { MUTED } }

#[derive(Clone)]
struct Resource { name: String, namespace: String, kind: String, status: String, node: String, ready: String, cpu: Option<f64>, memory: Option<f64>, restarts: u64, created: Option<OffsetDateTime>, raw: Value }
impl Resource {
    fn replicas(&self) -> u64 { desired_replicas(&self.raw) }

    fn qos(&self) -> Option<&str> { self.raw.pointer("/status/qosClass")?.as_str().filter(|value| !value.is_empty()) }

    fn controller_kind(&self) -> Option<&str> {
        self.raw.pointer("/metadata/ownerReferences")?.as_array()?.iter()
            .find(|owner| owner["controller"] == true)?["kind"].as_str().filter(|value| !value.is_empty())
    }

    fn parse(v: Value) -> Self {
        let kind = text(&v,"/kind");
        let mut status = text(&v,"/status/phase");
        let mut ready = String::new();
        if kind == "Node" {
            status = if array(&v,"/status/conditions").iter().any(|c| text(c,"/type")=="Ready" && text(c,"/status")=="True") {"Ready"} else {"NotReady"}.into();
        } else if kind == "Pod" {
            let containers = array(&v,"/status/containerStatuses");
            ready = format!("{}/{}",containers.iter().filter(|c|c["ready"]==true).count(), array(&v,"/spec/containers").len());
            for c in &containers { let reason=text(c,"/state/waiting/reason"); if !reason.is_empty() {status=reason;} }
        } else if kind == "Deployment" {
            let desired = desired_replicas(&v);
            let ready_count = num(&v, "/status/readyReplicas");
            let available = v.pointer("/status/availableReplicas").and_then(Value::as_u64).unwrap_or(ready_count);
            ready = format!("{ready_count}/{desired}");
            status = if available >= desired { "Running" } else { "Unavailable" }.into();
        } else if matches!(kind.as_str(), "StatefulSet" | "ReplicaSet" | "DaemonSet") {
            let (available, desired) = if kind == "DaemonSet" {
                (num(&v, "/status/numberReady"), num(&v, "/status/desiredNumberScheduled"))
            } else {
                let desired = desired_replicas(&v);
                let available = v.pointer("/status/availableReplicas").and_then(Value::as_u64)
                    .unwrap_or_else(|| num(&v, "/status/readyReplicas"));
                (available, desired)
            };
            ready = format!("{available}/{desired}"); status = if available >= desired {"Available"}else{"Pending"}.into();
        } else if kind == "Service" {
            status = if v.pointer("/metadata/deletionTimestamp").and_then(Value::as_str).is_some() {
                "Terminating"
            } else if service_type(&v) == "LoadBalancer" && service_ingress_addresses(&v).next().is_none() {
                "Pending"
            } else {
                "Active"
            }.into();
        }
        else if kind == "Event" { status=text(&v,"/type"); }
        if status.is_empty() {status="Active".into();}
        let created = v.pointer("/metadata/creationTimestamp").and_then(Value::as_str)
            .and_then(|stamp| OffsetDateTime::parse(stamp, &Rfc3339).ok());
        Self { name:text(&v,"/metadata/name"),namespace:text(&v,"/metadata/namespace"),kind,status,node:text(&v,"/spec/nodeName"),ready,cpu:None,memory:None,restarts:array(&v,"/status/containerStatuses").iter().map(|c|num(c,"/restartCount")).sum(),created,raw:v }
    }
}

fn desired_replicas(v: &Value) -> u64 {
    v.pointer("/spec/replicas").and_then(Value::as_u64).unwrap_or(1)
}

fn service_type(v: &Value) -> &str {
    v.pointer("/spec/type").and_then(Value::as_str).unwrap_or("ClusterIP")
}

fn service_ingress_addresses(v: &Value) -> impl Iterator<Item = &str> {
    v.pointer("/status/loadBalancer/ingress").and_then(Value::as_array).into_iter().flatten()
        .flat_map(|ingress| ["ip", "hostname"].into_iter().filter_map(move |key| ingress[key].as_str()))
        .filter(|address| !address.is_empty())
}

fn service_ports(v: &Value) -> impl Iterator<Item = (u64, &Value)> {
    v.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|port| Some((port["port"].as_u64()?, port)))
}

#[derive(Clone, Default)]
struct Snapshot { resources: Vec<Resource>, errors: Vec<String>, related: Vec<Resource>, detail_errors: HashMap<&'static str, String>, stamp: u64, loaded: usize }
impl Snapshot {
    fn kind(&self, k: &str) -> Vec<&Resource> {self.resources.iter().filter(|r|r.kind==k).collect()}

}
fn demo() -> Snapshot {
    let mut resources=Vec::new();
    for i in 0..3 {
        let mut n=Resource::parse(json!({"kind":"Node","metadata":{"name":format!("worker-0{}",i+1),"labels":{"node-role.kubernetes.io/worker":""}},"status":{"capacity":{"cpu":"8","memory":"34Gi","pods":"110"},"allocatable":{"cpu":"8","memory":"32Gi","pods":"110"},"conditions":[{"type":"Ready","status":"True"}]}}));
        n.cpu=Some([2.8,4.9,1.7][i]); n.memory=Some([15.2,21.6,10.4][i]); resources.push(n);
    }
    for (i,name) in ["gateway","web-frontend","orders-api","payments","redis","metrics-agent"].iter().enumerate() {
        let ns=if i==5 {"monitoring"}else{"production"};
        let ready = if i == 3 { 1 } else { 2 };
        resources.push(Resource::parse(json!({"kind":"Deployment","metadata":{"name":name,"namespace":ns},"spec":{"replicas":2},"status":{"readyReplicas":ready,"availableReplicas":ready}})));
        resources.push(Resource::parse(json!({"kind":"Service","metadata":{"name":name,"namespace":ns},"spec":{"type":"ClusterIP","clusterIP":format!("10.96.0.{}",10+i),"ports":[{"port":80}],"selector":{"app":name}}})));
        for j in 0..2 {
            let waiting=i==3&&j==1;
            let mut p=Resource::parse(json!({"kind":"Pod","metadata":{"name":format!("{name}-7d8f6c-{j}k4xz"),"namespace":ns,"labels":{"app":name}},"spec":{"nodeName":format!("worker-0{}",(i+j)%3+1),"containers":[{"name":name,"image":format!("example/{name}:1.4.2"),"resources":{"requests":{"cpu":"100m","memory":"128Mi"},"limits":{"cpu":"500m","memory":"512Mi"}}}]},"status":{"phase":if waiting{"Pending"}else{"Running"},"containerStatuses":[{"ready":!waiting,"restartCount":if waiting{3}else{0},"state":if waiting{json!({"waiting":{"reason":"CrashLoopBackOff"}})}else{json!({"running":{}})}}]}}));
            p.cpu=Some(0.08+i as f64*0.035);p.memory=Some(0.13+i as f64*0.04); resources.push(p);
        }
    }
    for ns in ["production","monitoring","kube-system"] {resources.push(Resource::parse(json!({"kind":"Namespace","metadata":{"name":ns},"status":{"phase":"Active"}})));}
    resources.push(Resource::parse(json!({"kind":"Event","metadata":{"name":"payments.backoff","namespace":"production"},"type":"Warning","reason":"BackOff","message":"Restart back-off for the payments container","involvedObject":{"kind":"Pod","name":"payments-7d8f6c-1k4xz"}})));
    resources.push(Resource::parse(json!({"kind":"Event","metadata":{"name":"orders.scaled","namespace":"production"},"type":"Normal","reason":"ScalingReplicaSet","message":"Scaled orders-api to 2 replicas","involvedObject":{"kind":"Deployment","name":"orders-api"}})));
    resources.push(Resource::parse(json!({"kind":"PersistentVolumeClaim","metadata":{"name":"redis-data","namespace":"production"},"spec":{"resources":{"requests":{"storage":"10Gi"}}},"status":{"phase":"Bound"}})));
    resources.push(Resource::parse(json!({"kind":"ConfigMap","metadata":{"name":"gateway-config","namespace":"production"},"data":{"LOG_LEVEL":"info"}})));
    Snapshot { resources, stamp: now(), loaded: 1, ..Default::default() }
}

#[derive(Clone, Copy)]
enum OutputTo {
    Console,
    Action,
    ManifestSave,
    ManifestLoad,
}

enum Reply {
    Snapshot { snapshot: Snapshot, cancelled: Arc<AtomicBool> },
    Contexts { discovered: LocalContexts, connect_current: bool, source: String },
    Output(OutputTo, Result<String, String>),
}

#[derive(Clone)]
struct Action {
    target: Target,
    args: Vec<String>,
    input: Option<String>,
    output_to: OutputTo,
}

struct Manifest {
    yaml: String,
    original: Option<Value>,
    feedback: String,
}

const LOG_CHROME: Color32 = Color32::from_rgb(22, 32, 47);
const LOG_SURFACE: Color32 = Color32::from_rgb(13, 20, 31);
const LOG_BORDER: Color32 = Color32::from_rgb(49, 65, 85);
const LOG_TIMESTAMP: Color32 = Color32::from_rgb(143, 166, 195);

struct PodLogs {
    pod: Resource,
    container: String,
    filter: String,
    mode: Option<&'static str>,
    stream: Option<Result<Stream, String>>,
}

impl PodLogs {
    fn load(&mut self, target: Option<&Target>, mode: Option<&'static str>) {
        let Some(target) = target else { return };
        if self.container.is_empty() { return; }
        let mut args = pod_args("logs", &self.pod, &self.container, &["--tail=200", "--timestamps=true"]);
        if let Some(flag) = mode { args.push(flag.into()); }
        if mode != Some("--follow") { args.push("--request-timeout=15s".into()); }
        self.mode = mode;
        self.stream = Some(Stream::start(target, &args));
    }

    fn show(&mut self, ui: &mut egui::Ui, target: Option<&Target>) {
        if target.is_some() {
            ui.horizontal_wrapped(|ui| {
                if container_picker(ui, &self.pod, &mut self.container) {
                    self.load(target, Some("--follow"));
                }
                ui.separator();
                let enabled = !self.container.is_empty();
                for (label, mode) in [("Recent", None), ("Follow", Some("--follow")), ("Previous", Some("--previous"))] {
                    let active = self.stream.is_some() && self.mode == mode;
                    if ui.add_enabled(enabled, egui::Button::new(label).selected(active)).clicked() {
                        self.load(target, mode);
                    }
                }
                if let Some(Ok(logs)) = &mut self.stream {
                    if ui.add_enabled(logs.status.is_none(), egui::Button::new("Stop")).clicked() {
                        logs.stop();
                    }
                    if ui.button("Copy").clicked() { ui.ctx().copy_text(clean_log_text(&logs.output()).into_owned()); }
                }
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter log lines…").desired_width(175.0));
            });
        }
        egui::Frame::new().fill(LOG_SURFACE).stroke(Stroke::new(1.0_f32, LOG_BORDER))
            .corner_radius(7).inner_margin(10).show(ui, |ui| {
                egui::ScrollArea::both().id_salt(("pod-log-output", &self.pod.namespace, &self.pod.name, &self.container))
                    .auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                        if target.is_none() {
                            show_log_text(ui, "[INFO] Service initialized\n[INFO] Readiness check passed", "");
                            return;
                        }
                        match &self.stream {
                            Some(Ok(logs)) => {
                                let output = logs.output();
                                if output.is_empty() {
                                    let status = if logs.status.is_none() { "Waiting for log output…" } else { "This container returned no log output." };
                                    ui.colored_label(MUTED, status);
                                } else {
                                    show_log_text(ui, &output, &self.filter);
                                }
                                if let Some(status) = &logs.status { ui.colored_label(MUTED, status); }
                            }
                            Some(Err(error)) => { ui.colored_label(ORANGE, error); }
                            None => { ui.label("Choose a container to read its logs."); }
                        }
                    });
            });
    }
}

fn clean_log_text(output: &str) -> std::borrow::Cow<'_, str> {
    if !output.contains("\x1b[") { return output.into(); }
    let mut clean = String::with_capacity(output.len());
    let mut rest = output;
    while let Some((plain, escape)) = rest.split_once("\x1b[") {
        clean.push_str(plain);
        let parameters = escape.bytes().take_while(|byte| (0x20..=0x3f).contains(byte)).count();
        rest = if escape.as_bytes().get(parameters).is_some_and(|byte| (0x40..=0x7e).contains(byte)) {
            &escape[parameters + 1..]
        } else {
            escape
        };
    }
    clean.push_str(rest);
    clean.into()
}

fn log_level(message: &str) -> Option<(&str, Color32)> {
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

fn show_log_text(ui: &mut egui::Ui, output: &str, filter: &str) {
    let output = clean_log_text(output);
    let query = filter.to_lowercase();
    let mut job = egui::text::LayoutJob::default();
    let format = |color| egui::TextFormat {
        font_id: FontId::monospace(12.5),
        line_height: Some(19.0),
        color,
        ..Default::default()
    };
    for (index, line) in output.lines()
        .filter(|line| query.is_empty() || line.to_lowercase().contains(&query)).enumerate() {
        if index > 0 { job.append("\n", 0.0, format(TEXT)); }
        let mut message = line;
        if let Some((timestamp, _)) = line.split_once(' ') {
            if timestamp.as_bytes().get(10) == Some(&b'T') && timestamp.ends_with('Z') {
                let split = timestamp.len() + 1;
                job.append(&line[..split], 0.0, format(LOG_TIMESTAMP));
                message = &line[split..];
            }
        }
        if let Some((level, color)) = log_level(message) {
            let start = message.len() - message.trim_start().len();
            let end = start + level.len();
            job.append(&message[..start], 0.0, format(TEXT));
            job.append(&message[start..end], 0.0, format(color));
            message = &message[end..];
        }
        job.append(message, 0.0, format(TEXT));
    }
    if job.text.is_empty() {
        ui.colored_label(MUTED, "No log lines match the filter.");
    } else {
        ui.add(egui::Label::new(job).extend().selectable(true));
    }
}

struct Forward {
    resource: String,
    namespace: String,
    local: u16,
    remote: u16,
    stream: Stream,
}

#[derive(Clone)]
struct Sample {stamp:u64,cpu:Option<f64>,memory:Option<f64>}
struct Observatory {
    snapshot:Snapshot, demo:bool,page:String,namespace:String,search:String,
    cluster_overview: cluster_overview::Overview, selected:Option<Resource>, detail_tab:String,
    connections:bool,contexts:Vec<Target>,context:String,exe:String,kubeconfig:String,
    tx:mpsc::Sender<Reply>,rx:mpsc::Receiver<Reply>,busy:bool,last_refresh:Instant,
    output:String,command:String,console:bool,notice:String,confirm:Option<Action>,replicas:u32,
    target: Option<Target>,
    fetch_cancel: Option<Arc<AtomicBool>>,
    repaint: egui::Context,
    visualize: visualize::Visualize,
    detail_metrics: detail_metrics::Metrics,
    #[cfg(target_os = "macos")]
    process_usage: process_usage::ProcessUsage,
    #[cfg(target_os = "macos")]
    updates: updates::Updates,
    manifest: Option<Manifest>,
    container: String,
    pod_logs: Option<PodLogs>,
    forwards: Vec<Forward>,
    local_port: u16,
    remote_port: u16,
    shot:Option<PathBuf>,frames:u32,shot_requested:bool,
    nav_open:HashMap<&'static str,bool>,
    table_sort: HashMap<&'static str, (ResourceColumn, bool)>,
    table_widths: HashMap<(&'static str, ResourceColumn), f32>,
    table_widths_dir: Option<PathBuf>,
    pending_table_width: Option<(&'static str, ResourceColumn)>,
}
impl Observatory {
    fn new(cc:&eframe::CreationContext<'_>,shot:Option<PathBuf>, target: Option<Target>)->Self {
        let mut style=(*cc.egui_ctx.style()).clone();style.visuals=egui::Visuals::dark();
        style.visuals.panel_fill=BG;style.visuals.window_fill=PANEL;style.visuals.override_text_color=Some(TEXT);
        style.visuals.widgets.inactive.bg_fill=SIDE;style.visuals.widgets.inactive.weak_bg_fill=SIDE;
        style.visuals.selection.bg_fill=BLUE.gamma_multiply(0.35);style.spacing.item_spacing=Vec2::new(10.0,10.0);
        style.text_styles.insert(egui::TextStyle::Body,FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Heading,FontId::proportional(25.0));
        cc.egui_ctx.set_style(style);
        let (tx, rx) = mpsc::channel();
        let demo = shot.is_some() && target.is_none();
        let snapshot = if demo { self::demo() } else { Snapshot::default() };
        let table_widths_dir = shot.is_none().then(table_widths_dir).flatten();
        let table_widths = table_widths_dir.as_deref().map(read_table_widths).unwrap_or_default();
        let mut app = Self{snapshot,demo,page:"Overview".into(),namespace:if demo { ALL_NAMESPACES.into() } else { String::new() },search:String::new(),cluster_overview: Default::default(),selected:None,detail_tab:"Details".into(),connections:false,contexts:vec![],context:String::new(),exe:default_kubectl(),kubeconfig:String::new(),tx,rx,busy:false,last_refresh:Instant::now(),output:String::new(),command:"get pods -A".into(),console:false,notice:String::new(),confirm:None,replicas:2,shot,frames:0,shot_requested:false,nav_open:HashMap::new(),
            target: None, fetch_cancel: None, repaint: cc.egui_ctx.clone(), visualize: Default::default(), detail_metrics: Default::default(), manifest: None, container: String::new(),
            #[cfg(target_os = "macos")]
            process_usage: Default::default(),
            #[cfg(target_os = "macos")]
            updates: Default::default(),
            pod_logs: None, forwards: Vec::new(), local_port: 8080, remote_port: 80,
            table_sort: HashMap::new(),
            table_widths, table_widths_dir, pending_table_width: None,
        };
        if demo { app.cluster_overview.record(&app.snapshot); }
        if let Some(target) = target {
            app.exe = target.exe.clone();
            app.kubeconfig = target.config.clone();
            app.context = target.context.clone();
            app.switch(Some(target));
        } else if !app.demo {
            app.discover(true);
        }
        app
    }
    fn discover(&mut self, connect_current: bool) {
        let target = Target { exe: self.exe.trim().into(), config: self.kubeconfig.trim().into(), context: String::new() };
        self.busy = true;
        self.notice = "Scanning local kubeconfigs…".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let source = target.config.clone();
            let discovered = LocalContexts::discover(target);
            let _ = tx.send(Reply::Contexts { discovered, connect_current, source });
        });
    }

    fn start_fetch(&mut self) {
        let Some(target) = self.target.clone().filter(|_| !self.busy) else { return };
        self.busy = true;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.fetch_cancel = Some(cancelled.clone());
        let tx = self.tx.clone();
        let repaint = self.repaint.clone();
        std::thread::spawn(move || {
            let snapshot = fetch(&target, &cancelled);
            let _ = tx.send(Reply::Snapshot { snapshot, cancelled });
            repaint.request_repaint();
        });
    }

    fn switch(&mut self, target: Option<Target>) {
        if (self.busy && self.fetch_cancel.is_none())
            || (self.target == target && (target.is_some() || self.demo)) { return; }
        if let Some(cancelled) = self.fetch_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
            self.busy = false;
        }
        if let Some(target) = &target {
            if !self.contexts.contains(target) { self.contexts.push(target.clone()); }
        }
        self.demo = target.is_none();
        self.target = target;
        self.snapshot = if self.demo { demo() } else { Snapshot::default() };
        self.cluster_overview.clear();
        self.detail_metrics.reset();
        if self.demo {
            self.cluster_overview.record(&self.snapshot);
            if self.page == "Port Forwarding" { self.page = "Overview".into(); }
        }
        self.namespace = if self.demo { ALL_NAMESPACES.into() } else { String::new() };
        self.nav_open.insert("cluster", true);
        self.visualize.reset_scope();
        self.search.clear();
        self.selected = None;
        self.pod_logs = None;
        self.forwards.clear();
        self.manifest = None;
        self.confirm = None;
        self.output.clear();
        self.notice.clear();
        self.start_fetch();
    }

    fn select(&mut self, resource: Resource) {
        self.replicas = resource.raw.pointer("/spec/replicas").and_then(Value::as_u64).unwrap_or(1) as u32;
        self.container = containers(&resource).into_iter().next().unwrap_or_default();
        self.remote_port = suggested_port(&resource).unwrap_or(80);
        self.detail_tab = "Details".into();
        self.selected = Some(resource);
    }

    fn queue(&mut self, args: Vec<String>, input: Option<String>, output_to: OutputTo) {
        if let Some(target) = self.target.clone() {
            self.confirm = Some(Action { target, args, input, output_to });
        }
    }

    fn execute(&mut self, action: Action) {
        self.busy = true;
        if matches!(action.output_to, OutputTo::Console) { self.output = "Running…".into(); }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = action.target.run(&action.args, action.input);
            let _ = tx.send(Reply::Output(action.output_to, result));
        });
    }

    fn load_manifest(&mut self, resource: &Resource) {
        let Some(target) = self.target.clone() else { return };
        self.manifest = None;
        self.busy = true;
        let tx = self.tx.clone();
        let resource = resource.clone();
        std::thread::spawn(move || {
            let result = target.run(&resource_args("get", &resource, words(&["-o", "json"])), None)
                .and_then(|output| {
                    let loaded: Value = serde_json::from_str(&output).map_err(|error| error.to_string())?;
                    if same_object(&resource.raw, &loaded) { Ok(output) }
                    else { Err("The resource was replaced. Refresh and select it again.".into()) }
                });
            let _ = tx.send(Reply::Output(OutputTo::ManifestLoad, result));
        });
    }

    fn poll(&mut self) {
        if let Some(Ok(logs)) = self.pod_logs.as_mut().and_then(|view| view.stream.as_mut()) { logs.poll(); }
        for forward in &mut self.forwards { forward.stream.poll(); }
        while let Ok(reply) = self.rx.try_recv() {
            if matches!(&reply, Reply::Snapshot { cancelled, .. } if cancelled.load(Ordering::Relaxed)) {
                continue;
            }
            self.busy = false;
            match reply {
                Reply::Snapshot { snapshot, .. } => {
                    self.fetch_cancel = None;
                    self.last_refresh = Instant::now();
                    if snapshot.loaded == 0 {
                        self.snapshot.errors = snapshot.errors;
                        self.notice = "Cluster refresh failed".into();
                        continue;
                    }
                    self.cluster_overview.record(&snapshot);
                    self.detail_metrics.record(&snapshot);
                    if let Some(resource) = &self.selected {
                        self.selected = snapshot.resources.iter().find(|updated| same_object(&resource.raw, &updated.raw)).cloned();
                    }
                    self.snapshot = snapshot;
                    self.notice = "Cluster refreshed".into();
                }
                Reply::Contexts { discovered, connect_current, source } => {
                    let LocalContexts { entries, current, error } = discovered;
                    self.notice = error.unwrap_or_else(|| format!("{} local contexts found", entries.len()));
                    if let Some(current) = &current {
                        if self.context.is_empty() { self.context = current.context.clone(); }
                    }
                    if source.is_empty() {
                        self.contexts = entries;
                    } else if !entries.is_empty() {
                        self.contexts.retain(|entry| entry.config != source);
                        self.contexts.extend(entries);
                    }
                    if let Some(active) = &self.target {
                        if !self.contexts.contains(active) { self.contexts.push(active.clone()); }
                    }
                    if connect_current && self.target.is_none() {
                        if let Some(current) = current {
                            self.context = current.context.clone();
                            self.switch(Some(current));
                        }
                    }
                }
                Reply::Output(OutputTo::ManifestLoad, result) => {
                    match result.and_then(parse_manifest) {
                        Ok(manifest) => self.manifest = Some(manifest),
                        Err(error) => self.notice = format!("Could not load YAML: {error}"),
                    }
                }
                Reply::Output(destination, result) => {
                    let succeeded = result.is_ok();
                    let output = match result {
                        Ok(s) if s.trim().is_empty() => "Completed successfully".into(),
                        Ok(s) => s,
                        Err(error) => format!("Command failed\n{error}"),
                    };
                    match destination {
                        OutputTo::Console => self.output = output,
                        OutputTo::Action => { self.output = output; self.console = true; }
                        OutputTo::ManifestSave => {
                            if let Some(manifest) = &mut self.manifest {
                                manifest.feedback = output.clone();
                            }
                            if succeeded { self.manifest = None; self.output = output; self.console = true; }
                        }
                        OutputTo::ManifestLoad => unreachable!(),
                    }
                    if !matches!(destination, OutputTo::Console) && (succeeded || matches!(destination, OutputTo::ManifestSave)) {
                        self.start_fetch();
                    }
                }
            }
        }
    }
    fn in_namespace(&self, namespace: &str) -> bool {
        namespace.is_empty() || (!self.namespace.is_empty() && (self.namespace == ALL_NAMESPACES || namespace == self.namespace))
    }

    fn page_uses_namespace(page: &str) -> bool {
        page != "Overview"
            && CATALOG.iter().find(|entry| entry.page == page).is_none_or(|entry| entry.namespaced)
    }

    fn filtered(&self, kind: &str) -> Vec<Resource> {
        let search = self.search.to_lowercase();
        self.snapshot.resources.iter().filter(|resource| {
            if resource.kind != kind || !self.in_namespace(&resource.namespace) { return false; }
            if search.is_empty() { return true; }
            let mut fields = vec![resource.name.as_str(), &resource.namespace, &resource.status];
            if kind == "Event" {
                fields.extend([event_message(resource), event_source(resource),
                    event_object(resource)["name"].as_str().unwrap_or_default(),
                    event_object(resource)["kind"].as_str().unwrap_or_default(),
                    resource.raw["reason"].as_str().unwrap_or_default()]);
            }
            fields.join(" ").to_lowercase().contains(&search)
        }).cloned().collect()
    }
    fn active_context(&self) -> &str {
        self.target.as_ref().map_or("Local Kubernetes", |target| target.context.as_str())
    }
    fn top(&mut self,ctx:&egui::Context){egui::TopBottomPanel::top("header").frame(egui::Frame::new().fill(PANEL).inner_margin(14)).show(ctx,|ui|{ui.horizontal(|ui|{
        let (logo, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        nav_icon(ui.painter(), logo.center(), "cluster", BLUE);ui.label(RichText::new("CLUSTER OBSERVATORY").size(13.0).strong());
        ui.separator();
        let title = if self.demo { "example-production" } else { self.active_context() };
        ui.add_sized([270.0, 20.0], egui::Label::new(title).truncate()).on_hover_text(title);
        badge(ui,if self.demo{"SAMPLE DATA"}else if self.snapshot.loaded > 0 {"KUBERNETES"} else if self.busy {"CONNECTING"} else if self.target.is_none() {"DISCONNECTED"} else {"UNAVAILABLE"},if self.demo{ORANGE}else{BLUE});
        ui.with_layout(egui::Layout::right_to_left(Align::Center),|ui|{
            if ui.button("Connections").clicked(){self.connections=true;}
            #[cfg(target_os = "macos")]
            self.updates.toolbar(ui);
            if ui.add_enabled(self.target.is_some() && !self.namespace.is_empty() && !self.busy && self.manifest.is_none(), egui::Button::new("Apply YAML…")).clicked() {
                self.manifest = Some(Manifest { yaml: String::new(), original: None, feedback: String::new() });
            }
            if ui.button(if self.busy{"Refreshing…"}else{"↻  Refresh"}).clicked(){if self.demo{self.snapshot=demo();self.cluster_overview.record(&self.snapshot);self.notice="Sample refreshed".into();}else if !self.busy { if self.target.is_some() { self.start_fetch(); } else { self.discover(true); } }}
            if self.busy {
                if self.page != "Overview" || self.snapshot.loaded == 0 || self.fetch_cancel.is_none() { ui.spinner(); }
            } else {ui.label(RichText::new(if self.demo{"Demo workspace"}else if self.target.is_some() {"Refresh every 60s"} else {""}).color(MUTED).size(12.0));}
        });
    });});}
    fn sidebar(&mut self,ctx:&egui::Context){
        egui::SidePanel::left("sidebar").exact_width(256.0).resizable(false)
            .frame(egui::Frame::new().fill(SIDE).inner_margin(0)).show(ctx,|ui|{
            ui.spacing_mut().item_spacing=Vec2::ZERO;
            ui.add_space(12.0);
            let footer_height = 78.0;
            egui::ScrollArea::vertical().id_salt("navigation-tree").auto_shrink([false, false])
                .max_height((ui.available_height() - footer_height).max(100.0))
                .show(ui, |ui| self.cluster_sidebar(ui));
            ui.with_layout(egui::Layout::bottom_up(Align::Min),|ui|{
                ui.add_space(8.0);
                if nav_row(ui,"Cluster connections","cluster",0,false,None,None).clicked(){self.connections=true;}
                if nav_row(ui,"Command panel","terminal",0,self.console,None,None).clicked(){self.console = !self.console;}
                ui.add_space(7.0);ui.separator();
            });
        });
    }
    fn cluster_sidebar(&mut self, ui: &mut egui::Ui) {
        let root_open = self.nav_open.get("cluster").copied().unwrap_or(true);
        if self.demo {
            if nav_row(ui, "example-production", "cluster", 0, true, None, Some(root_open)).clicked() {
                self.nav_open.insert("cluster", !root_open);
            }
            if root_open { self.resource_navigation(ui); }
        }
        if self.contexts.is_empty() && self.demo { return; }

        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("LOCAL KUBECONFIGS").size(11.0).strong().color(MUTED));
        });
        ui.add_space(4.0);
        let can_switch = (!self.busy || self.fetch_cancel.is_some()) && self.confirm.is_none();
        let mut next = None;
        for index in 0..self.contexts.len() {
            let target = &self.contexts[index];
            let active = self.target.as_ref() == Some(target);
            let expanded = active && root_open;
            let source = if target.config.is_empty() { "kubectl defaults" } else { &target.config };
            let tooltip = format!("{}\n{}", target.context, source);
            let clicked = ui.add_enabled_ui(can_switch || active, |ui| {
                nav_row(ui, cluster_label(&target.context), "cluster", 0, active, None, Some(expanded))
                    .on_hover_text(tooltip).clicked()
            }).inner;
            if clicked {
                if active {
                    self.nav_open.insert("cluster", !expanded);
                } else {
                    next = Some(target.clone());
                }
            }
            if expanded { self.resource_navigation(ui); }
        }
        if self.contexts.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(if self.busy { "Scanning…" } else { "No local contexts found" }).color(MUTED));
            });
        }
        if let Some(target) = next {
            self.exe = target.exe.clone();
            self.kubeconfig = target.config.clone();
            self.context = target.context.clone();
            self.switch(Some(target));
        }
    }

    fn resource_navigation(&mut self, ui: &mut egui::Ui) {
        self.nav_page(ui, "Overview", "Overview", "overview", 1, None);
        self.nav_page(ui, "Nodes", "Nodes", "nodes", 1, Some("Node"));
        self.nav_page(ui, "Visualize", "Visualize", "topology", 1, None);
        for (group, icon) in [
            ("Workloads", "workloads"), ("Config", "config"), ("Network", "network"),
            ("Storage", "storage"), ("Access Control", "namespace"),
        ] {
            let open = self.nav_open.get(group).copied().unwrap_or(self.demo && group == "Workloads");
            if nav_row(ui, group, icon, 1, false, None, Some(open)).clicked() {
                self.nav_open.insert(group, !open);
            }
            if open {
                if group == "Workloads" {
                    self.nav_page(ui, "Workloads Overview", "Overview", icon, 2, None);
                }
                for entry in CATALOG.iter().filter(|entry| entry.group == group) {
                    self.nav_page(ui, entry.page, entry.page, icon, 2, Some(entry.kind));
                }
                if group == "Network" && !self.demo {
                    self.nav_page(ui, "Port Forwarding", "Port Forwarding", icon, 2, None);
                }
            }
        }
        self.nav_page(ui, "Namespaces", "Namespaces", "namespace", 1, Some("Namespace"));
        self.nav_page(ui, "Events", "Events", "events", 1, Some("Event"));
    }

    fn nav_page(&mut self,ui:&mut egui::Ui,page:&str,label:&str,icon:&str,depth:usize,kind:Option<&str>){
        let count = kind.filter(|_| !self.namespace.is_empty() || !Self::page_uses_namespace(page)).map(|kind| {
            self.snapshot.kind(kind).into_iter().filter(|resource| self.in_namespace(&resource.namespace)).count()
        });
        let response=nav_row(ui,label,icon,depth,self.page==page,count,None);
        if response.on_hover_text(label).clicked(){
            self.page=page.into();self.selected=None;self.search.clear();
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let uses_namespace = Self::page_uses_namespace(&self.page);
        ui.horizontal(|ui| {
            ui.heading(&self.page);
            if !uses_namespace { return; }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let label = if self.namespace.is_empty() { "Select a namespace" } else { &self.namespace };
                let key = self.target.as_ref().map(|target| (&target.context, &target.config));
                egui::ComboBox::from_id_salt(("namespace", key)).selected_text(label).width(190.0).show_ui(ui, |ui| {
                    changed |= ui.selectable_value(&mut self.namespace, ALL_NAMESPACES.into(), ALL_NAMESPACES).changed();
                    let namespaces: std::collections::BTreeSet<_> = self.snapshot.resources.iter()
                        .map(|resource| if resource.kind == "Namespace" { &resource.name } else { &resource.namespace })
                        .filter(|namespace| !namespace.is_empty()).collect();
                    for namespace in namespaces {
                        changed |= ui.selectable_value(&mut self.namespace, namespace.clone(), namespace).changed();
                    }
                });
                ui.label(RichText::new("Namespace").color(MUTED).size(12.0));
            });
        });
        if changed {
            self.selected = None;
            self.visualize.reset_scope();
        }
        if !uses_namespace || !self.namespace.is_empty() {
            let description = match self.page.as_str() {
                "Overview" => "Resource health, capacity and activity across the cluster.",
                "Workloads Overview" => "Workload status and events for the selected namespace.",
                "Visualize" => "Explore the Kubernetes topology and observed service traffic.",
                _ => "Browse resources. Select a row to inspect details, configuration and actions.",
            };
            ui.colored_label(MUTED, description);
        }
        ui.add_space(14.0);
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        match self.cluster_overview.show(ui, &self.snapshot, self.demo) {
            Some(cluster_overview::Action::Open(resource)) => self.select(resource.clone()),
            Some(cluster_overview::Action::Events) => {
                self.page = "Events".into();
                self.namespace = ALL_NAMESPACES.into();
                self.selected = None;
                self.search.clear();
            }
            None => {}
        }
    }
    fn resources(&mut self, ui: &mut egui::Ui) {
        let workloads_overview = self.page == "Workloads Overview";
        let table_page = if workloads_overview { "Events" } else { &self.page };
        let Some(entry) = CATALOG.iter().find(|entry| entry.page == table_page) else { return };
        let mut rows = Vec::new();
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() * 0.55).clamp(160.0, 360.0);
            ui.add_sized([search_width, 28.0], egui::TextEdit::singleline(&mut self.search)
                .hint_text(if entry.kind == "Event" { "Search Events…" } else { "Search resources…" }));
            rows = self.filtered(entry.kind);
            let unit = match (entry.kind, rows.len()) {
                ("Event", 1) => "event", ("Event", _) => "events",
                (_, 1) => "resource", _ => "resources",
            };
            ui.colored_label(MUTED, format!("{} {unit}", rows.len()));
        });
        ui.add_space(8.0);

        if workloads_overview {
            if let Some(page) = workloads_overview::show(ui, &self.snapshot, &self.namespace) {
                self.page = page.into();
                self.selected = None;
                self.search.clear();
                return;
            }
            ui.add_space(10.0);
        }
        let columns = if workloads_overview {
            use ResourceColumn::*;
            vec![EventType, Source, Namespace, InvolvedObject, Message, Count, Age, LastSeen]
        } else {
            ResourceColumn::for_table(entry, self.namespace == ALL_NAMESPACES)
        };

        let minimum_width: f32 = columns.iter().map(|column| column.default_width(entry.kind)).sum();
        let viewport_width = ui.available_width();
        let extra_width = (viewport_width - minimum_width).max(0.0);
        let growth: f32 = columns.iter().map(|column| column.growth()).sum();
        let columns: Vec<_> = columns.into_iter().map(|column| {
            let cell_width = self.table_widths.get(&(entry.kind, column)).copied()
                .unwrap_or_else(|| column.default_width(entry.kind) + extra_width * column.growth() / growth);
            (column, cell_width)
        }).collect();
        let columns_width: f32 = columns.iter().map(|(_, width)| width).sum();
        let width = columns_width.max(viewport_width);
        let mut sort = self.table_sort.get(entry.kind).copied()
            .filter(|(sorted, _)| columns.iter().any(|(column, _)| column == sorted))
            .unwrap_or(if entry.kind == "Event" { (ResourceColumn::LastSeen, true) } else { (ResourceColumn::Name, false) });
        let resources_by_uid: HashMap<_, _> = if entry.kind == "Event" {
            self.snapshot.resources.iter().enumerate().filter_map(|(index, resource)| {
                Some((resource.raw.pointer("/metadata/uid")?.as_str()?, index))
            }).collect()
        } else {
            HashMap::new()
        };
        let mut selection = None;
        let mut logs = None;
        egui::ScrollArea::horizontal().drag_to_scroll(false)
            .id_salt(("resource-table", entry.kind)).show(ui, |ui| {
            ui.set_min_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (_, header) = ui.allocate_space(Vec2::new(width, 34.0));
            ui.painter().rect_filled(header, 4.0, SIDE);
            for (column, cell) in resource_cells(header, &columns) {
                let grip_left = cell.right() - 12.0;
                let sort_area = Rect::from_min_max(cell.min, Pos2::new(grip_left, cell.bottom()));
                let grip = Rect::from_min_max(Pos2::new(grip_left, cell.top()), cell.max);
                let response = ui.interact(sort_area, ui.id().with(("sort", column)), Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() {
                    sort = (column, sort.0 == column && !sort.1);
                    self.table_sort.insert(entry.kind, sort);
                    ui.ctx().request_repaint();
                }
                let title = column.title();
                let label = if sort.0 == column {
                    format!("{title} {}", if sort.1 { "↓" } else { "↑" })
                } else {
                    title.into()
                };
                if response.hovered() || response.has_focus() {
                    ui.painter().rect_filled(cell, 4.0, Color32::from_rgb(49, 55, 61));
                }
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                draw_resource_cell(ui.painter(), sort_area, title.into(), column, true, None);
                if sort.0 == column {
                    let center = Pos2::new(cell.right() - 17.0, cell.center().y);
                    let tip = if sort.1 { 2.0 } else { -2.0 };
                    ui.painter().add(egui::Shape::line(
                        vec![
                            center + Vec2::new(-3.0, -tip),
                            center + Vec2::new(0.0, tip),
                            center + Vec2::new(3.0, -tip),
                        ],
                        Stroke::new(1.0_f32, TEXT),
                    ));
                }

                let resize = ui.interact(grip, ui.id().with(("resize-column", column)), Sense::drag())
                    .on_hover_and_drag_cursor(egui::CursorIcon::ResizeHorizontal);
                let delta = resize.drag_delta().x;
                if delta != 0.0 {
                    let new_width = column.clamp_width(cell.width() + delta);
                    let key = (entry.kind, column);
                    self.table_widths.insert(key, new_width);
                    self.pending_table_width = Some(key);
                    ui.ctx().request_repaint();
                }
                let color = if resize.hovered() || resize.dragged() { BLUE } else { MUTED.gamma_multiply(0.25) };
                let x = cell.right() - 1.0;
                ui.painter().line_segment(
                    [Pos2::new(x, cell.top() + 7.0), Pos2::new(x, cell.bottom() - 7.0)],
                    Stroke::new(1.0_f32, color),
                );
            }

            rows.sort_by(|left, right| sort.0.compare(left, right, sort.1)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.namespace.cmp(&right.namespace)));
            if rows.is_empty() {
                ui.add_space(16.0);
                ui.colored_label(MUTED, "No resources match this view.");
                ui.add_space(16.0);
            }
            for (index, resource) in rows.iter().enumerate() {
                let (_, row) = ui.allocate_space(Vec2::new(width, 38.0));
                let id = ui.id().with((&resource.kind, &resource.namespace, &resource.name));
                let response = ui.interact(row, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let selected = self.selected.as_ref().is_some_and(|item| same_object(&item.raw, &resource.raw));
                let fill = if selected { Color32::from_rgb(33, 52, 70) }
                    else if response.hovered() || response.has_focus() { SIDE }
                    else if index % 2 == 0 { PANEL } else { BG };
                let painter = ui.painter();
                painter.rect_filled(row, 0.0, fill);
                painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, BORDER));
                if selected {
                    painter.rect_filled(Rect::from_min_size(row.min, Vec2::new(3.0, row.height())), 0.0, BLUE);
                }
                let mut linked_resource = None;
                for (column, cell) in resource_cells(row, &columns) {
                    let warning = column == ResourceColumn::Message && resource.status == "Warning";
                    let mut accent = warning.then_some(ORANGE);
                    let label = if workloads_overview && column == ResourceColumn::LastSeen {
                        relative_age(event_last_seen(resource))
                    } else {
                        column.value(resource)
                    };
                    if column == ResourceColumn::InvolvedObject {
                        let target = match event_object(resource)["uid"].as_str().filter(|uid| !uid.is_empty()) {
                            Some(uid) => resources_by_uid.get(uid).map(|&index| &self.snapshot.resources[index]),
                            None => self.snapshot.resources.iter().find(|item| event_for(resource, item)),
                        };
                        if let Some(target) = target {
                            let link = ui.interact(cell, id.with("involved-object"), Sense::click())
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            link.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, &label));
                            accent = Some(if link.hovered() || link.has_focus() { BLUE.gamma_multiply(1.35) } else { BLUE });
                            if link.on_hover_text(&label).clicked() { linked_resource = Some(target); }
                        }
                    }
                    draw_resource_cell(painter, cell, label, column, false, accent);
                }

                let caption = if entry.kind == "Event" { event_message(resource) } else { &resource.name };
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, caption));
                let response = match entry.kind {
                    "Event" => response.on_hover_text(caption),
                    "Node" => response.on_hover_text(format!("Used / total capacity\nCPU: {}\nMemory: {}",
                        ResourceColumn::Cpu.value(resource), ResourceColumn::Memory.value(resource))),
                    _ => response,
                };
                if let Some(target) = linked_resource {
                    selection = Some(target.clone());
                } else if response.clicked() {
                    selection = Some(resource.clone());
                }
                if resource.kind == "Pod" {
                    response.context_menu(|ui| {
                        if ui.button("View logs").clicked() {
                            logs = Some(resource);
                            ui.close_menu();
                        }
                    });
                }
            }
        });
        if let Some(pod) = logs {
            self.open_pod_logs(pod, containers(pod).into_iter().next().unwrap_or_default());
        } else if let Some(resource) = selection {
            self.select(resource);
        }
    }

    fn save_pending_table_width(&mut self) {
        let Some((kind, column)) = self.pending_table_width.take() else { return };
        let Some(dir) = &self.table_widths_dir else { return };
        if let Err(error) = write_table_width(dir, kind, column, self.table_widths[&(kind, column)]) {
            self.notice = format!("Could not save column width: {error}");
        }
    }

    fn workspace(&mut self, ui: &mut egui::Ui) {
        if !self.demo && self.snapshot.loaded == 0 {
            ui.heading("Local Kubernetes");
            if self.busy {
                ui.spinner();
                ui.label(if self.target.is_some() { "Loading resources from the cluster…" } else { "Scanning local kubeconfigs…" });
            } else {
                ui.label(if self.target.is_some() { "Could not load cluster resources." } else { "Select a discovered context to connect." });
                if ui.button("Connections").clicked() { self.connections = true; }
                if let Some(error) = self.snapshot.errors.first() { ui.colored_label(ORANGE, error); }
            }
            return;
        }
        self.toolbar(ui);
        if Self::page_uses_namespace(&self.page) && self.namespace.is_empty() {
            ui.label("Select a namespace to view resources in this cluster.");
            return;
        }
        if !self.snapshot.errors.is_empty() {
            let label = RichText::new(format!("{} data sources need attention", self.snapshot.errors.len())).color(ORANGE);
            egui::CollapsingHeader::new(label).show(ui, |ui| {
                for error in &self.snapshot.errors { ui.label(error); }
            });
            ui.add_space(10.0);
        }
        match self.page.as_str() {
            "Overview" => self.overview(ui),
            "Visualize" => self.visualize(ui),
            "Port Forwarding" => self.port_forwards(ui),
            _ => self.resources(ui),
        }
    }

    fn visualize(&mut self, ui: &mut egui::Ui) {
        let resources: Vec<_> = self.snapshot.resources.iter()
            .filter(|resource| self.in_namespace(&resource.namespace)).collect();
        let namespace = (self.namespace != ALL_NAMESPACES).then_some(self.namespace.as_str());
        let selected = self.visualize.show(ui, &resources, &self.snapshot.resources, self.target.as_ref(), namespace);
        if let Some(resource) = selected { self.select(resource); }
    }

    fn details(&mut self, ctx: &egui::Context) {
        let Some(resource) = self.selected.clone() else { return };
        let max_width = (ctx.available_rect().width() * 0.70).clamp(310.0, 760.0);
        let mut close = false;
        egui::SidePanel::right("resource-details")
            .resizable(true)
            .default_width(650.0_f32.min(max_width))
            .width_range(310.0..=max_width)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(14))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let close_button = ui.small_button("×").on_hover_text("Close details");
                        close_button.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close details")
                        });
                        close = close_button.clicked();
                        ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                            let title = format!("{}: {}", resource.kind, resource.name);
                            ui.add(egui::Label::new(RichText::new(&title).strong().size(15.0)).truncate())
                                .on_hover_text(title);
                        });
                    });
                });
                if close { return; }

                ui.horizontal_wrapped(|ui| {
                    for tab in ["Details", "Configuration", "Events"] {
                        ui.selectable_value(&mut self.detail_tab, tab.into(), tab);
                    }
                    if resource.kind == "Pod" && ui.button("Logs").clicked() {
                        self.open_pod_logs(&resource, self.container.clone());
                    }
                    if matches!(resource.kind.as_str(), "Pod" | "Service") {
                        ui.selectable_value(&mut self.detail_tab, "Port Forward".into(), "Port Forward");
                    }
                });
                ui.separator();
                if self.selected.is_none() { return; }

                let scroll_key = (&resource.kind, &resource.namespace, &resource.name, &self.detail_tab);
                egui::ScrollArea::vertical().id_salt(scroll_key).auto_shrink([false, false]).show(ui, |ui| {
                    match self.detail_tab.as_str() {
                        "Configuration" => {
                            let json = serde_json::to_string_pretty(&resource.raw).unwrap_or_default();
                            if resource.kind == "Secret" { ui.label("Secret metadata. Open the YAML editor to fetch its values."); }
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Copy JSON").clicked() { ctx.copy_text(json.clone()); }
                                let label = if resource.kind == "Event" { "View YAML…" } else { "Edit YAML…" };
                                if ui.add_enabled(!self.demo && !self.busy && self.manifest.is_none(), egui::Button::new(label)).clicked() {
                                    self.load_manifest(&resource);
                                }
                            });
                            egui::ScrollArea::horizontal().show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(json).monospace()).extend());
                            });
                        }
                        "Events" => resource_details::events(ui, &self.snapshot, &resource),
                        "Port Forward" => self.forward_form(ui, &resource),
                        _ => self.resource_details(ui, &resource),
                    }
                });
            });
        if close { self.selected = None; }
    }

    fn resource_details(&mut self, ui: &mut egui::Ui, resource: &Resource) {
        ui.spacing_mut().item_spacing.y = 3.0;
        match resource_details::show(ui, &self.snapshot, resource, &mut self.detail_metrics, self.target.as_ref()) {
            Some(resource_details::Action::Forward(port)) => {
                self.remote_port = port;
                self.local_port = port;
                self.detail_tab = "Port Forward".into();
                return;
            }
            Some(resource_details::Action::Open(related)) => {
                self.select(related.clone());
                return;
            }
            None => {}
        }
        resource_details::section(ui, "Actions");
        if resource.kind == "Pod" {
            container_picker(ui, resource, &mut self.container);
            if ui.button("Read logs").clicked() {
                self.open_pod_logs(resource, self.container.clone());
            }
            #[cfg(target_os = "macos")]
            if ui.add_enabled(self.target.is_some() && !self.container.is_empty() && !self.busy, egui::Button::new("Open Shell in Terminal")).clicked() {
                let args = pod_args("exec", resource, &self.container, &["-it", "--", "sh"]);
                let target = self.target.clone().expect("connected cluster");
                self.busy = true;
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Reply::Output(OutputTo::Console, target.open_terminal(&args)));
                });
                self.console = true;
            }
        }
        if matches!(resource.kind.as_str(), "Deployment" | "StatefulSet" | "ReplicaSet") {
            ui.horizontal(|ui| {
                ui.label("Replicas");
                ui.add(egui::DragValue::new(&mut self.replicas).range(0..=i32::MAX as u32));
                if ui.add_enabled(!self.demo && !self.busy, egui::Button::new("Scale…")).clicked() {
                    let mut flags = vec![format!("--replicas={}", self.replicas)];
                    let version = text(&resource.raw, "/metadata/resourceVersion");
                    if !version.is_empty() { flags.push(format!("--resource-version={version}")); }
                    self.queue(resource_args("scale", resource, flags), None, OutputTo::Action);
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("View configuration").clicked() { self.detail_tab = "Configuration".into(); }
            if ui.add_enabled(!self.demo && !self.busy, egui::Button::new(RichText::new("Delete resource…").color(ORANGE))).clicked() {
                self.queue(resource_args("delete", resource, words(&["--wait=false"])), None, OutputTo::Action);
            }
        });
        resource_details::events(ui, &self.snapshot, resource);
    }

    fn open_pod_logs(&mut self, pod: &Resource, container: String) {
        self.console = false;
        self.selected = None;
        if self.pod_logs.as_ref().is_some_and(|logs| same_object(&logs.pod.raw, &pod.raw) && logs.container == container) {
            return;
        }
        let mut logs = PodLogs { pod: pod.clone(), container, filter: String::new(), mode: None, stream: None };
        logs.load(self.target.as_ref(), Some("--follow"));
        self.pod_logs = Some(logs);
    }

    fn log_dock(&mut self, ctx: &egui::Context) {
        if self.console { return; }
        let Some(logs) = &mut self.pod_logs else { return };
        let mut close = false;
        let max_height = (ctx.screen_rect().height() * 0.55).max(165.0);
        egui::TopBottomPanel::bottom("pod-logs-dock")
            .resizable(true).default_height(275.0_f32.min(max_height)).height_range(165.0..=max_height)
            .frame(egui::Frame::new().fill(LOG_CHROME).inner_margin(12))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(">_").monospace().strong().color(LOG_TIMESTAMP));
                    ui.label(RichText::new("Pod logs").strong().size(15.0));
                    ui.separator();
                    ui.add(egui::Label::new(RichText::new(format!("{}/{}", logs.pod.namespace, logs.pod.name))
                        .monospace().color(LOG_TIMESTAMP)).truncate());
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        close = ui.small_button("Close logs").clicked();
                        match &logs.stream {
                            Some(Ok(stream)) if stream.status.is_none() => {
                                if logs.mode == Some("--follow") { badge(ui, "LIVE", GREEN); }
                                else { badge(ui, "LOADING", LOG_TIMESTAMP); }
                            }
                            Some(Ok(_)) => badge(ui, "ENDED", LOG_TIMESTAMP),
                            Some(Err(_)) => badge(ui, "ERROR", ORANGE),
                            None => {}
                        }
                    });
                });
                logs.show(ui, self.target.as_ref());
            });
        if close { self.pod_logs = None; }
    }

    fn forward_form(&mut self, ui: &mut egui::Ui, resource: &Resource) {
        ui.label("Forward a TCP port to this computer.");
        ui.horizontal_wrapped(|ui| {
            ui.label("Local port");
            ui.add(egui::DragValue::new(&mut self.local_port).range(1..=u16::MAX));
            ui.label("Remote port");
            ui.add(egui::DragValue::new(&mut self.remote_port).range(1..=u16::MAX));
            if ui.add_enabled(!self.demo, egui::Button::new("Start")).clicked() {
                let name = format!("{}/{}", resource.kind.to_lowercase(), resource.name);
                let ports = format!("{}:{}", self.local_port, self.remote_port);
                let args = words(&["port-forward", &name, &ports, "-n", &resource.namespace, "--address=127.0.0.1"]);
                if let Some(target) = &self.target {
                    match Stream::start(target, &args) {
                        Ok(stream) => {
                            self.forwards.push(Forward {
                                resource: format!("{} · {}/{}", resource.kind, resource.namespace, resource.name),
                                namespace: resource.namespace.clone(),
                                local: self.local_port,
                                remote: self.remote_port,
                                stream,
                            });
                            self.page = "Port Forwarding".into();
                            self.selected = None;
                        }
                        Err(error) => self.notice = error,
                    }
                }
            }
            if !self.demo && ui.link("Manage sessions").clicked() {
                self.page = "Port Forwarding".into();
                self.selected = None;
                }
        });
    }

    fn port_forwards(&mut self, ui: &mut egui::Ui) {
        let scope = &self.namespace;
        let mut visible = false;
        let mut remove = None;
        for (index, forward) in self.forwards.iter_mut().enumerate() {
            if scope != ALL_NAMESPACES && scope != &forward.namespace { continue; }
            visible = true;
            ui.push_id(index, |ui| {
                card(ui, |ui| {
                    let output = forward.stream.output();
                    let running = forward.stream.status.is_none();
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(&forward.resource);
                        ui.label(format!("127.0.0.1:{} → {}", forward.local, forward.remote));
                        if running && output.contains("Forwarding from 127.0.0.1:") && ui.button("Copy address").clicked() {
                            ui.ctx().copy_text(format!("127.0.0.1:{}", forward.local));
                        }
                        if running && ui.button("Stop").clicked() { forward.stream.stop(); }
                        if !running && ui.button("Remove").clicked() { remove = Some(index); }
                    });
                    let state = if output.contains("Forwarding from 127.0.0.1:") { "Listening" } else { "Starting…" };
                    ui.colored_label(MUTED, forward.stream.status.as_deref().unwrap_or(state));
                    if !output.is_empty() {
                        egui::CollapsingHeader::new("Session output").show(ui, |ui| { ui.monospace(output); });
                    }
                });
            });
        }
        if !visible { ui.label("Open a Pod or Service and select Port Forward to start a session."); }
        if let Some(index) = remove { self.forwards.remove(index); }
    }

    fn manifest_editor(&mut self, ctx: &egui::Context) {
        let Some(manifest) = &mut self.manifest else { return };
        let mut open = true;
        let mut submit = None;
        let readonly = manifest.original.as_ref().is_some_and(|original| original["kind"] == "Event");
        let context = self.target.as_ref().map_or("", |target| target.context.as_str());
        egui::Window::new(if manifest.original.is_some() { "Resource YAML" } else { "Apply manifest" })
            .open(&mut open).default_width(720.0).resizable(true).show(ctx, |ui| {
                ui.label(format!("Context: {context}"));
                egui::ScrollArea::both().max_height(480.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut manifest.yaml).code_editor().desired_width(680.0)
                        .desired_rows(18).interactive(!readonly && !self.busy)
                        .hint_text("Paste Kubernetes YAML here"));
                });
                if !manifest.feedback.is_empty() { ui.label(&manifest.feedback); }
                if readonly { return; }
                let button = if manifest.original.is_some() { "Save changes…" } else { "Apply…" };
                if ui.add_enabled(!self.busy && !manifest.yaml.trim().is_empty(), egui::Button::new(button)).clicked() {
                    match manifest_request(manifest, &self.namespace) {
                        Ok(request) => submit = Some(request),
                        Err(error) => manifest.feedback = error,
                    }
                }
            });
        if !open && !self.busy { self.manifest = None; }
        if let Some((args, input)) = submit { self.queue(args, Some(input), OutputTo::ManifestSave); }
    }

    fn connections(&mut self, ctx: &egui::Context) {
        if !self.connections { return; }
        let mut open = true;
        egui::Window::new("Cluster connections").open(&mut open).default_width(500.0).collapsible(false).show(ctx, |ui| {
            ui.label("Connect using your existing Kubernetes credentials.");
            ui.label("Kubectl executable");
            ui.add_enabled(!self.busy, egui::TextEdit::singleline(&mut self.exe));
            ui.label("Kubeconfig path (optional)");
            let config = ui.add_enabled(!self.busy, egui::TextEdit::singleline(&mut self.kubeconfig));
            if config.changed() { self.context.clear(); }
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy, egui::Button::new("Scan local kubeconfigs")).clicked() {
                    self.kubeconfig.clear();
                    self.context.clear();
                    self.discover(false);
                }
                if ui.add_enabled(!self.busy && !self.kubeconfig.trim().is_empty(), egui::Button::new("Read file")).clicked() {
                    self.discover(false);
                }
            });
            ui.label("Local contexts");
            ui.add_enabled_ui(!self.busy, |ui| {
                egui::ComboBox::from_id_salt("context-picker")
                    .selected_text(if self.context.is_empty() { "Choose a context" } else { &self.context })
                    .width(350.0).show_ui(ui, |ui| {
                        for entry in &self.contexts {
                            let source = if entry.config.is_empty() { "kubectl defaults" } else { &entry.config };
                            let selected = self.context == entry.context && self.kubeconfig == entry.config;
                            let label = if entry.config.is_empty() {
                                entry.context.clone()
                            } else {
                                let file = std::path::Path::new(&entry.config).file_name().unwrap_or_default().to_string_lossy();
                                format!("{}  ·  {}", entry.context, file)
                            };
                            if ui.selectable_label(selected, label).on_hover_text(source).clicked() {
                                self.context = entry.context.clone();
                                self.kubeconfig = entry.config.clone();
                            }
                        }
                    });
                ui.text_edit_singleline(&mut self.context);
            });
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.busy && !self.context.trim().is_empty() && !self.exe.trim().is_empty(),
                    egui::Button::new("Connect cluster").fill(BLUE)).clicked() {
                    let target = Target { exe: self.exe.trim().into(), config: self.kubeconfig.trim().into(), context: self.context.trim().into() };
                    if self.target.as_ref() == Some(&target) { self.start_fetch(); } else { self.switch(Some(target)); }
                    self.connections = false;
                }
                if ui.add_enabled(!self.busy, egui::Button::new("Use sample workspace")).clicked() {
                    self.switch(None);
                    self.connections = false;
                }
            });
            if !self.notice.is_empty() { ui.label(&self.notice); }
        });
        if !open { self.connections = false; }
    }
    fn console(&mut self,ctx:&egui::Context){if !self.console{return}egui::TopBottomPanel::bottom("console").resizable(true).default_height(215.0).frame(egui::Frame::new().fill(Color32::from_rgb(16,18,20)).inner_margin(13)).show(ctx,|ui|{
        ui.horizontal(|ui|{ui.label(RichText::new("KUBECTL COMMAND PANEL").strong().size(11.0));ui.label(RichText::new("Arguments only · no shell").color(MUTED).size(11.0));ui.with_layout(egui::Layout::right_to_left(Align::Center),|ui|{if ui.small_button("Close").clicked(){self.console=false;}if ui.small_button("Copy output").clicked(){ctx.copy_text(self.output.clone());}});});
        ui.horizontal(|ui|{ui.label(RichText::new("kubectl").color(BLUE));let response=ui.add(egui::TextEdit::singleline(&mut self.command).font(egui::TextStyle::Monospace).desired_width(ui.available_width()-70.0));let run=ui.add_enabled(!self.busy && self.confirm.is_none(),egui::Button::new("Run")).clicked()||(!self.busy && self.confirm.is_none() && response.lost_focus()&&ui.input(|i|i.key_pressed(egui::Key::Enter)));if run{
            if self.demo{self.output="Connect a cluster to run kubectl commands. Sample mode does not execute commands.".into();}
            else{let args:Vec<String>=self.command.split_whitespace().map(str::to_owned).collect();if args.is_empty(){self.output="Enter kubectl arguments, for example: get pods -A".into();}else{self.queue(args, None, OutputTo::Console);}}
        }});egui::ScrollArea::both().stick_to_bottom(true).show(ui,|ui|{ui.monospace(&self.output);});
    });}
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ResourceColumn {
    Name,
    Namespace,
    Status,
    Ready,
    Cpu,
    Memory,
    Containers,
    Restarts,
    ControlledBy,
    Node,
    Qos,
    Age,
    Pods,
    Replicas,
    ServiceType,
    ClusterIp,
    Ports,
    ExternalIp,
    Selector,
    EventType,
    Message,
    InvolvedObject,
    Source,
    Count,
    LastSeen,
}

impl ResourceColumn {
    fn for_table(entry: &ResourceType, all_namespaces: bool) -> Vec<Self> {
        use ResourceColumn::*;
        match entry.kind {
            "Pod" => vec![Name, Namespace, Cpu, Memory, Containers, Restarts, ControlledBy, Node, Qos, Status, Age],
            "Deployment" => vec![Name, Namespace, Pods, Replicas, Age, Status],
            "Service" => vec![Name, Namespace, ServiceType, ClusterIp, Ports, ExternalIp, Selector, Age, Status],
            "Event" => vec![EventType, Message, Namespace, InvolvedObject, Source, Count, Age, LastSeen],
            _ => {
                let mut columns = vec![Name];
                if entry.namespaced && all_namespaces { columns.push(Namespace); }
                columns.push(Status);
                if matches!(entry.kind, "StatefulSet" | "ReplicaSet" | "DaemonSet") { columns.push(Ready); }
                if entry.kind == "Node" { columns.extend([Cpu, Memory]); }
                columns
            }
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::Namespace => "NAMESPACE",
            Self::Status => "STATUS",
            Self::Ready => "READY",
            Self::Cpu => "CPU",
            Self::Memory => "MEMORY",
            Self::Containers => "CONTAINERS",
            Self::Restarts => "RESTARTS",
            Self::ControlledBy => "CONTROLLED BY",
            Self::Node => "NODE",
            Self::Qos => "QOS",
            Self::Age => "AGE",
            Self::Pods => "PODS",
            Self::Replicas => "REPLICAS",
            Self::ServiceType => "TYPE",
            Self::ClusterIp => "CLUSTER IP",
            Self::Ports => "PORTS",
            Self::ExternalIp => "EXTERNAL IP",
            Self::Selector => "SELECTOR",
            Self::EventType => "TYPE",
            Self::Message => "MESSAGE",
            Self::InvolvedObject => "INVOLVED OBJECT",
            Self::Source => "SOURCE",
            Self::Count => "COUNT",
            Self::LastSeen => "LAST SEEN",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Name => 175.0,
            Self::Namespace => 155.0,
            Self::Status => 158.0,
            Self::Ready | Self::Pods => 68.0,
            Self::Cpu | Self::Memory => 104.0,
            Self::Containers => 110.0,
            Self::Restarts | Self::Replicas => 92.0,
            Self::ControlledBy => 128.0,
            Self::Node => 116.0,
            Self::Qos => 98.0,
            Self::Age => 60.0,
            Self::ServiceType => 111.0,
            Self::ClusterIp | Self::ExternalIp | Self::Selector => 120.0,
            Self::Ports => 145.0,
            Self::EventType => 88.0,
            Self::Message => 232.0,
            Self::InvolvedObject => 180.0,
            Self::Source => 126.0,
            Self::Count => 78.0,
            Self::LastSeen => 166.0,
        }
    }

    fn default_width(self, kind: &str) -> f32 {
        match (kind, self) {
            ("Pod" | "Service", Self::Name) => 158.0,
            ("Pod" | "Service", Self::Namespace) => 106.0,
            ("Event", Self::Namespace) => 110.0,
            ("Pod" | "Service", Self::Status) => 105.0,
            ("Pod", Self::Cpu | Self::Memory) => 90.0,
            ("Node", Self::Cpu | Self::Memory) => 164.0,
            _ => self.width(),
        }
    }

    fn growth(self) -> f32 {
        match self {
            Self::Name => 4.0,
            Self::Message => 5.0,
            Self::Namespace | Self::Node => 1.5,
            Self::Status => 2.0,
            _ => 0.5,
        }
    }

    fn clamp_width(self, width: f32) -> f32 {
        width.clamp(self.width().min(90.0), 1200.0)
    }

    fn value(self, resource: &Resource) -> String {
        match self {
            Self::Ready | Self::Containers | Self::Pods => resource.ready.clone(),
            Self::Cpu | Self::Memory if resource.kind == "Node" => {
                let (key, usage, unit) = if self == Self::Cpu {
                    ("cpu", resource.cpu, "cores")
                } else {
                    ("memory", resource.memory, "MiB")
                };
                let total = resource_quantity(&resource.raw["status"]["capacity"][key], key);
                let format_value = |value: Option<f64>, precision| {
                    value.map_or_else(|| "—".into(), |value| {
                        let value = if key == "memory" { value * 1024.0 } else { value };
                        format!("{value:.precision$}")
                    })
                };
                let usage_precision = if key == "cpu" { 3 } else { 0 };
                let total_precision = if key == "cpu" && total.is_some_and(|value| value.fract() != 0.0) { 3 } else { 0 };
                format!("{} / {} {unit}", format_value(usage, usage_precision), format_value(total, total_precision))
            }
            Self::Cpu => resource.cpu.map(|cpu| {
                if resource.kind == "Pod" { format!("{cpu:.3}") } else { format!("{cpu:.3} cores") }
            }).unwrap_or_else(|| "—".into()),
            Self::Memory => resource.memory.map(|memory| {
                let mib = memory * 1024.0;
                if resource.kind == "Pod" { format!("{mib:.1}MiB") } else { format!("{mib:.0} MiB") }
            }).unwrap_or_else(|| "—".into()),
            Self::Restarts => resource.restarts.to_string(),
            Self::Count => event_count(resource).to_string(),
            Self::LastSeen => event_timestamp(event_last_seen(resource)),
            Self::InvolvedObject => event_object_label(resource),
            Self::Replicas => resource.replicas().to_string(),
            Self::ClusterIp | Self::Ports | Self::ExternalIp | Self::Selector =>
                self.service_value(resource).unwrap_or_else(|| "—".into()),
            Self::Age => relative_age(resource_age_start(resource)),
            _ => self.text_value(resource).unwrap_or("—").into(),
        }
    }

    fn service_value(self, resource: &Resource) -> Option<String> {
        let spec = &resource.raw["spec"];
        let strings = |value: &Value| value.as_array().into_iter().flatten()
            .filter_map(Value::as_str).filter(|value| !value.is_empty()).map(str::to_owned).collect::<Vec<_>>();
        let values = match self {
            Self::ClusterIp => {
                let ips = strings(&spec["clusterIPs"]);
                if ips.is_empty() {
                    spec["clusterIP"].as_str().filter(|ip| !ip.is_empty()).map(str::to_owned).into_iter().collect()
                } else { ips }
            }
            Self::Ports => service_ports(&resource.raw).map(|(number, port)| {
                let protocol = port["protocol"].as_str().unwrap_or("TCP");
                let target = match &port["targetPort"] {
                    Value::Number(target) if target.as_u64() != Some(number) => Some(target.to_string()),
                    Value::String(target) if !target.is_empty() => Some(target.clone()),
                    _ => None,
                };
                match target {
                    Some(target) => format!("{number}:{target}/{protocol}"),
                    None => format!("{number}/{protocol}"),
                }
            }).collect(),
            Self::ExternalIp => {
                let mut addresses = strings(&spec["externalIPs"]);
                let name = spec["externalName"].as_str().filter(|name| !name.is_empty());
                for address in service_ingress_addresses(&resource.raw).chain(name) {
                    if !addresses.iter().any(|existing| existing == address) { addresses.push(address.into()); }
                }
                addresses
            }
            Self::Selector => spec["selector"].as_object()?.iter()
                .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
                .collect(),
            _ => return None,
        };
        (!values.is_empty()).then(|| values.join(", "))
    }

    fn text_value(self, resource: &Resource) -> Option<&str> {
        match self {
            Self::Name => Some(&resource.name),
            Self::Namespace => Some(&resource.namespace),
            Self::Status | Self::EventType => Some(&resource.status),
            Self::Message => Some(event_message(resource)),
            Self::Source => Some(event_source(resource)),
            Self::ControlledBy => resource.controller_kind(),
            Self::Node => Some(resource.node.as_str()).filter(|node| !node.is_empty()),
            Self::Qos => resource.qos(),
            Self::ServiceType => Some(service_type(&resource.raw)),
            _ => None,
        }
    }

    fn compare(self, left: &Resource, right: &Resource, descending: bool) -> std::cmp::Ordering {
        let ready = |resource: &Resource| {
            let (ready, total) = resource.ready.split_once('/')?;
            Some((ready.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
        };
        match self {
            Self::Cpu => compare_table_values(left.cpu, right.cpu, descending, f64::total_cmp),
            Self::Memory => compare_table_values(left.memory, right.memory, descending, f64::total_cmp),
            Self::Ready | Self::Containers | Self::Pods => compare_table_values(ready(left), ready(right), descending, Ord::cmp),
            Self::Age => compare_table_values(resource_age_start(left), resource_age_start(right), descending, |left, right| right.cmp(left)),
            Self::LastSeen => compare_table_values(event_last_seen(left), event_last_seen(right), descending, Ord::cmp),
            Self::Count => compare_table_values(Some(event_count(left)), Some(event_count(right)), descending, Ord::cmp),
            Self::InvolvedObject => compare_table_values(Some(event_object_label(left)), Some(event_object_label(right)), descending, Ord::cmp),
            Self::Restarts => compare_table_values(Some(left.restarts), Some(right.restarts), descending, Ord::cmp),
            Self::Replicas => compare_table_values(Some(left.replicas()), Some(right.replicas()), descending, Ord::cmp),
            Self::Ports => {
                let port_key = |resource: &Resource| {
                    let (number, _) = service_ports(&resource.raw).next()?;
                    Some((number, self.service_value(resource)?))
                };
                compare_table_values(port_key(left), port_key(right), descending, Ord::cmp)
            }
            Self::ClusterIp | Self::ExternalIp | Self::Selector =>
                compare_table_values(self.service_value(left), self.service_value(right), descending, Ord::cmp),
            _ => compare_table_values(self.text_value(left), self.text_value(right), descending, Ord::cmp),
        }
    }
}

fn table_widths_dir() -> Option<PathBuf> {
    let home_dir = || std::env::var_os("HOME").filter(|path| !path.is_empty()).map(PathBuf::from);
    let base_dir = if cfg!(target_os = "macos") {
        home_dir().map(|dir| dir.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").filter(|path| !path.is_empty()).map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()).map(PathBuf::from)
            .or_else(|| home_dir().map(|dir| dir.join(".config")))
    };
    base_dir.map(|dir| dir.join("Cluster Observatory").join("column-widths"))
}

fn table_width_file(dir: &std::path::Path, kind: &str, column: ResourceColumn) -> PathBuf {
    dir.join(format!("{kind}.{}", column.title()))
}

fn read_table_widths(dir: &std::path::Path) -> HashMap<(&'static str, ResourceColumn), f32> {
    let mut widths = HashMap::new();
    if !dir.is_dir() { return widths; }
    for entry in CATALOG {
        for column in ResourceColumn::for_table(entry, true) {
            let read = |column| std::fs::read_to_string(table_width_file(dir, entry.kind, column)).ok()
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|value| value.is_finite() && *value > 0.0);
            let width = read(column).or_else(|| match column {
                ResourceColumn::Containers | ResourceColumn::Pods => read(ResourceColumn::Ready),
                _ => None,
            });
            if let Some(width) = width {
                widths.insert((entry.kind, column), column.clamp_width(width));
            }
        }
    }
    widths
}

fn write_table_width(dir: &std::path::Path, kind: &str, column: ResourceColumn, width: f32) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Replace only the changed column so other open windows can save different columns independently.
    let path = table_width_file(dir, kind, column);
    let temporary = dir.join(format!("{kind}.{}.{}.tmp", column.title(), std::process::id()));
    std::fs::write(&temporary, width.to_string())?;
    std::fs::rename(temporary, path)
}

fn compare_table_values<T>(
    left: Option<T>,
    right: Option<T>,
    descending: bool,
    compare: impl FnOnce(&T, &T) -> std::cmp::Ordering,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (Some(left), Some(right)) if descending => compare(&right, &left),
        (Some(left), Some(right)) => compare(&left, &right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn resource_cells(row: Rect, columns: &[(ResourceColumn, f32)]) -> impl Iterator<Item = (ResourceColumn, Rect)> + '_ {
    let mut x = row.left();
    columns.iter().copied().map(move |(column, width)| {
        let cell = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(width, row.height()));
        x += width;
        (column, cell)
    })
}

fn draw_resource_cell(painter: &egui::Painter, cell: Rect, label: String, column: ResourceColumn, header: bool, accent: Option<Color32>) {
    let status = !header && matches!(column, ResourceColumn::Status);
    let name = !header && matches!(column, ResourceColumn::Name);
    let numeric = matches!(column, ResourceColumn::Ready | ResourceColumn::Cpu | ResourceColumn::Memory
        | ResourceColumn::Containers | ResourceColumn::Restarts | ResourceColumn::Age | ResourceColumn::Pods | ResourceColumn::Replicas | ResourceColumn::Count);
    let color = if header { MUTED } else if status { status_color(&label) } else { accent.unwrap_or(TEXT) };
    let font = if header { FontId::proportional(11.0) }
        else if name { FontId::proportional(14.0) }
        else if numeric { FontId::monospace(12.0) }
        else { FontId::proportional(12.0) };

    let mut job = egui::text::LayoutJob::simple_singleline(label, font, color);
    if column == ResourceColumn::InvolvedObject && accent.is_some() {
        job.sections[0].format.underline = Stroke::new(1.0_f32, color);
    }
    let padding = if status { 20.0 } else { 12.0 };
    job.wrap.max_width = (cell.width() - 2.0 * padding).max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    let x = if numeric { cell.right() - padding - galley.size().x } else { cell.left() + padding };
    let position = Pos2::new(x, cell.center().y - galley.size().y / 2.0);
    let painter = painter.with_clip_rect(cell);
    if status {
        let pill = Rect::from_min_size(position, galley.size()).expand2(Vec2::new(8.0, 3.0));
        painter.rect_filled(pill, 5.0, color.gamma_multiply(0.15));
    }
    painter.galley(position, galley, color);
}

struct ResourceType {
    page: &'static str,
    group: &'static str,
    kind: &'static str,
    api: &'static str,
    namespaced: bool,
}

const CATALOG: &[ResourceType] = &[
    ResourceType { page: "Nodes", group: "", kind: "Node", api: "nodes", namespaced: false },
    ResourceType { page: "Namespaces", group: "", kind: "Namespace", api: "namespaces", namespaced: false },
    ResourceType { page: "Events", group: "", kind: "Event", api: "events", namespaced: true },
    ResourceType { page: "Pods", group: "Workloads", kind: "Pod", api: "pods", namespaced: true },
    ResourceType { page: "Deployments", group: "Workloads", kind: "Deployment", api: "deployments.apps", namespaced: true },
    ResourceType { page: "StatefulSets", group: "Workloads", kind: "StatefulSet", api: "statefulsets.apps", namespaced: true },
    ResourceType { page: "DaemonSets", group: "Workloads", kind: "DaemonSet", api: "daemonsets.apps", namespaced: true },
    ResourceType { page: "ReplicaSets", group: "Workloads", kind: "ReplicaSet", api: "replicasets.apps", namespaced: true },
    ResourceType { page: "Jobs", group: "Workloads", kind: "Job", api: "jobs.batch", namespaced: true },
    ResourceType { page: "CronJobs", group: "Workloads", kind: "CronJob", api: "cronjobs.batch", namespaced: true },
    ResourceType { page: "Custom Resource Definitions", group: "Config", kind: "CustomResourceDefinition", api: "customresourcedefinitions.apiextensions.k8s.io", namespaced: false },
    ResourceType { page: "ConfigMaps", group: "Config", kind: "ConfigMap", api: "configmaps", namespaced: true },
    ResourceType { page: "Secrets", group: "Config", kind: "Secret", api: "secrets", namespaced: true },
    ResourceType { page: "Autoscalers", group: "Config", kind: "HorizontalPodAutoscaler", api: "horizontalpodautoscalers.autoscaling", namespaced: true },
    ResourceType { page: "Services", group: "Network", kind: "Service", api: "services", namespaced: true },
    ResourceType { page: "Ingresses", group: "Network", kind: "Ingress", api: "ingresses.networking.k8s.io", namespaced: true },
    ResourceType { page: "Network Policies", group: "Network", kind: "NetworkPolicy", api: "networkpolicies.networking.k8s.io", namespaced: true },
    ResourceType { page: "Volume Claims", group: "Storage", kind: "PersistentVolumeClaim", api: "persistentvolumeclaims", namespaced: true },
    ResourceType { page: "Persistent Volumes", group: "Storage", kind: "PersistentVolume", api: "persistentvolumes", namespaced: false },
    ResourceType { page: "Storage Classes", group: "Storage", kind: "StorageClass", api: "storageclasses.storage.k8s.io", namespaced: false },
    ResourceType { page: "Service Accounts", group: "Access Control", kind: "ServiceAccount", api: "serviceaccounts", namespaced: true },
    ResourceType { page: "Roles", group: "Access Control", kind: "Role", api: "roles.rbac.authorization.k8s.io", namespaced: true },
    ResourceType { page: "Role Bindings", group: "Access Control", kind: "RoleBinding", api: "rolebindings.rbac.authorization.k8s.io", namespaced: true },
    ResourceType { page: "Cluster Roles", group: "Access Control", kind: "ClusterRole", api: "clusterroles.rbac.authorization.k8s.io", namespaced: false },
    ResourceType { page: "Cluster Role Bindings", group: "Access Control", kind: "ClusterRoleBinding", api: "clusterrolebindings.rbac.authorization.k8s.io", namespaced: false },
];

const DETAIL_SOURCES: &[ResourceType] = &[
    ResourceType { page: "Endpoint Slices", group: "", kind: "EndpointSlice", api: "endpointslices.discovery.k8s.io", namespaced: true },
    ResourceType { page: "Endpoints", group: "", kind: "Endpoints", api: "endpoints", namespaced: true },
];

fn cluster_label(context: &str) -> &str {
    if context.starts_with("arn:") {
        if let Some((_, name)) = context.split_once(":cluster/") {
            return name;
        }
    }
    context
}

fn words(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn fetch(target: &Target, cancelled: &AtomicBool) -> Snapshot {
    let mut snapshot = Snapshot { stamp: now(), ..Default::default() };
    let namespace = std::sync::OnceLock::new();
    let entries: Vec<_> = CATALOG.iter().chain(DETAIL_SOURCES).collect();
    // Keep the secret metadata read separate. Group the other lists so kubectl can
    // reuse its connection and exec credential across multiple API requests.
    let (cluster, namespaced): (Vec<_>, Vec<_>) = entries.iter().copied()
        .filter(|entry| entry.kind != "Secret")
        .partition(|entry| !entry.namespaced);
    let secrets: Vec<_> = entries.iter().copied().filter(|entry| entry.kind == "Secret").collect();
    let metrics = std::thread::scope(|scope| {
        let metrics = scope.spawn(|| fetch_metrics(target, cancelled));
        let workers: Vec<_> = [cluster, namespaced, secrets].into_iter().map(|batch| {
            let namespace = &namespace;
            scope.spawn(move || {
                if cancelled.load(Ordering::Relaxed) { return Vec::new(); }
                if batch.len() > 1 {
                    if let Ok(results) = list_resource_batch(target, &batch) {
                        return results;
                    }
                }
                batch.into_iter().take_while(|_| !cancelled.load(Ordering::Relaxed))
                    .map(|entry| (entry, list_resources(target, entry, namespace))).collect()
            })
        }).collect();
        for worker in workers {
            for (entry, result) in worker.join().expect("resource worker") {
                if DETAIL_SOURCES.iter().any(|source| source.kind == entry.kind) {
                    match result {
                        Ok((resources, warning)) => {
                            snapshot.related.extend(resources);
                            snapshot.detail_errors.extend(warning.map(|warning| (entry.kind, warning)));
                        }
                        Err(error) => { snapshot.detail_errors.insert(entry.kind, error); }
                    }
                    continue;
                }
                match result {
                    Ok((resources, warning)) => {
                        snapshot.loaded += 1;
                        snapshot.resources.extend(resources);
                        snapshot.errors.extend(warning);
                    }
                    Err(error) => snapshot.errors.push(format!("{}: {error}", entry.page)),
                }
            }
        }
        metrics.join().expect("metrics worker")
    });

    for (kind, result) in metrics {
        match result {
            Ok(items) => {
                let metrics: HashMap<_, _> = items.into_iter().map(|item| {
                    ((text(&item, "/metadata/namespace"), text(&item, "/metadata/name")), item)
                }).collect();
                for resource in snapshot.resources.iter_mut().filter(|resource| resource.kind == kind) {
                    if let Some(item) = metrics.get(&(resource.namespace.clone(), resource.name.clone())) {
                        let usages = if kind == "Node" { vec![&item["usage"]] } else {
                            item["containers"].as_array().into_iter().flatten().map(|container| &container["usage"]).collect()
                        };
                        for (key, is_memory, destination) in [
                            ("/cpu", false, &mut resource.cpu),
                            ("/memory", true, &mut resource.memory),
                        ] {
                            if !usages.is_empty() {
                                *destination = usages.iter().map(|usage| {
                                    Some(quantity(usage.pointer(key)?.as_str()?, is_memory))
                                }).sum::<Option<f64>>();
                            }
                        }
                    }
                }
            }
            Err(error) => {
                if snapshot.resources.iter().any(|resource| resource.kind == kind) {
                    snapshot.errors.push(format!("{kind} metrics unavailable: {error}"));
                }
            }
        }
    }
    snapshot.resources.sort_by(|a, b| (&a.kind, &a.namespace, &a.name).cmp(&(&b.kind, &b.namespace, &b.name)));
    snapshot
}

type ResourceList = Result<(Vec<Resource>, Option<String>), String>;

fn list_resource_batch<'a>(target: &Target, batch: &[&'a ResourceType]) -> Result<Vec<(&'a ResourceType, ResourceList)>, String> {
    let names = batch.iter().map(|entry| entry.api).collect::<Vec<_>>().join(",");
    let mut args = words(&["get", &names, "-o", "json"]);
    if batch[0].namespaced { args.push("-A".into()); }
    let items = target.run(&args, None).and_then(|output| parse_items(&output))?;
    let mut by_kind: HashMap<_, Vec<Resource>> = batch.iter().map(|entry| (entry.kind, Vec::new())).collect();
    for item in items {
        let kind = item["kind"].as_str().ok_or("Missing kind in combined resource list")?;
        by_kind.get_mut(kind).ok_or("Unexpected kind in combined resource list")?
            .push(Resource::parse(item));
    }
    Ok(batch.iter().map(|entry| (*entry, Ok((by_kind.remove(entry.kind).unwrap_or_default(), None)))).collect())
}

fn fetch_metrics(target: &Target, cancelled: &AtomicBool) -> Vec<(&'static str, Result<Vec<Value>, String>)> {
    if cancelled.load(Ordering::Relaxed) { return Vec::new(); }
    let combined = target.run(&words(&["get", "nodes.metrics.k8s.io,pods.metrics.k8s.io", "-A", "-o", "json"]), None)
        .and_then(|output| parse_items(&output));
    if let Ok(items) = combined {
        if items.iter().all(|item| matches!(item["kind"].as_str(), Some("NodeMetrics" | "PodMetrics"))) {
            let (nodes, pods) = items.into_iter().partition(|item| item["kind"] == "NodeMetrics");
            return vec![("Node", Ok(nodes)), ("Pod", Ok(pods))];
        }
    }
    // Some clusters authorize node and pod metrics differently.
    [("Node", "nodes"), ("Pod", "pods")].into_iter()
        .take_while(|_| !cancelled.load(Ordering::Relaxed))
        .map(|(kind, path)| {
            let endpoint = format!("/apis/metrics.k8s.io/v1beta1/{path}");
            (kind, target.run(&words(&["get", "--raw", &endpoint]), None).and_then(|output| parse_items(&output)))
        }).collect()
}

fn list_resources(target: &Target, entry: &ResourceType, namespace: &std::sync::OnceLock<String>) -> ResourceList {
    let format = if entry.kind == "Secret" {
        "custom-columns=NAME:.metadata.name,NAMESPACE:.metadata.namespace,UID:.metadata.uid"
    } else { "json" };
    let mut base = words(&["get", entry.api, "-o", format]);
    if entry.kind == "Secret" { base.push("--no-headers".into()); }
    let mut args = base.clone();
    if entry.namespaced { args.push("-A".into()); }
    let (output, warning) = match target.run(&args, None) {
        Ok(output) => (output, None),
        Err(error) if entry.namespaced && error.contains("(Forbidden)") => {
            let namespace = namespace.get_or_init(|| {
                target.run(&words(&["config", "view", "--minify", "-o", "jsonpath={.contexts[0].context.namespace}"]), None)
                    .ok().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| "default".into())
            });
            let mut args = base;
            args.extend(words(&["-n", namespace]));
            let warning = format!("{}: permissions limit this view to namespace {namespace}", entry.page);
            (target.run(&args, None)?, Some(warning))
        }
        Err(error) => return Err(error),
    };
    let items = if entry.kind == "Secret" {
        output.lines().filter(|line| !line.trim().is_empty()).map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if let [name, namespace, uid] = fields.as_slice() {
                Ok(json!({"apiVersion":"v1", "metadata":{"name": name, "namespace": namespace, "uid": uid}}))
            } else {
                Err("Unexpected Secret metadata output".to_owned())
            }
        }).collect::<Result<Vec<_>, _>>()?
    } else { parse_items(&output)? };
    let resources = items.into_iter().map(|mut item| {
        // kubectl may omit the GVK of items from a homogeneous list.
        item["kind"] = json!(entry.kind);
        Resource::parse(item)
    }).collect();
    Ok((resources, warning))
}

fn parse_items(output: &str) -> Result<Vec<Value>, String> {
    let mut value: Value = serde_json::from_str(output).map_err(|error| error.to_string())?;
    match value.get_mut("items").map(Value::take) {
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => Ok(items),
        _ => Err("Expected a Kubernetes resource list".into()),
    }
}

fn resource_args(verb: &str, resource: &Resource, extra: Vec<String>) -> Vec<String> {
    let api = CATALOG.iter().find(|entry| entry.kind == resource.kind).expect("resource type").api;
    let mut args = words(&[verb, api, &resource.name]);
    if !resource.namespace.is_empty() { args.extend(words(&["-n", &resource.namespace])); }
    args.extend(extra);
    args
}

fn pod_args(verb: &str, resource: &Resource, container: &str, extra: &[&str]) -> Vec<String> {
    let mut args = words(&[verb, &resource.name, "-n", &resource.namespace, "-c", container]);
    args.extend(words(extra));
    args
}

fn container_picker(ui: &mut egui::Ui, pod: &Resource, container: &mut String) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Container");
        egui::ComboBox::from_id_salt("container-picker").selected_text(container.as_str()).show_ui(ui, |ui| {
            for name in containers(pod) {
                changed |= ui.selectable_value(container, name.clone(), name).changed();
            }
        });
    });
    changed
}

fn containers(resource: &Resource) -> Vec<String> {
    ["/spec/containers", "/spec/initContainers", "/spec/ephemeralContainers"]
        .into_iter().flat_map(|path| array(&resource.raw, path))
        .map(|value| text(&value, "/name")).filter(|name| !name.is_empty()).collect()
}

fn suggested_port(resource: &Resource) -> Option<u16> {
    let port = if resource.kind == "Service" {
        num(&resource.raw, "/spec/ports/0/port")
    } else {
        num(&resource.raw, "/spec/containers/0/ports/0/containerPort")
    };
    u16::try_from(port).ok().filter(|port| *port > 0)
}

fn event_object(event: &Resource) -> &Value {
    event.raw.get("regarding").filter(|value| value.is_object()).unwrap_or(&event.raw["involvedObject"])
}

fn event_object_label(event: &Resource) -> String {
    let object = event_object(event);
    let kind = object["kind"].as_str().unwrap_or_default();
    let name = object["name"].as_str().unwrap_or_default();
    match (kind, name) {
        ("", "") => "—".into(),
        (kind, "") => kind.into(),
        ("", name) => name.into(),
        (kind, name) => format!("{kind}: {name}"),
    }
}

fn event_message(event: &Resource) -> &str {
    event_field(event, &["/message", "/note", "/reason"]).unwrap_or("—")
}

fn event_source(event: &Resource) -> &str {
    event_field(event, &["/source/component", "/deprecatedSource/component", "/reportingController", "/reportingComponent"])
        .unwrap_or("—")
}

fn event_count(event: &Resource) -> u64 {
    ["/series/count", "/count", "/deprecatedCount"].iter()
        .find_map(|path| event.raw.pointer(path)?.as_u64().filter(|count| *count > 0))
        .unwrap_or(1)
}

fn event_field<'a>(event: &'a Resource, paths: &[&str]) -> Option<&'a str> {
    paths.iter().find_map(|path| event.raw.pointer(path)?.as_str().filter(|value| !value.is_empty()))
}

fn event_time(event: &Resource, paths: &[&str]) -> Option<OffsetDateTime> {
    paths.iter().find_map(|path| {
        OffsetDateTime::parse(event.raw.pointer(path)?.as_str()?, &Rfc3339).ok()
    })
}

fn resource_age_start(resource: &Resource) -> Option<OffsetDateTime> {
    if resource.kind == "Event" {
        event_time(resource, &["/eventTime", "/firstTimestamp", "/deprecatedFirstTimestamp"]).or(resource.created)
    } else {
        resource.created
    }
}

fn event_last_seen(event: &Resource) -> Option<OffsetDateTime> {
    event_time(event, &["/series/lastObservedTime", "/lastTimestamp", "/deprecatedLastTimestamp"])
        .or_else(|| resource_age_start(event))
}

fn relative_age(stamp: Option<OffsetDateTime>) -> String {
    stamp.map_or_else(|| "—".into(), |stamp| {
        let seconds = (OffsetDateTime::now_utc() - stamp).whole_seconds().max(0);
        match seconds {
            0..=59 => format!("{seconds}s"),
            60..=3599 => format!("{}m{}s", seconds / 60, seconds % 60),
            3600..=86399 => format!("{}h{}m", seconds / 3600, seconds / 60 % 60),
            _ => format!("{}d{}h", seconds / 86400, seconds / 3600 % 24),
        }
    })
}

fn event_timestamp(value: Option<OffsetDateTime>) -> String {
    value.map_or_else(|| "—".into(), |stamp| {
        let utc = stamp.to_offset(time::UtcOffset::UTC);
        format!("{} {:02}:{:02}:{:02}Z", utc.date(), utc.hour(), utc.minute(), utc.second())
    })
}

fn event_for(event: &Resource, resource: &Resource) -> bool {
    let object = event_object(event);
    if let Some(uid) = object["uid"].as_str().filter(|uid| !uid.is_empty()) {
        return resource.raw["metadata"]["uid"] == uid;
    }
    let scope = if resource.namespace.is_empty() { "" } else { &event.namespace };
    object["namespace"].as_str().unwrap_or(scope) == resource.namespace
        && object["name"] == resource.name && object["kind"] == resource.kind
}

fn same_object(a: &Value, b: &Value) -> bool {
    ["/kind", "/metadata/uid", "/metadata/name", "/metadata/namespace"].into_iter()
        .all(|field| a.pointer(field) == b.pointer(field))
}

fn parse_manifest(output: String) -> Result<Manifest, String> {
    let original: Value = serde_json::from_str(&output).map_err(|error| error.to_string())?;
    let yaml = serde_yaml_ng::to_string(&original).map_err(|error| error.to_string())?;
    Ok(Manifest { yaml, original: Some(original), feedback: String::new() })
}

fn manifest_request(manifest: &Manifest, namespace: &str) -> Result<(Vec<String>, String), String> {
    if let Some(original) = &manifest.original {
        let edited: Value = serde_yaml_ng::from_str(&manifest.yaml).map_err(|error| error.to_string())?;
        let version = text(original, "/metadata/resourceVersion");
        if !same_object(original, &edited) || original["apiVersion"] != edited["apiVersion"] {
            return Err("Keep the resource's API version, kind, name, namespace and UID.".into());
        }
        if version.is_empty() || edited.pointer("/metadata/resourceVersion").and_then(Value::as_str) != Some(&version) {
            return Err("Keep metadata.resourceVersion so Kubernetes can detect conflicting changes.".into());
        }
        let mut args = words(&["replace", "-f", "-"]);
        let namespace = text(original, "/metadata/namespace");
        if !namespace.is_empty() { args.extend(words(&["-n", &namespace])); }
        Ok((args, edited.to_string()))
    } else {
        let mut args = words(&["apply", "-f", "-"]);
        if !namespace.is_empty() && namespace != ALL_NAMESPACES { args.extend(words(&["-n", namespace])); }
        Ok((args, manifest.yaml.clone()))
    }
}
// A full-width navigator row keeps text, icons and counts in fixed columns.
fn nav_row(ui:&mut egui::Ui,label:&str,icon:&str,depth:usize,selected:bool,count:Option<usize>,branch:Option<bool>)->egui::Response{
    let(rect,response)=ui.allocate_exact_size(Vec2::new(ui.available_width(),32.0),Sense::click());
    let painter=ui.painter();
    if selected||response.hovered()||response.has_focus(){
        painter.rect_filled(rect,0.0,if selected{Color32::from_rgb(55,56,62)}else{Color32::from_rgb(58,62,68)});
    }
    if selected{painter.rect_filled(Rect::from_min_size(rect.min,Vec2::new(2.0,rect.height())),0.0,BLUE);}
    let color=if selected{Color32::WHITE}else{MUTED};
    let left=rect.left()+8.0+depth as f32*16.0;
    if let Some(open)=branch{
        let center=Pos2::new(left+5.0,rect.center().y);
        let points=if open{vec![center+Vec2::new(-3.0,-2.0),center+Vec2::new(0.0,1.0),center+Vec2::new(3.0,-2.0)]}
            else{vec![center+Vec2::new(-1.0,-3.0),center+Vec2::new(2.0,0.0),center+Vec2::new(-1.0,3.0)]};
        painter.add(egui::Shape::line(points,Stroke::new(1.3_f32,color)));
    }
    nav_icon(painter,Pos2::new(left+23.0,rect.center().y),icon,color);
    let text_rect=Rect::from_min_max(Pos2::new(left+38.0,rect.top()),Pos2::new(rect.right()-if count.is_some(){38.0}else{12.0},rect.bottom()));
    painter.with_clip_rect(text_rect).text(text_rect.left_center(),Align2::LEFT_CENTER,label,FontId::proportional(13.0),color);
    if let Some(count)=count{
        painter.text(Pos2::new(rect.right()-14.0,rect.center().y),Align2::RIGHT_CENTER,count.to_string(),FontId::proportional(11.0),MUTED);
    }
    response.widget_info(||egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel,true,selected,label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
fn nav_icon(p:&egui::Painter,c:Pos2,kind:&str,color:Color32){
    let stroke=Stroke::new(1.25_f32,color);
    let line=|a:Vec2,b:Vec2|{p.line_segment([c+a,c+b],stroke);};
    let box_at=|x:f32,y:f32,w:f32,h:f32|{p.rect_stroke(Rect::from_min_size(c+Vec2::new(x,y),Vec2::new(w,h)),1.0,stroke,egui::StrokeKind::Inside);};
    match kind{
        "overview"=>{for x in [-6.0,1.0]{for y in [-6.0,1.0]{box_at(x,y,5.0,5.0);}}}
        "nodes"=>{for y in [-6.0,1.0]{box_at(-7.0,y,14.0,5.0);p.circle_filled(c+Vec2::new(4.0,y+2.5),0.8,color);}}
        "workloads"|"pod"|"cluster"=>{
            let points:Vec<_>=(0..6).map(|i|{let a=std::f32::consts::TAU*i as f32/6.0;c+Vec2::new(a.sin()*7.0,a.cos()*7.0)}).collect();
            p.add(egui::Shape::closed_line(points,stroke));
            if kind!="pod"{for i in 0..3{let a=std::f32::consts::TAU*i as f32/3.0;line(Vec2::ZERO,Vec2::new(a.sin()*7.0,a.cos()*7.0));}}
        }
        "config"=>{for(y,x)in [(-4.0,-2.0),(1.0,3.0),(6.0,-3.0)]{line(Vec2::new(-7.0,y),Vec2::new(7.0,y));p.circle_filled(c+Vec2::new(x,y),2.0,SIDE);p.circle_stroke(c+Vec2::new(x,y),2.0,stroke);}}
        "network"|"topology"=>{box_at(-2.5,-7.0,5.0,5.0);line(Vec2::new(0.0,-2.0),Vec2::new(0.0,2.0));line(Vec2::new(-5.0,2.0),Vec2::new(5.0,2.0));for x in [-5.0,5.0]{box_at(x-2.5,3.0,5.0,5.0);}}
        "storage"=>{box_at(-7.0,-6.0,14.0,12.0);for y in [-2.0,2.0]{line(Vec2::new(-7.0,y),Vec2::new(7.0,y));}}
        "namespace"=>{box_at(-6.0,-6.0,12.0,12.0);line(Vec2::new(-2.0,0.0),Vec2::new(2.0,0.0));line(Vec2::new(0.0,-2.0),Vec2::new(0.0,2.0));}
        "events"=>{p.circle_stroke(c,7.0,stroke);line(Vec2::new(0.0,-4.0),Vec2::new(0.0,0.0));line(Vec2::ZERO,Vec2::new(3.0,2.0));}
        "terminal"=>{line(Vec2::new(-6.0,-4.0),Vec2::new(-2.0,0.0));line(Vec2::new(-2.0,0.0),Vec2::new(-6.0,4.0));line(Vec2::new(0.0,5.0),Vec2::new(6.0,5.0));}
        _=>{}
    }
}
fn card<R>(ui:&mut egui::Ui,body:impl FnOnce(&mut egui::Ui)->R)->R{egui::Frame::new().fill(PANEL).corner_radius(7).inner_margin(16).show(ui,|ui|{ui.set_min_width(ui.available_width());body(ui)}).inner}
fn badge(ui:&mut egui::Ui,label:&str,color:Color32){egui::Frame::new().fill(color.gamma_multiply(0.12)).corner_radius(4).inner_margin(egui::Margin::symmetric(7,3)).show(ui,|ui|{ui.label(RichText::new(label).color(color).size(10.0).strong());});}
fn legend(ui:&mut egui::Ui,color:Color32,label:&str){ui.colored_label(color,"●");ui.label(RichText::new(label).color(MUTED).size(12.0));}
impl eframe::App for Observatory {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_pending_table_width();
    }

    fn update(&mut self,ctx:&egui::Context,_frame:&mut eframe::Frame){self.poll();
        #[cfg(target_os = "macos")]
        self.updates.poll(ctx);
        if self.shot.is_none()&&!self.demo&&!self.busy&&self.confirm.is_none()&&self.last_refresh.elapsed()>=Duration::from_secs(REFRESH){self.start_fetch();}
        self.top(ctx);self.sidebar(ctx);
        #[cfg(target_os = "macos")]
        self.process_usage.poll(ctx);
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(8))
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    #[cfg(target_os = "macos")]
                    self.updates.footer(ui);
                    #[cfg(target_os = "macos")]
                    ui.label(RichText::new(self.process_usage.label()).monospace().size(11.0).color(MUTED))
                        .on_hover_text("This app process · CPU is a recent average, with 100% equal to one core. RAM is resident memory.");
                    ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                        ui.colored_label(if self.demo { ORANGE } else { GREEN }, "●");
                        let workspace = if self.demo { "Sample workspace · illustrative metrics" }
                            else if self.snapshot.loaded > 0 { "Cluster workspace" }
                            else { "Local kubeconfigs" };
                        ui.label(RichText::new(workspace).size(11.0).color(MUTED));
                        if !self.notice.is_empty() {
                            ui.separator();
                            ui.add(egui::Label::new(RichText::new(&self.notice).size(11.0).color(MUTED)).truncate())
                                .on_hover_text(&self.notice);
                        }
                    });
                });
            });
        self.console(ctx);
        self.log_dock(ctx);
        self.details(ctx);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(24)).show(ctx,|ui|{
            if self.page == "Visualize" && !self.namespace.is_empty() {
                self.workspace(ui);
            } else {
                egui::ScrollArea::vertical().id_salt("main-scroll").show(ui, |ui| self.workspace(ui));
            }
        });
        self.connections(ctx);
        self.manifest_editor(ctx);
        #[cfg(target_os = "macos")]
        self.updates.show_history(ctx);
        if !ctx.input(|input| input.pointer.primary_down()) {
            self.save_pending_table_width();
        }
        if let Some(action) = self.confirm.clone() {
            egui::Modal::new(egui::Id::new("confirm-action")).show(ctx, |ui| {
                ui.heading("Confirm cluster action");
                ui.label(format!("Context: {}", action.target.context));
                ui.monospace(format!("kubectl {}", action.args.join(" ")));
                if let Some(manifest) = &self.manifest {
                    if matches!(action.output_to, OutputTo::ManifestSave) {
                        egui::ScrollArea::both().max_height(280.0).max_width(600.0).show(ui, |ui| {
                            ui.monospace(&manifest.yaml);
                        });
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.confirm = None; }
                    if ui.add_enabled(!self.busy && self.target.as_ref() == Some(&action.target), egui::Button::new("Run command")).clicked() {
                        self.confirm = None;
                        self.execute(action);
                    }
                });
            });
        }
        if !self.busy { self.frames+=1; }
        if let Some(path)=&self.shot{
            let events=ctx.input(|i|i.events.clone());for event in events{if let egui::Event::Screenshot{image,..}=event{
                if let Some(parent)=path.parent().filter(|p|!p.as_os_str().is_empty()){if let Err(e)=std::fs::create_dir_all(parent){eprintln!("Cannot create screenshot directory: {e}");std::process::exit(1);}}
                let bytes:Vec<u8>=image.pixels.iter().flat_map(|c|c.to_array()).collect();
                match image::save_buffer(path,&bytes,image.size[0] as u32,image.size[1] as u32,image::ColorType::Rgba8){Ok(())=>ctx.send_viewport_cmd(egui::ViewportCommand::Close),Err(e)=>{eprintln!("Screenshot failed: {e}");std::process::exit(1);}}
            }}
            if self.frames>=120&&!self.shot_requested&&!self.busy{self.shot_requested=true;ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));}
            if self.frames>600 || self.last_refresh.elapsed() > Duration::from_secs(60) {eprintln!("Timed out waiting for screenshot capture");std::process::exit(1);}
            ctx.request_repaint();
        } else {
            let quiet_overview = self.page == "Overview" && self.selected.is_none()
                && !self.connections && !self.console && self.pod_logs.is_none()
                && self.manifest.is_none() && self.confirm.is_none() && self.forwards.is_empty()
                && (!self.busy || self.fetch_cancel.is_some());
            let delay = if quiet_overview { Duration::from_secs(3) } else { Duration::from_millis(300) };
            let delay = if !self.busy && self.target.is_some() && self.confirm.is_none() {
                delay.min(Duration::from_secs(REFRESH).saturating_sub(self.last_refresh.elapsed()))
            } else {
                delay
            };
            ctx.request_repaint_after(delay);
        }
    }
}
fn default_kubectl() -> String {
    if std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("kubectl").is_file())) {
        return "kubectl".into();
    }
    ["/opt/homebrew/bin/kubectl", "/usr/local/bin/kubectl"]
        .into_iter().find(|path| std::path::Path::new(path).is_file()).unwrap_or("kubectl").into()
}

fn main()->eframe::Result {
    let mut arguments = std::env::args().skip(1);
    let mut shot = None;
    let default_exe = default_kubectl();
    let mut target = Target { exe: default_exe.clone(), ..Default::default() };
    while let Some(flag) = arguments.next() {
        if !matches!(flag.as_str(), "--screenshot" | "--kubeconfig" | "--context" | "--kubectl") {
            eprintln!("Unknown option {flag}");
            std::process::exit(2);
        }
        let value = arguments.next().filter(|arg| !arg.is_empty() && !arg.starts_with("--")).unwrap_or_else(|| {
            eprintln!("{flag} requires a value");
            std::process::exit(2);
        });
        match flag.as_str() {
            "--screenshot" => shot = Some(PathBuf::from(value)),
            "--kubeconfig" => target.config = value,
            "--context" => target.context = value,
            "--kubectl" => target.exe = value,
            _ => unreachable!(),
        }
    }
    if target.context.is_empty() && (!target.config.is_empty() || target.exe != default_exe) {
        eprintln!("--kubeconfig and --kubectl require an explicit --context");
        std::process::exit(2);
    }
    let target = (!target.context.is_empty()).then_some(target);
    let options=eframe::NativeOptions{viewport:egui::ViewportBuilder::default().with_title("Cluster Observatory").with_inner_size([1440.0,960.0]).with_min_inner_size([1080.0,720.0]),..Default::default()};
    eframe::run_native("Cluster Observatory",options,Box::new(move|cc|Ok(Box::new(Observatory::new(cc,shot,target)))))
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn kubernetes_quantities(){assert!((quantity("250m",false)-0.25).abs()<1e-9);assert!((quantity("500000000n",false)-0.5).abs()<1e-9);assert!((quantity("1048576Ki",true)-1.0).abs()<1e-9);}
    #[test]fn demo_resources_and_relationships(){let s=demo();assert_eq!(s.kind("Node").len(),3);assert_eq!(s.kind("Pod").len(),12);assert_eq!(s.kind("Deployment").len(),6);for p in s.kind("Pod"){assert!(s.kind("Node").iter().any(|n|n.name==p.node));}}

    #[test]
    fn deployment_pods_count_readiness_separately_from_availability() {
        let rolling_out = Resource::parse(json!({
            "kind": "Deployment",
            "spec": {"replicas": 3},
            "status": {"replicas": 4, "readyReplicas": 3, "availableReplicas": 2}
        }));
        assert_eq!(ResourceColumn::Pods.value(&rolling_out), "3/3");
        assert_eq!(ResourceColumn::Replicas.value(&rolling_out), "3");
        assert_eq!(ResourceColumn::Status.value(&rolling_out), "Unavailable");
    }

    #[test]
    fn age_sorts_by_timestamp_across_offsets_and_leaves_missing_last() {
        let pod = |stamp| Resource::parse(json!({"kind": "Pod", "metadata": {"creationTimestamp": stamp}}));
        let older = pod("2026-11-06T09:00:00+07:00");
        let newer = pod("2026-11-06T02:01:00Z");
        let missing = pod("");
        let age = ResourceColumn::Age;
        assert!(age.compare(&older, &newer, false).is_gt());
        assert!(age.compare(&older, &newer, true).is_lt());
        assert!(age.compare(&older, &missing, true).is_lt());
        assert_eq!(age.value(&missing), "—");
    }

    #[test]
    fn edit_keeps_identity_and_version() {
        let original = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "example", "namespace": "test", "uid": "uid-1", "resourceVersion": "42"},
            "data": {"setting": "before"},
        });
        let mut manifest = parse_manifest(original.to_string()).unwrap();
        let yaml = manifest.yaml.replace("before", "after");
        manifest.yaml = yaml.clone();
        let (args, input) = manifest_request(&manifest, "other").unwrap();
        assert_eq!(args, words(&["replace", "-f", "-", "-n", "test"]));
        assert_eq!(serde_json::from_str::<Value>(&input).unwrap()["data"]["setting"], "after");
        for altered in [
            yaml.replace("uid-1", "uid-2"),
            yaml.replace("42", "43"),
            yaml.replace("name: example", "name: another"),
            format!("{yaml}---\nkind: ConfigMap\n"),
        ] {
            manifest.yaml = altered;
            assert!(manifest_request(&manifest, "other").is_err());
        }
    }

}

#[cfg(test)]
#[test]
fn displays_colored_container_logs_without_ansi_codes() {
    let plain = clean_log_text("\x1b[33mWARN\x1b[0m[time] retry\n\x1b[36mqueue\x1b[0m=false");
    assert_eq!(plain, "WARN[time] retry\nqueue=false");
    assert_eq!(log_level(plain.lines().next().unwrap()).map(|(level, _)| level), Some("WARN"));
    assert_eq!(log_level("  [INFO] started").map(|(level, _)| level), Some("[INFO]"));
}

#[cfg(test)]
#[test]
fn dragging_a_resource_header_resizes_without_sorting() {
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = Observatory::new(&creation, Some(PathBuf::from("test.png")), None);
    app.page = "Deployments".into();
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(960.0, 540.0));
    let mut frame = |events| {
        ctx.run(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.resources(ui));
        })
    };
    let first = frame(Vec::new());
    let grip = first.shapes.iter().find_map(|shape| {
        if let egui::Shape::LineSegment { points: [top, bottom], stroke } = shape.shape {
            if top.x == bottom.x && stroke.color == MUTED.gamma_multiply(0.25) {
                return Some(top.lerp(bottom, 0.5) - Vec2::new(4.0, 0.0));
            }
        }
        None
    }).expect("Name column resize grip");
    let button = |pos, pressed| egui::Event::PointerButton {
        pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
    };
    frame(vec![egui::Event::PointerMoved(grip)]);
    frame(vec![button(grip, true)]);
    let end = grip + Vec2::new(80.0, 0.0);
    frame(vec![egui::Event::PointerMoved(end)]);
    let resized = frame(vec![button(end, false)]);
    assert!(resized.shapes.iter().any(|shape| {
        matches!(shape.shape, egui::Shape::LineSegment { points: [top, bottom], .. }
            if top.x == bottom.x && (top.x - grip.x - 84.0).abs() < 1.0)
    }));

    frame(vec![button(end, true)]);
    let narrowed = grip - Vec2::new(80.0, 0.0);
    frame(vec![egui::Event::PointerMoved(narrowed)]);
    let resized = frame(vec![button(narrowed, false)]);
    assert!(resized.shapes.iter().any(|shape| {
        matches!(shape.shape, egui::Shape::LineSegment { points: [top, bottom], .. }
            if top.x == bottom.x && (top.x - narrowed.x - 4.0).abs() < 1.0)
    }));
    let row_right = |output: &egui::FullOutput| {
        output.shapes.iter().find_map(|shape| {
            match shape.shape {
                egui::Shape::LineSegment { points: [left, right], stroke }
                    if left.y == right.y && stroke.color == BORDER => Some(right.x),
                _ => None,
            }
        }).expect("resource row divider")
    };
    assert!((row_right(&resized) - row_right(&first)).abs() < 1.0);
    assert!(app.table_widths.contains_key(&("Deployment", ResourceColumn::Name)));
    assert!(app.table_sort.is_empty());
}

#[cfg(test)]
#[test]
fn column_widths_restore_independently_for_each_table() {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("lenser-widths-{}-{timestamp}", std::process::id()));
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx);
    let mut first = Observatory::new(&creation, Some(PathBuf::from("test.png")), None);
    let mut second = Observatory::new(&creation, Some(PathBuf::from("test.png")), None);
    for (app, kind, width) in [(&mut first, "Pod", 215.0), (&mut second, "Deployment", 360.0)] {
        let key = (kind, ResourceColumn::Name);
        app.table_widths_dir = Some(dir.clone());
        app.table_widths.insert(key, width);
        app.pending_table_width = Some(key);
        app.save_pending_table_width();
    }
    let restored = read_table_widths(&dir);
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(restored.get(&("Pod", ResourceColumn::Name)), Some(&215.0));
    assert_eq!(restored.get(&("Deployment", ResourceColumn::Name)), Some(&360.0));
    assert_eq!(restored.len(), 2);
}
