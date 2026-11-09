use eframe::egui::{self, Align, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use serde_json::{json, Value};
use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::{mpsc, Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

mod cluster_overview;
mod kubernetes;
mod kubeconfig_settings;
mod workloads_overview;
mod detail_metrics;
mod env_secrets;
#[cfg(target_os = "macos")]
mod process_usage;
#[cfg(target_os = "macos")]
mod updates;
mod resource_details;
mod visualize;
mod theme;
mod ui_icons;
mod pod_logs;
mod demo_data;
use pod_logs::PodLogs;
use theme::{bg, panel, side, border, blue, green, orange, purple, muted, text as text_color};
use kubernetes::{LocalContexts, Stream, Target};

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

fn status_color(s: &str) -> Color32 { if ["Running", "Ready", "Active", "Available", "Bound", "Normal", "Complete", "Scheduled"].contains(&s) { green() } else if matches!(s, "Unavailable" | "Failed") { theme::danger() } else if ["Pending", "Warning", "CrashLoopBackOff", "NotReady"].contains(&s) { orange() } else { muted() } }

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
        else if kind == "Job" {
            let conditions = array(&v, "/status/conditions");
            let has_condition = |name: &str| conditions.iter().any(|condition| condition["type"] == name && condition["status"] == "True");
            status = if has_condition("Failed") { "Failed" } else if has_condition("Complete") { "Complete" }
                else if v["spec"]["suspend"] == true { "Suspended" }
                else if num(&v, "/status/active") > 0 { "Running" } else { "Pending" }.into();
        }
        else if kind == "CronJob" { status = if v["spec"]["suspend"] == true { "Suspended" } else { "Scheduled" }.into(); }
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
        resources.push(Resource::parse(json!({"kind":"Deployment","metadata":{"name":name,"namespace":ns},"spec":{"replicas":2},"status":{"replicas":2,"updatedReplicas":2,"readyReplicas":ready,"availableReplicas":ready,"unavailableReplicas":2-ready}})));
        resources.push(Resource::parse(json!({"kind":"Service","metadata":{"name":name,"namespace":ns},"spec":{"type":"ClusterIP","clusterIP":format!("10.96.0.{}",10+i),"ports":[{"port":80}],"selector":{"app":name}}})));
        resources.push(Resource::parse(json!({"kind":"ReplicaSet","metadata":{"name":format!("{name}-7d8f6c"),"namespace":ns,"ownerReferences":[{"kind":"Deployment","name":name,"controller":true}]},"spec":{"replicas":2,"selector":{"matchLabels":{"app":name}}},"status":{"replicas":2,"readyReplicas":ready}})));
        for j in 0..2 {
            let waiting=i==3&&j==1;
            let mut p=Resource::parse(json!({"kind":"Pod","metadata":{"name":format!("{name}-7d8f6c-{j}k4xz"),"namespace":ns,"creationTimestamp":"2026-11-09T03:30:00Z","labels":{"app":name},"ownerReferences":[{"kind":"ReplicaSet","name":format!("{name}-7d8f6c"),"controller":true}]},"spec":{"nodeName":format!("worker-0{}",(i+j)%3+1),"serviceAccountName":"observatory-reader","containers":[{"name":name,"image":format!("example/{name}:1.4.2"),"resources":{"requests":{"cpu":"100m","memory":"128Mi"},"limits":{"cpu":"500m","memory":"512Mi"}}}]},"status":{"podIP":format!("10.244.{}.{}",(i+j)%3+1,10+i*2+j),"qosClass":"Burstable","phase":if waiting{"Pending"}else{"Running"},"containerStatuses":[{"ready":!waiting,"restartCount":if waiting{3}else{0},"state":if waiting{json!({"waiting":{"reason":"CrashLoopBackOff"}})}else{json!({"running":{}})}}]}}));
            p.raw["apiVersion"] = json!("v1");
            if waiting {
                p.raw["spec"]["initContainers"] = json!([{"name":"wait-for-redis","image":"busybox:1.37","command":["sh","-c","until nslookup redis; do sleep 2; done"]}]);
                p.raw["status"]["initContainerStatuses"] = json!([{"name":"wait-for-redis","restartCount":0,"ready":true,"state":{"terminated":{"exitCode":0,"reason":"Completed"}}}]);
                p.raw["status"]["conditions"] = json!([
                    {"type":"Initialized","status":"True"},
                    {"type":"PodScheduled","status":"True"},
                    {"type":"ContainersReady","status":"False","reason":"ContainersNotReady"},
                    {"type":"Ready","status":"False","reason":"ContainersNotReady"}
                ]);
                p.raw["spec"]["volumes"] = json!([{"name":"configuration","configMap":{"name":"gateway-config"}}]);
                p.raw["spec"]["containers"][0]["volumeMounts"] = json!([{"name":"configuration","mountPath":"/etc/payments","readOnly":true}]);
                p.raw["spec"]["containers"][0]["env"] = json!([
                    {"name":"APP_ENV","value":"sample"},
                    {"name":"DATABASE_URL","valueFrom":{"secretKeyRef":{"name":"payments-env","key":"DATABASE_URL"}}}
                ]);
            }
            p.cpu=Some(0.08+i as f64*0.035);p.memory=Some(0.13+i as f64*0.04); resources.push(p);
        }
    }
    let usage: HashMap<String, (f64, f64)> = resources.iter().filter(|resource| resource.kind == "Pod")
        .filter_map(|pod| Some((text(&pod.raw, "/metadata/labels/app"), (pod.cpu?, pod.memory?))))
        .fold(HashMap::new(), |mut totals, (name, (cpu, memory))| {
            let value = totals.entry(name).or_default();
            value.0 += cpu;
            value.1 += memory;
            totals
        });
    for deployment in resources.iter_mut().filter(|resource| resource.kind == "Deployment") {
        if let Some(&(cpu, memory)) = usage.get(&deployment.name) {
            deployment.cpu = Some(cpu);
            deployment.memory = Some(memory);
        }
    }
    for ns in ["production","monitoring","kube-system"] {resources.push(Resource::parse(json!({"kind":"Namespace","metadata":{"name":ns},"status":{"phase":"Active"}})));}
    resources.push(Resource::parse(json!({"kind":"Event","metadata":{"name":"payments.backoff","namespace":"production"},"type":"Warning","reason":"BackOff","message":"Restart back-off for the payments container","involvedObject":{"kind":"Pod","name":"payments-7d8f6c-1k4xz"}})));
    resources.push(Resource::parse(json!({"kind":"Event","metadata":{"name":"orders.scaled","namespace":"production"},"type":"Normal","reason":"ScalingReplicaSet","message":"Scaled orders-api to 2 replicas","involvedObject":{"kind":"Deployment","name":"orders-api"}})));
    resources.push(Resource::parse(json!({"kind":"PersistentVolumeClaim","metadata":{"name":"redis-data","namespace":"production"},"spec":{"resources":{"requests":{"storage":"10Gi"}}},"status":{"phase":"Bound"}})));
    resources.push(Resource::parse(json!({"kind":"ConfigMap","metadata":{"name":"gateway-config","namespace":"production"},"data":{"LOG_LEVEL":"info"}})));
    resources.extend(demo_data::resources());
    Snapshot { resources, stamp: now(), loaded: 1, ..Default::default() }
}

#[derive(Clone, Copy)]
enum OutputTo {
    Console,
    Action,
    Eviction,
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
    target: ActionTarget,
    args: Vec<String>,
    input: Option<String>,
    output_to: OutputTo,
}

#[derive(Clone)]
enum ActionTarget {
    Cluster(Target),
    SamplePod(Resource),
}

struct Manifest {
    yaml: String,
    original: Option<Value>,
    feedback: String,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    Appearance,
    Kubernetes,
    Connections,
    #[cfg(target_os = "macos")]
    Updates,
}

struct Observatory {
    snapshot:Snapshot, demo:bool,page:String,namespace:String,search:String,
    cluster_overview: cluster_overview::Overview, selected:Option<Resource>, detail_tab:String,
    appearance: theme::Preference, appearance_dir: Option<PathBuf>, rendered_theme: Option<egui::Theme>,
    settings_page: Option<SettingsPage>, contexts:Vec<Target>,context:String,exe:String,kubeconfig:String,
    kubeconfig_settings: kubeconfig_settings::KubeconfigSettings,
    synced_contexts: Vec<Target>,
    tx:mpsc::Sender<Reply>,rx:mpsc::Receiver<Reply>,busy:bool,last_refresh:Instant,
    output:String,command:String,console:bool,notice:String,confirm:Option<Action>,replicas:u32,
    target: Option<Target>,
    fetch_cancel: Option<Arc<AtomicBool>>,
    repaint: egui::Context,
    visualize: visualize::Visualize,
    detail_metrics: detail_metrics::Metrics,
    env_secrets: env_secrets::EnvSecrets,
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
    hidden_columns: HashSet<(&'static str, ResourceColumn)>,
    table_sort: HashMap<&'static str, (ResourceColumn, bool)>,
    table_widths: HashMap<(&'static str, ResourceColumn), f32>,
    table_widths_dir: Option<PathBuf>,
    pending_table_width: Option<(&'static str, ResourceColumn)>,
}
impl Observatory {
    fn new(cc: &eframe::CreationContext<'_>, shot: Option<PathBuf>, target: Option<Target>, sample: bool) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let demo = sample || shot.is_some() && target.is_none();
        let snapshot = if demo { self::demo() } else { Snapshot::default() };
        let settings_dir = shot.is_none().then(app_config_dir).flatten();
        let appearance = settings_dir.as_deref().map(theme::Preference::load).unwrap_or_default();
        let table_widths_dir = settings_dir.as_ref().map(|dir| dir.join("column-widths"));
        let table_widths = table_widths_dir.as_deref().map(read_table_widths).unwrap_or_default();
        let kubeconfig_settings = kubeconfig_settings::KubeconfigSettings::load(settings_dir.clone());
        let mut app = Self{snapshot,demo,page:"Overview".into(),namespace:if demo { ALL_NAMESPACES.into() } else { String::new() },search:String::new(),cluster_overview: Default::default(),selected:None,detail_tab:"Details".into(),settings_page:None,contexts:vec![],context:String::new(),exe:default_kubectl(),kubeconfig:String::new(),tx,rx,busy:false,last_refresh:Instant::now(),output:String::new(),command:"get pods -A".into(),console:false,notice:String::new(),confirm:None,replicas:2,shot,frames:0,shot_requested:false,nav_open:HashMap::new(),
            target: None, fetch_cancel: None, repaint: cc.egui_ctx.clone(), visualize: Default::default(), detail_metrics: Default::default(), manifest: None, container: String::new(),
            #[cfg(target_os = "macos")]
            process_usage: Default::default(),
            #[cfg(target_os = "macos")]
            updates: updates::Updates::load(settings_dir.as_deref()),
            pod_logs: None, forwards: Vec::new(), local_port: 8080, remote_port: 80,
            hidden_columns: default_hidden_columns(), table_sort: HashMap::new(),
            table_widths, table_widths_dir, pending_table_width: None,
            kubeconfig_settings, synced_contexts: Vec::new(),
            appearance, appearance_dir: settings_dir, rendered_theme: None,
            env_secrets: Default::default(),
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
        #[cfg(target_os = "macos")]
        if app.shot.is_none() {
            app.updates.check_for_updates(&cc.egui_ctx);
        }
        app
    }
    fn prepare_screenshot(&mut self, view: &str) {
        let (page, kind) = match view {
            "pods" | "pod" | "pod-search" | "pod-events" | "pod-configuration" | "start-port-forward" | "logs" | "logs-expanded" | "command" => ("Pods", "Pod"),
            "ingress" => ("Ingresses", "Ingress"),
            "appearance" | "kubernetes" | "connections" | "updates" => {
                self.settings_page = Some(match view {
                    "kubernetes" => SettingsPage::Kubernetes,
                    "connections" => SettingsPage::Connections,
                    #[cfg(target_os = "macos")]
                    "updates" => SettingsPage::Updates,
                    _ => SettingsPage::Appearance,
                });
                return;
            }
            "topology" | "traffic" => {
                self.page = "Visualize".into();
                self.namespace = "production".into();
                if view == "traffic" { self.visualize.show_traffic(); }
                return;
            }
            "workloads" | "config" | "network" | "storage" | "access-control" | "port-forwarding" => {
                let (page, group) = match view {
                    "workloads" => ("Workloads Overview", "Workloads"),
                    "config" => ("Config", "Config"), "network" => ("Network", "Network"),
                    "storage" => ("Storage", "Storage"), "access-control" => ("Access Control", "Access Control"),
                    _ => ("Port Forwarding", "Network"),
                };
                self.page = page.into();
                self.nav_open.insert(group, true);
                return;
            }
            "overview" => return,
            view => {
                let requested = view.trim_end_matches("-details");
                let Some(entry) = CATALOG.iter().find(|entry| entry.page.to_ascii_lowercase().replace(' ', "-") == requested) else { return };
                (entry.page, entry.kind)
            }
        };
        self.page = page.into();
        if let Some(entry) = CATALOG.iter().find(|entry| entry.page == page) {
            self.nav_open.insert(entry.group, true);
        }
        if view == "command" { self.console = true; return; }
        if view == "pod-search" { self.search = "payments".into(); return; }
        if view == "pods" || (view != "deployments" && view != "secrets" && view != "ingress"
            && view != "start-port-forward" && !view.starts_with("pod") && !view.starts_with("logs") && !view.ends_with("-details")) { return; }
        let selected = self.snapshot.resources.iter().find(|resource| {
            resource.kind == kind && (kind != "Pod" || resource.status == "CrashLoopBackOff")
        }).cloned();
        if let Some(resource) = selected {
            if matches!(view, "logs" | "logs-expanded") {
                let container = containers(&resource).into_iter().next().unwrap_or_default();
                self.open_pod_logs(&resource, container);
                if let Some(logs) = &mut self.pod_logs { logs.expanded = view == "logs-expanded"; }
            } else {
                self.select(resource);
                if view == "pod-events" { self.detail_tab = "Events".into(); }
                if view == "pod-configuration" { self.detail_tab = "Configuration".into(); }
                if view == "start-port-forward" { self.detail_tab = "Port Forward".into(); }
            }
        }
    }

    fn discover(&mut self, connect_current: bool) {
        self.discover_source(self.kubeconfig.trim().to_owned(), connect_current);
    }

    fn discover_source(&mut self, config: String, connect_current: bool) {
        let target = Target { exe: self.exe.trim().into(), config, context: String::new() };
        let sources = self.kubeconfig_settings.sources();
        self.busy = true;
        self.notice = "Scanning local kubeconfigs…".into();
        let tx = self.tx.clone();
        let repaint = self.repaint.clone();
        std::thread::spawn(move || {
            let source = target.config.clone();
            let discovered = LocalContexts::discover(target, &sources);
            let _ = tx.send(Reply::Contexts { discovered, connect_current, source });
            repaint.request_repaint();
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
        let demo = target.is_none();
        self.switch_workspace(target, demo);
    }

    fn switch_workspace(&mut self, target: Option<Target>, sample: bool) {
        if (self.busy && self.fetch_cancel.is_none())
            || (self.target == target && self.demo == sample) { return; }
        if let Some(cancelled) = self.fetch_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
            self.busy = false;
        }
        if let Some(target) = &target {
            if !self.contexts.contains(target) { self.contexts.push(target.clone()); }
        }
        self.demo = sample;
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
        self.container = default_container(&resource);
        self.remote_port = suggested_port(&resource).unwrap_or(80);
        self.detail_tab = "Details".into();
        self.selected = Some(resource);
    }

    fn queue(&mut self, args: Vec<String>, input: Option<String>, output_to: OutputTo) {
        if let Some(target) = self.target.clone() {
            self.confirm = Some(Action { target: ActionTarget::Cluster(target), args, input, output_to });
        }
    }

    fn execute(&mut self, action: Action) {
        let target = match action.target {
            ActionTarget::Cluster(target) => target,
            ActionTarget::SamplePod(pod) => {
                self.snapshot.resources.retain(|resource| !same_object(&pod.raw, &resource.raw));
                if self.selected.as_ref().is_some_and(|resource| same_object(&pod.raw, &resource.raw)) {
                    self.selected = None;
                }
                if self.pod_logs.as_ref().is_some_and(|logs| same_object(&pod.raw, &logs.pod.raw)) {
                    self.pod_logs = None;
                }
                let verb = if matches!(action.output_to, OutputTo::Eviction) { "Evicted" } else { "Deleted" };
                self.notice = format!("{verb} {} from the sample workspace", pod.name);
                return;
            }
        };
        self.busy = true;
        if matches!(action.output_to, OutputTo::Console) { self.output = "Running…".into(); }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = target.run(&action.args, action.input);
            let _ = tx.send(Reply::Output(action.output_to, result));
        });
    }

    fn load_manifest(&mut self, resource: &Resource) {
        if self.demo && resource.kind == "Pod" {
            match manifest_from_resource(resource.raw.clone()) {
                Ok(manifest) => self.manifest = Some(manifest),
                Err(error) => self.notice = format!("Could not load YAML: {error}"),
            }
            return;
        }
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
                    let discovery_succeeded = error.is_none();
                    self.notice = error.unwrap_or_else(|| format!("{} local contexts found", entries.len()));
                    if let Some(current) = &current {
                        if self.context.is_empty() { self.context = current.context.clone(); }
                    }
                    if source.is_empty() {
                        let active_removed = discovery_succeeded && self.target.as_ref().is_some_and(|active| {
                            let same_source = |entry: &Target| entry.config == active.config && entry.context == active.context;
                            self.synced_contexts.iter().any(same_source) && !entries.iter().any(same_source)
                        });
                        self.synced_contexts.clone_from(&entries);
                        self.contexts = entries;
                        if active_removed {
                            self.switch_workspace(None, false);
                            self.context.clear();
                            self.kubeconfig.clear();
                            self.notice = "The active kubeconfig is no longer synced.".into();
                        }
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
                        OutputTo::Action | OutputTo::Eviction => { self.output = output; self.console = true; }
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
        !matches!(page, "Overview" | "Config" | "Network" | "Storage" | "Access Control")
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
    fn workspace_context(&self) -> &str {
        if self.demo { "example-production" } else { self.active_context() }
    }
    fn top(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header").exact_height(56.0)
            .frame(egui::Frame::new().fill(panel()).inner_margin(egui::Margin {
                left: 20, right: 24, top: 11, bottom: 11,
            }))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.horizontal(|ui| {
                    let (logo, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                    ui.painter().rect_filled(logo, 6.0, theme::sidebar());
                    nav_icon(ui.painter(), logo.center(), "cluster", Color32::WHITE);
                    ui.add_space(10.0);
                    ui.add_sized([148.0, 34.0], egui::Label::new(
                        RichText::new("Cluster Observatory").font(theme::semibold(15.0))).truncate());
                    ui.add_space(16.0);
                    let status = if self.demo { "Sample data" } else if self.snapshot.loaded > 0 { "Connected" }
                        else if self.busy { "Connecting" } else if self.target.is_none() { "Disconnected" } else { "Unavailable" };
                    let (tag, _) = ui.allocate_exact_size(Vec2::new(94.0, 22.0), Sense::hover());
                    ui.painter().rect_filled(tag, 4.0, theme::selection());
                    ui.painter().text(tag.center(), Align2::CENTER_CENTER, status, FontId::proportional(12.0), blue());

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if settings_button(ui, "Refresh", Some("refresh"), 92.0, !self.busy, false).clicked() {
                            if self.demo {
                                self.snapshot = demo();
                                if let Some(selected) = &self.selected {
                                    self.selected = self.snapshot.resources.iter()
                                        .find(|resource| same_object(&selected.raw, &resource.raw)).cloned();
                                    if let Some(resource) = &self.selected {
                                        if !containers(resource).contains(&self.container) {
                                            self.container = default_container(resource);
                                        }
                                    }
                                }
                                self.cluster_overview.record(&self.snapshot);
                                self.notice = "Sample refreshed".into();
                            } else if self.target.is_some() { self.start_fetch(); }
                            else { self.discover(true); }
                        }
                        ui.add_space(8.0);
                        let in_settings = self.settings_page.is_some();
                        if chrome_button(ui, "Settings", Some("config"), 98.0, true, false, in_settings).clicked() {
                            self.settings_page = Some(SettingsPage::Appearance);
                        }
                        if !in_settings {
                            ui.add_space(9.0);
                            let can_apply = self.target.is_some() && !self.namespace.is_empty() && !self.busy && self.manifest.is_none();
                            if settings_button(ui, "Apply YAML…", None, 117.0, can_apply, false).clicked() {
                                self.manifest = Some(Manifest { yaml: String::new(), original: None, feedback: String::new() });
                            }
                        }
                        if self.busy && (self.page != "Overview" || self.snapshot.loaded == 0 || self.fetch_cancel.is_none()) {
                            ui.add_space(8.0);
                            ui.spinner();
                        }
                    });
                });
                let header = ui.max_rect();
                let y = header.bottom() + 10.0;
                ui.painter().hline(0.0..=ctx.screen_rect().right(), y, Stroke::new(1.0_f32, border()));
            });
    }
    fn sidebar(&mut self,ctx:&egui::Context){
        egui::SidePanel::left("sidebar").exact_width(220.0).resizable(false).show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::sidebar()).inner_margin(0)).show(ctx,|ui|{
            ui.spacing_mut().item_spacing=Vec2::ZERO;
            ui.add_space(9.0);
            let footer_height = 122.0;
            egui::ScrollArea::vertical().id_salt("navigation-tree").auto_shrink([false, false])
                .max_height((ui.available_height() - footer_height).max(100.0))
                .show(ui, |ui| self.cluster_sidebar(ui));
            ui.with_layout(egui::Layout::bottom_up(Align::Min),|ui|{
                ui.add_space(14.0);
                if nav_row(ui,"Settings","config",0,self.settings_page==Some(SettingsPage::Appearance),None,None).clicked() {
                    self.settings_page=Some(SettingsPage::Appearance);
                }
                if nav_row(ui,"Cluster connections","plug",0,self.settings_page==Some(SettingsPage::Connections),None,None).clicked(){self.settings_page=Some(SettingsPage::Connections);}
                if nav_row(ui,"Command panel","terminal",0,self.console && self.settings_page.is_none(),None,None).clicked(){
                    self.console = self.settings_page.is_some() || !self.console;
                    self.settings_page = None;
                }
                ui.add_space(7.0);
                sidebar_divider(ui);
            });
        });
    }
    fn cluster_sidebar(&mut self, ui: &mut egui::Ui) {
        let root_open = self.nav_open.get("cluster").copied().unwrap_or(true);
        if self.demo {
            if nav_row(ui, "example-production", "cluster-root", 0, false, None, Some(root_open)).clicked() {
                self.nav_open.insert("cluster", !root_open);
            }
            ui.add_space(12.0);
            sidebar_divider(ui);
            ui.add_space(9.0);
            if root_open { self.resource_navigation(ui); }
        }
        if self.contexts.is_empty() && self.demo { return; }

        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("LOCAL KUBECONFIGS").size(11.0).strong().color(theme::sidebar_text()));
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
                nav_row(ui, cluster_label(&target.context), "cluster", 0, active && !expanded, None, Some(expanded))
                    .on_hover_text(tooltip).clicked()
            }).inner;
            if clicked {
                let from_settings = self.settings_page.take().is_some();
                if active {
                    self.nav_open.insert("cluster", from_settings || !expanded);
                } else {
                    next = Some(target.clone());
                }
            }
            if expanded { self.resource_navigation(ui); }
        }
        if self.contexts.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(if self.busy { "Scanning…" } else { "No local contexts found" }).color(theme::sidebar_text()));
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
        ui.add_space(8.0);
        for (group, icon) in [
            ("Workloads", "layers"), ("Config", "file"), ("Network", "network"),
            ("Storage", "storage"), ("Access Control", "lock"),
        ] {
            let open = self.nav_open.get(group).copied().unwrap_or(group == "Workloads" && (self.page == "Workloads Overview" || CATALOG.iter().any(|entry| entry.group == group && entry.page == self.page)));
            let landing = if group == "Workloads" { "Workloads Overview" } else { group };
            let mut expanded = open;
            let selected = self.settings_page.is_none() && self.page == landing;
            if nav_row(ui, group, icon, 1, selected && group != "Workloads", None, Some(open)).clicked() {
                expanded = !open || !selected;
                self.nav_open.insert(group, expanded);
                if expanded { self.open_page(landing); }
            }
            if expanded {
                if group == "Workloads" {
                    self.nav_page(ui, "Workloads Overview", "Overview", icon, 2, None);
                }
                for entry in CATALOG.iter().filter(|entry| entry.group == group) {
                    self.nav_page(ui, entry.page, entry.page, icon, 2, Some(entry.kind));
                }
                if group == "Network" {
                    self.nav_page(ui, "Port Forwarding", "Port Forwarding", icon, 2, None);
                }
            }
        }
        ui.add_space(8.0);
        self.nav_page(ui, "Namespaces", "Namespaces", "namespace", 1, Some("Namespace"));
        self.nav_page(ui, "Events", "Events", "events", 1, Some("Event"));
    }

    fn nav_page(&mut self,ui:&mut egui::Ui,page:&str,label:&str,icon:&str,depth:usize,kind:Option<&str>){
        let count = kind.filter(|_| !self.namespace.is_empty() || !Self::page_uses_namespace(page)).map(|kind| {
            self.snapshot.kind(kind).into_iter().filter(|resource| self.in_namespace(&resource.namespace)).count()
        });
        let response=nav_row(ui,label,icon,depth,self.settings_page.is_none() && self.page==page,count,None);
        if response.on_hover_text(label).clicked(){
            self.open_page(page);
        }
    }

    fn open_page(&mut self, page: &str) {
        self.settings_page = None;
        self.page = page.into();
        self.selected = None;
        self.search.clear();
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let uses_namespace = Self::page_uses_namespace(&self.page);
        ui.horizontal(|ui| {
            workspace_title(ui, match self.page.as_str() {
                "Overview" => "Cluster overview",
                "Visualize" => "Topology",
                "Workloads Overview" => "Workloads overview",
                "Port Forwarding" => "Port forwarding",
                page => page,
            });
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if !uses_namespace {
                    if !matches!(self.page.as_str(), "Config" | "Network" | "Storage" | "Access Control") {
                        settings_button(ui, "Cluster-wide", Some("workloads"), 174.0, false, false);
                    }
                    return;
                }
                let label = if self.namespace.is_empty() { "Select a namespace".into() } else { self.namespace.clone() };
                let key = self.target.as_ref().map(|target| (&target.context, &target.config));
                let menu = ui.push_id(("namespace", key), |ui| {
                    egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(Vec2::new(174.0, 34.0)), |ui| {
                        changed |= ui.selectable_value(&mut self.namespace, ALL_NAMESPACES.into(), ALL_NAMESPACES).changed();
                        let namespaces: std::collections::BTreeSet<_> = self.snapshot.resources.iter()
                            .map(|resource| if resource.kind == "Namespace" { &resource.name } else { &resource.namespace })
                            .filter(|namespace| !namespace.is_empty()).collect();
                        for namespace in namespaces {
                            changed |= ui.selectable_value(&mut self.namespace, namespace.clone(), namespace).changed();
                        }
                        if changed { ui.close_menu(); }
                    })
                }).inner.response.on_hover_text(format!("Namespace: {label}"));
                menu.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Namespace: {label}")));
                let painter = ui.painter();
                painter.rect(menu.rect, 6.0, if menu.hovered() { theme::selection() } else { panel() },
                    Stroke::new(1.0_f32, if menu.has_focus() { blue() } else { border() }), egui::StrokeKind::Inside);
                nav_icon(painter, menu.rect.left_center() + Vec2::new(18.0, 0.0), "workloads", text_color());
                nav_icon(painter, menu.rect.right_center() - Vec2::new(17.0, 0.0), "chevron", muted());
                let label_rect = menu.rect.shrink2(Vec2::new(34.0, 0.0));
                painter.with_clip_rect(label_rect).text(label_rect.left_center(), Align2::LEFT_CENTER, &label, FontId::proportional(13.0), text_color());
            });
        });
        if changed {
            self.selected = None;
            self.visualize.reset_scope();
        }
        let description = match self.page.as_str() {
            "Overview" => Some("Health, capacity, and activity across this cluster."),
            "Workloads Overview" => Some("Review the workload types in the selected namespace scope."),
            "Visualize" => Some("Trace relationships between Kubernetes resources."),
            "Pods" => Some("Inspect pod health, placement, and container readiness."),
            "Deployments" => Some("Compare desired replicas with the workloads that are ready and available."),
            "Ingresses" => Some("Inspect HTTP routes, TLS, and load balancer addresses."),
            "Secrets" => Some("Inspect Secret metadata and decode individual values when needed."),
            "Nodes" => Some("Review readiness and resource utilization across the cluster."),
            "Events" => Some("Inspect Kubernetes events and jump to the resources they reference."),
            "Port Forwarding" => Some("Review and manage TCP forwards from this computer."),
            "Config" | "Network" | "Storage" | "Access Control" => Some("Choose a resource type to inspect."),
            _ => Some("Browse resources reported by this cluster."),
        };
        if let Some(description) = description {
            ui.colored_label(muted(), RichText::new(description).size(14.0));
        }
        ui.add_space(8.0);
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        match self.cluster_overview.show(ui, &self.snapshot, self.demo) {
            Some(cluster_overview::Action::Open(resource)) => self.select(resource.clone()),
            Some(action @ (cluster_overview::Action::Events | cluster_overview::Action::Pods)) => {
                self.page = if matches!(action, cluster_overview::Action::Events) { "Events" } else { "Pods" }.into();
                if self.page == "Pods" { self.nav_open.insert("Workloads", true); }
                self.namespace = ALL_NAMESPACES.into();
                self.selected = None;
                self.search.clear();
            }
            None => {}
        }
    }
    fn resources(&mut self, ui: &mut egui::Ui) {
        const ACTION_WIDTH: f32 = 76.0;
        let workloads_overview = self.page == "Workloads Overview";
        let table_page = if workloads_overview { "Events" } else { &self.page };
        let Some(entry) = CATALOG.iter().find(|entry| entry.page == table_page) else { return };
        if entry.kind == "Pod" {
            let issues: Vec<_> = self.snapshot.resources.iter().filter(|pod| {
                pod.kind == "Pod" && self.in_namespace(&pod.namespace)
                    && matches!(pod.status.as_str(), "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull" | "Failed" | "Unknown")
            }).collect();
            if let Some(first) = issues.first() {
                let label = if issues.len() == 1 { "1 pod needs attention".into() } else { format!("{} pods need attention", issues.len()) };
                let pod = (*first).clone();
                let inspect = egui::Frame::new().fill(theme::warning_fill()).corner_radius(5)
                    .inner_margin(egui::Margin::symmetric(16, 7)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon, _) = ui.allocate_exact_size(Vec2::new(14.0, 27.0), Sense::hover());
                            nav_icon(ui.painter(), icon.center(), "warning", orange());
                            ui.colored_label(orange(), RichText::new(label).font(theme::semibold(13.0)));
                            ui.add_space(12.0);
                            let status = if pod.status == "CrashLoopBackOff" { "Container restart back-off" } else { &pod.status };
                            ui.colored_label(orange(), RichText::new(status).size(13.0));
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.link(RichText::new("Inspect affected pod").color(orange())).clicked()
                            }).inner
                        }).inner
                    }).inner;
                if inspect { self.select(pod); }
                ui.add_space(6.0);
            }
        }
        let all_columns = if workloads_overview {
            use ResourceColumn::*;
            vec![EventType, Source, Namespace, InvolvedObject, Message, Count, Age, LastSeen]
        } else {
            ResourceColumn::for_table(entry, self.namespace == ALL_NAMESPACES)
        };
        let mut rows = Vec::new();
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() * 0.48).clamp(120.0, 286.0);
            let search_hint = format!("Search {}…", entry.page.to_lowercase());
            let search_left = ui.cursor().left();
            let search = ui.add_sized([search_width, 34.0], egui::TextEdit::singleline(&mut self.search)
                .font(FontId::proportional(13.0)).margin(egui::Margin::symmetric(34, 7)).hint_text(search_hint));
            nav_icon(ui.painter(), Pos2::new(search_left + 18.0, search.rect.center().y), "search", muted());
            rows = self.filtered(entry.kind);
            let unit = match (entry.kind, rows.len()) {
                ("Event", 1) => "event", ("Event", _) => "events",
                ("Node", 1) => "node", ("Node", _) => "nodes",
                ("Pod", 1) => "pod", ("Pod", _) => "pods",
                (_, 1) => "resource", _ => "resources",
            };
            ui.add_space(8.0);
            ui.colored_label(muted(), RichText::new(format!("{} {unit}", rows.len())).size(13.0));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let response = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(Vec2::new(104.0, 34.0)), |ui| {
                    for &column in &all_columns {
                        let key = (entry.kind, column);
                        let mut shown = !self.hidden_columns.contains(&key);
                        let can_hide = column != all_columns[0];
                        if ui.add_enabled(can_hide, egui::Checkbox::new(&mut shown, column.title())).changed() {
                            if shown { self.hidden_columns.remove(&key); } else { self.hidden_columns.insert(key); }
                        }
                    }
                    if ui.button("Restore all columns").clicked() {
                        self.hidden_columns.retain(|(kind, _)| *kind != entry.kind);
                        ui.close_menu();
                    }
                }).response;
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Columns"));
                paint_chrome_button(ui, response.rect, &response, "Columns", Some("config"), true, false, false);
            });
        });
        ui.add_space(3.0);

        if workloads_overview {
            if let Some(page) = workloads_overview::show(ui, &self.snapshot, &self.namespace) {
                self.page = page.into();
                self.selected = None;
                self.search.clear();
                return;
            }
            ui.add_space(10.0);
        }
        let columns: Vec<_> = all_columns.into_iter().filter(|column| !self.hidden_columns.contains(&(entry.kind, *column))).collect();

        let compact_pods = entry.kind == "Pod" && self.selected.is_some();
        let base_width = |column: ResourceColumn| {
            if entry.kind != "Pod" { return column.default_width(entry.kind); }
            let (full, compact) = match column {
                ResourceColumn::Name => (326.0, 232.0), ResourceColumn::Namespace => (158.0, 121.0),
                ResourceColumn::Status => (216.0, 160.0), ResourceColumn::Ready => (116.0, 55.0),
                ResourceColumn::Restarts => (115.0, 73.0), ResourceColumn::Node => (109.0, 78.0),
                ResourceColumn::Cpu => (92.0, 92.0), _ => return column.default_width(entry.kind),
            };
            if compact_pods { compact } else {
                let scale = (ui.available_width() / 1168.0).clamp(0.70, 1.0);
                full * scale
            }
        };
        let viewport_width = ui.available_width();
        let table_right = ui.cursor().left() + viewport_width;
        let actions_left = table_right - ACTION_WIDTH;
        let mut columns: Vec<_> = columns.into_iter().map(|column| {
            let saved_width = self.table_widths.get(&(entry.kind, column)).copied();
            let weight = if saved_width.is_some() { 0.0 } else { column.growth() };
            (column, saved_width.unwrap_or_else(|| base_width(column)), weight)
        }).collect();
        let minimum_width: f32 = columns.iter().map(|(_, width, _)| width).sum();
        let extra_width = (viewport_width - ACTION_WIDTH - minimum_width).max(0.0);
        let mut growth: f32 = columns.iter().map(|(_, _, weight)| weight).sum();
        // Keep the table filled even when every visible column has a saved width.
        if growth == 0.0 {
            if let Some((_, _, weight)) = columns.last_mut() {
                *weight = 1.0;
                growth = 1.0;
            }
        }
        let columns: Vec<_> = columns.into_iter().map(|(column, width, weight)| {
            (column, width + extra_width * weight / growth)
        }).collect();
        let columns_width: f32 = columns.iter().map(|(_, width)| width).sum();
        let width = (columns_width + ACTION_WIDTH).max(viewport_width);
        let mut sort = self.table_sort.get(entry.kind).copied()
            .filter(|(sorted, _)| columns.iter().any(|(column, _)| column == sorted))
            .unwrap_or(if entry.kind == "Event" { (ResourceColumn::LastSeen, true) } else { (ResourceColumn::Name, false) });
        let manageable = self.can_manage_resource(entry.kind);
        let resources_by_uid: HashMap<_, _> = if entry.kind == "Event" {
            self.snapshot.resources.iter().enumerate().filter_map(|(index, resource)| {
                Some((resource.raw.pointer("/metadata/uid")?.as_str()?, index))
            }).collect()
        } else {
            HashMap::new()
        };
        let mut selection = None;
        let mut row_action = None;
        let overflow = width > viewport_width + 1.0;
        use egui::scroll_area::ScrollBarVisibility;
        let scrollbar = if overflow { ScrollBarVisibility::AlwaysVisible } else { ScrollBarVisibility::VisibleWhenNeeded };
        egui::ScrollArea::horizontal().drag_to_scroll(false)
            .scroll_bar_visibility(scrollbar)
            .id_salt(("resource-table", entry.kind)).show(ui, |ui| {
            ui.set_min_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (_, header) = ui.allocate_space(Vec2::new(width, 32.0));
            ui.painter().rect_filled(header, 3.0, side());
            let column_clip = Rect::from_min_max(header.left_top(), Pos2::new(actions_left, header.bottom()));
            for (column, cell) in resource_cells(header, &columns) {
                let grip_left = cell.right() - 12.0;
                let sort_area = Rect::from_min_max(cell.min, Pos2::new(grip_left, cell.bottom())).intersect(column_clip);
                let grip = Rect::from_min_max(Pos2::new(grip_left, cell.top()), cell.max).intersect(column_clip);
                if !cell.intersect(column_clip).is_positive() { continue; }
                let response = ui.interact(sort_area, ui.id().with(("sort", column)), Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() {
                    sort = (column, sort.0 == column && !sort.1);
                    self.table_sort.insert(entry.kind, sort);
                    ui.ctx().request_repaint();
                }
                let title = match (entry.kind, column) {
                    ("Node", ResourceColumn::Cpu) => "CPU usage".into(),
                    ("Node", ResourceColumn::Memory) => "Memory usage".into(),
                    ("ConfigMap", ResourceColumn::Name) => "ConfigMap".into(),
                    ("Service", ResourceColumn::Name) => "Service".into(),
                    ("PersistentVolumeClaim", ResourceColumn::Name) => "Persistent Volume Claim".into(),
                    (_, column) if column.title() == "CPU" || column.title() == "QOS" => column.title().to_owned(),
                    (_, column) if matches!(column, ResourceColumn::Field(_)) => column.title().to_owned(),
                    (_, column) => {
                        let title = column.title().to_ascii_lowercase();
                        title[..1].to_ascii_uppercase() + &title[1..]
                    }
                };
                let label = if sort.0 == column {
                    format!("{title} {}", if sort.1 { "↓" } else { "↑" })
                } else {
                    title.clone()
                };
                if response.hovered() || response.has_focus() {
                    ui.painter().with_clip_rect(column_clip).rect_filled(cell, 3.0, theme::selection());
                }
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                draw_resource_cell(&ui.painter().with_clip_rect(column_clip), Rect::from_min_max(cell.min, Pos2::new(grip_left, cell.bottom())), title, column, true, None);
                if sort.0 == column {
                    let center = Pos2::new(cell.right() - 17.0, cell.center().y);
                    let tip = if sort.1 { 2.0 } else { -2.0 };
                    ui.painter().with_clip_rect(column_clip).add(egui::Shape::line(
                        vec![
                            center + Vec2::new(-3.0, -tip),
                            center + Vec2::new(0.0, tip),
                            center + Vec2::new(3.0, -tip),
                        ],
                        Stroke::new(1.0_f32, text_color()),
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
                let resizing = resize.hovered() || resize.dragged();
                if resizing || (cell.right() - actions_left).abs() > 1.0 {
                    let color = if resizing { blue() } else { border() };
                    let x = cell.right() - 1.0;
                    ui.painter().with_clip_rect(column_clip).line_segment(
                        [Pos2::new(x, cell.top() + 7.0), Pos2::new(x, cell.bottom() - 7.0)],
                        Stroke::new(1.0_f32, color),
                    );
                }
            }

            let actions_header = Rect::from_min_max(Pos2::new(actions_left, header.top()), Pos2::new(table_right, header.bottom()));
            ui.painter().vline(actions_left, header.y_range().shrink(7.0), Stroke::new(1.0_f32, border()));
            ui.painter().text(actions_header.center(), Align2::CENTER_CENTER, "Actions", theme::semibold(12.0), muted());
            ui.interact(actions_header, ui.id().with("actions-header"), Sense::hover())
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Actions"));

            rows.sort_by(|left, right| sort.0.compare(left, right, sort.1)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.namespace.cmp(&right.namespace)));
            egui::ScrollArea::vertical()
                .id_salt(("resource-rows", entry.kind, &self.namespace))
                .max_height((ui.available_height() - if overflow { 34.0 } else { 0.0 }).max(34.0))
                .show(ui, |ui| {
                    if rows.is_empty() {
                        ui.add_space(16.0);
                        ui.colored_label(muted(), "No resources match this view.");
                        ui.add_space(16.0);
                    }
                    for (index, resource) in rows.iter().enumerate() {
                        let row_height = if matches!(entry.kind, "Node" | "Event" | "Deployment" | "ConfigMap" | "Service" | "PersistentVolumeClaim" | "Namespace") { 52.0 } else { 42.0 };
                        let (_, row) = ui.allocate_space(Vec2::new(width, row_height));
                        let id = ui.id().with((&resource.kind, &resource.namespace, &resource.name));
                        let row_clip = Rect::from_min_max(row.left_top(), Pos2::new(actions_left, row.bottom()));
                        let response = ui.interact(row_clip, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                        let selected = self.selected.as_ref().is_some_and(|item| same_object(&item.raw, &resource.raw));
                        let fill = if selected { theme::selection() }
                            else if response.hovered() || response.has_focus() { side() }
                            else if index % 2 == 0 { panel() } else { side() };
                        let painter = ui.painter();
                        painter.rect_filled(row, 0.0, fill);
                        painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, border()));
                        if selected {
                            painter.rect_filled(Rect::from_min_size(row.min, Vec2::new(1.0, row.height())), 0.0, blue());
                        }
                        let mut linked_resource = None;
                        for (column, cell) in resource_cells(row, &columns) {
                            if !cell.intersect(row_clip).is_positive() { continue; }
                            let warning = column == ResourceColumn::Message && resource.status == "Warning";
                            let mut accent = warning.then_some(orange());
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
                                    let link = ui.interact(cell.intersect(row_clip), id.with("involved-object"), Sense::click())
                                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                                    link.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, &label));
                                    accent = Some(if link.hovered() || link.has_focus() { blue().gamma_multiply(1.35) } else { blue() });
                                    if link.on_hover_text(&label).clicked() { linked_resource = Some(target); }
                                }
                            }
                            draw_resource_cell(&painter.with_clip_rect(row_clip), cell, label, column, false, accent);
                        }

                        let caption = if entry.kind == "Event" { event_message(resource) } else { &resource.name };
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, caption));
                        let response = match entry.kind {
                            "Event" | "Pod" => response.on_hover_text(caption),
                            "Node" => response.on_hover_text(format!("Used / total capacity\nCPU: {}\nMemory: {}",
                                ResourceColumn::Cpu.value(resource), ResourceColumn::Memory.value(resource))),
                            _ => response,
                        };
                        if let Some(target) = linked_resource {
                            selection = Some(target.clone());
                        } else if response.clicked() {
                            selection = Some(resource.clone());
                        }
                        response.context_menu(|ui| {
                            if let Some(action) = row_menu(ui, resource, manageable) {
                                row_action = Some((resource.clone(), action));
                                ui.close_menu();
                            }
                        });
                        let slot = Rect::from_min_max(Pos2::new(actions_left, row.top()), Pos2::new(table_right, row.bottom()));
                        ui.painter().vline(actions_left, slot.y_range(), Stroke::new(1.0_f32, border()));
                        let action = ui.interact(slot, id.with("actions"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                        let caption = format!("Actions for {}", resource.name);
                        action.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &caption));
                        let action = action.on_hover_text(caption);
                        if action.hovered() || action.has_focus() { ui.painter().rect_filled(slot.shrink(3.0), 5.0, theme::selection()); }
                        ui_icons::draw(ui.painter(), slot.center(), "more", Stroke::new(1.3_f32, muted()));
                        let popup = id.with("action-menu");
                        if action.clicked() { ui.memory_mut(|mem| mem.toggle_popup(popup)); }
                        if let Some(Some(action)) = egui::popup_below_widget(ui, popup, &action, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                            row_menu(ui, resource, manageable)
                        }) {
                            row_action = Some((resource.clone(), action));
                            ui.memory_mut(|mem| mem.close_popup());
                        }
                    }
            });
        });
        if overflow {
            ui.colored_label(muted(), "Scroll horizontally for more columns");
        }
        if let Some((resource, action)) = row_action {
            let container = self.selected.as_ref()
                .filter(|selected| same_object(&resource.raw, &selected.raw))
                .map(|_| self.container.clone())
                .unwrap_or_else(|| default_container(&resource));
            self.perform_resource_action(ui.ctx(), &resource, &container, action);
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
                if self.target.is_some() {
                    ui.label("Could not load resources for the active cluster.");
                    self.cluster_data_issue(ui, true);
                } else if self.contexts.is_empty() {
                    ui.label("No local contexts found. Add a kubeconfig or scan again.");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Add Kubeconfig…").clicked() {
                            #[cfg(target_os = "macos")]
                            if self.kubeconfig_settings.pick_file() {
                                self.settings_page = Some(SettingsPage::Kubernetes);
                            }
                            #[cfg(not(target_os = "macos"))]
                            { self.settings_page = Some(SettingsPage::Kubernetes); }
                        }
                        if ui.button("Scan local kubeconfigs").clicked() {
                            self.discover_source(String::new(), true);
                        }
                    });
                    if !self.notice.is_empty() && self.notice != "0 local contexts found" {
                        ui.colored_label(muted(), &self.notice);
                    }
                } else {
                    ui.label("Choose a context under Local Kubeconfigs to connect.");
                    if ui.button("Connection settings").clicked() {
                        self.settings_page = Some(SettingsPage::Connections);
                    }
                }
            }
            return;
        }
        self.toolbar(ui);
        if matches!(self.page.as_str(), "Overview" | "Port Forwarding") {

            egui::ScrollArea::vertical().id_salt("main-scroll")
                .show(ui, |ui| self.workspace_content(ui));
        } else {
            self.workspace_content(ui);
        }
    }

    fn workspace_content(&mut self, ui: &mut egui::Ui) {
        if Self::page_uses_namespace(&self.page) && self.namespace.is_empty() {
            ui.label("Select a namespace to view resources in this cluster.");
            return;
        }
        if !self.snapshot.errors.is_empty() {
            self.cluster_data_issue(ui, false);
            ui.add_space(6.0);
        }
        match self.page.as_str() {
            "Overview" => self.overview(ui),
            "Visualize" => self.visualize(ui),
            "Port Forwarding" => self.port_forwards(ui),
            "Config" | "Network" | "Storage" | "Access Control" | "Workloads Overview" => self.resource_group(ui),
            _ => self.resources(ui),
        }
    }

    fn resource_group(&mut self, ui: &mut egui::Ui) {
        let workloads = self.page == "Workloads Overview";
        let group = if workloads { "Workloads" } else { self.page.as_str() };
        let mut selected = None;
        let row = |ui: &mut egui::Ui, values: [&str; 3], page: Option<&str>, striped: bool| {
            let fill = if striped || page.is_none() { side() } else { panel() };
            egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(14, 8)).show(ui, |ui| {
                ui.columns(3, |columns| {
                    let clicked = if page.is_some() { columns[0].link(RichText::new(values[0]).color(blue())).clicked() }
                        else { columns[0].colored_label(muted(), values[0]); false };
                    columns[1].label(values[1]);
                    let open = columns[2].horizontal(|ui| {
                        ui.label(values[2]);
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            page.is_some_and(|page| {
                                ui.add(egui::Button::new("›").frame(false))
                                    .on_hover_text(format!("Browse {page}")).clicked()
                            })
                        }).inner
                    }).inner;
                    clicked || open
                })
            }).inner
        };
        egui::Frame::new().fill(panel()).stroke(Stroke::new(1.0_f32, border())).corner_radius(6).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            row(ui, if workloads { ["Resource type", "Resources", "Status"] } else { ["Resource type", "Scope", "Resources"] }, None, false);
            for (index, entry) in CATALOG.iter().filter(|entry| entry.group == group).enumerate() {
                let records: Vec<_> = self.snapshot.kind(entry.kind).into_iter()
                    .filter(|resource| !workloads || self.in_namespace(&resource.namespace)).collect();
                let count = records.len().to_string();
                let mut status = std::collections::BTreeMap::new();
                for resource in &records {
                    if !resource.status.is_empty() { *status.entry(resource.status.as_str()).or_insert(0) += 1; }
                }
                let status = status.iter().map(|(label, count)| format!("{count} {label}")).collect::<Vec<_>>().join(" · ");
                let scope = if entry.namespaced { "Namespaced" } else { "Cluster-wide" };
                let values = if workloads { [entry.page, count.as_str(), status.as_str()] } else { [entry.page, scope, count.as_str()] };
                if row(ui, values, Some(entry.page), index % 2 != 0) { selected = Some(entry.page); }
            }
            if group == "Network" && row(ui, ["Port Forwarding", "Local", ""], Some("Port Forwarding"), false) {
                selected = Some("Port Forwarding");
            }
        });
        if workloads {
            egui::CollapsingHeader::new("Workload activity and events").show(ui, |ui| self.resources(ui));
        }
        if let Some(page) = selected { self.open_page(page); }
    }

    fn cluster_data_issue(&mut self, ui: &mut egui::Ui, full_failure: bool) {
        let errors = &self.snapshot.errors;
        let issue = errors.iter().find(|error| error.split_once(':').is_some_and(|(source, _)| source == self.page))
            .or_else(|| errors.first());
        if let Some(issue) = issue {
            let source = issue.split_once(':').map_or(issue.as_str(), |(source, _)| source);
            ui.colored_label(orange(), format!("{source} · data unavailable or limited"));
            ui.colored_label(muted(), if issue.contains("Forbidden") || issue.contains("permissions limit") {
                "Check your access for this resource and namespace, then retry."
            } else {
                "Check the cluster connection or credentials, then retry."
            });
        }
        let retry = ui.horizontal_wrapped(|ui| {
            let retry = ui.add_enabled(self.target.is_some() && !self.busy, egui::Button::new("Retry cluster refresh")).clicked();
            if full_failure && ui.button("Connection settings").clicked() {
                self.settings_page = Some(SettingsPage::Connections);
            }
            retry
        }).inner;
        if !errors.is_empty() {
            ui.collapsing(format!("Source details ({})", errors.len()), |ui| {
                egui::ScrollArea::vertical().max_height(130.0).show(ui, |ui| {
                    for error in errors { ui.label(error); }
                });
            });
        }
        if retry { self.start_fetch(); }
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
        let workspace_width = ctx.available_rect().width();
        let min_width = if resource.kind == "Pod" { 344.0 } else { 310.0 };
        let max_width = (workspace_width * 0.65).clamp(min_width, 760.0);
        let default_width = (workspace_width * 0.34).clamp(min_width, 440.0);
        let cluster = self.workspace_context().to_owned();
        let mut close = false;
        let mut header_action = None;
        egui::SidePanel::right("resource-details")
            .resizable(true)
            .default_width(default_width)
            .width_range(min_width..=max_width)
            .frame(egui::Frame::new().fill(panel()).inner_margin(egui::Margin::symmetric(24, 16)))
            .show(ctx, |ui| {
                let manageable = self.can_manage_resource(&resource.kind);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let (kind_icon, _) = ui.allocate_exact_size(Vec2::new(20.0, 30.0), Sense::hover());
                        nav_icon(ui.painter(), kind_icon.center(), "cluster", blue());
                        ui.add_space(2.0);
                        ui.colored_label(blue(), RichText::new(&resource.kind).font(theme::semibold(13.0)));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            close = ui_icons::button(ui, "close", "Close details", true, false).clicked();
                            if ui_icons::button(ui, "delete", "Delete resource…", manageable && resource.kind != "Event", false).clicked() {
                                header_action = Some(ResourceAction::Delete);
                            }
                            let edit_label = if resource.kind == "Event" { "View YAML…" } else { "Edit YAML…" };
                            if ui_icons::button(ui, "edit", edit_label, manageable, false).clicked() {
                                header_action = Some(ResourceAction::Edit);
                            }
                            if resource.kind != "Pod" { return; }
                            for (icon, label, enabled, action) in [
                                ("logs", "Logs", true, ResourceAction::Logs),
                                ("share", "Copy resource reference", true, ResourceAction::CopyReference),
                                ("evict", "Evict Pod…", manageable, ResourceAction::Evict),
                                #[cfg(target_os = "macos")]
                                ("terminal", "Open Shell in Terminal", manageable && !self.container.is_empty(), ResourceAction::Shell),
                                #[cfg(target_os = "macos")]
                                ("attach", "Attach Pod", manageable && !self.container.is_empty(), ResourceAction::Attach),
                            ] {
                                let active = icon == "logs" && !self.console && self.pod_logs.as_ref()
                                    .is_some_and(|logs| same_object(&logs.pod.raw, &resource.raw));
                                if ui_icons::button(ui, icon, label, enabled, active).clicked() { header_action = Some(action); }
                            }
                        });
                    });
                    if close { return; }
                    ui.add_space(7.0);
                    ui.add(egui::Label::new(RichText::new(&resource.name).font(theme::semibold(23.0))).wrap());
                    ui.add_space(10.0);
                    ui.horizontal_wrapped(|ui| {
                        let namespace = if resource.namespace.is_empty() { "Cluster-wide" } else { &resource.namespace };
                        ui.label(RichText::new(namespace).font(theme::semibold(13.0)));
                        ui.separator();
                        ui.colored_label(muted(), RichText::new(&cluster).size(13.0));
                    });
                    if !resource.status.is_empty() {
                        ui.add_space(12.0);
                        badge(ui, &resource.status, status_color(&resource.status));
                    }
                    if matches!(resource.kind.as_str(), "Pod" | "Service") {
                        ui.add_space(18.0);
                        if settings_button(ui, "Port forward", None, 110.0, true, false).clicked() {
                            self.detail_tab = "Port Forward".into();
                        }
                    }
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 20.0;
                        for (tab, width) in [("Details", 58.0), ("Configuration", 100.0), ("Events", 64.0)] {
                            let selected = self.detail_tab == tab;
                            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click());
                            let color = if selected { blue() } else { muted() };
                            let font = if selected { theme::semibold(13.0) } else { FontId::proportional(13.0) };
                            ui.painter().text(rect.left_center() + Vec2::new(3.0, 0.0), Align2::LEFT_CENTER, tab, font, color);
                            response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, tab));
                            if selected {
                                ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(2.0_f32, blue()));
                            }
                            if response.clicked() { self.detail_tab = tab.into(); }
                        }
                    });
                    settings_divider(ui);
                });
                if close { return; }
                if self.selected.is_none() { return; }

                let scroll_key = (&resource.kind, &resource.namespace, &resource.name, &self.detail_tab);
                // Keep the drawer scrollbar outside the resource controls' hit area.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().id_salt(scroll_key).auto_shrink([false, false]).show(ui, |ui| {
                    match self.detail_tab.as_str() {
                        "Configuration" => {
                            let json = serde_json::to_string_pretty(&resource.raw).unwrap_or_default();
                            if resource.kind == "Secret" { ui.label("Secret metadata. Open the YAML editor to fetch its values."); }
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Copy JSON").clicked() { ctx.copy_text(json.clone()); }
                                let label = if resource.kind == "Event" { "View YAML…" } else { "Edit YAML…" };
                                if ui.add_enabled(manageable, egui::Button::new(label)).clicked() {
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
        else if let Some(action) = header_action {
            self.perform_resource_action(ctx, &resource, &self.container.clone(), action);
        }
    }

    fn resource_details(&mut self, ui: &mut egui::Ui, resource: &Resource) {
        ui.spacing_mut().item_spacing.y = 3.0;
        let target = self.target.as_ref();
        let scope = (
            &resource.kind, &resource.namespace, &resource.name,
            resource.raw.pointer("/metadata/uid").and_then(Value::as_str),
            target.map(|target| (&target.config, &target.context)),
        );
        let action = ui.push_id(scope, |ui| {
            resource_details::show(ui, &self.snapshot, resource, &mut self.detail_metrics, target, &mut self.env_secrets)
        }).inner;
        match action {
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
            if ui.add_enabled(self.can_manage_resource(&resource.kind) && !self.container.is_empty(), egui::Button::new("Open Shell in Terminal")).clicked() {
                self.open_container_terminal(resource, &self.container.clone(), false);
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
            if ui.add_enabled(self.can_manage_resource(&resource.kind) && resource.kind != "Event", egui::Button::new(RichText::new("Delete resource…").color(orange()))).clicked() {
                self.perform_resource_action(ui.ctx(), resource, "", ResourceAction::Delete);
            }
        });
        resource_details::events(ui, &self.snapshot, resource);
        if resource.kind == "Secret" {
            resource_details::section(ui, "Data");
            self.env_secrets.show_data(ui, self.target.as_ref(), resource);
        }
    }

    fn can_manage_resource(&self, kind: &str) -> bool {
        !self.busy && (self.target.is_some() || self.demo && kind == "Pod")
    }

    fn perform_resource_action(&mut self, ctx: &egui::Context, resource: &Resource, container: &str, action: ResourceAction) {
        match action {
            ResourceAction::Details => self.select(resource.clone()),
            ResourceAction::Logs => self.open_pod_logs(resource, container.to_owned()),
            ResourceAction::Edit => self.load_manifest(resource),
            ResourceAction::Delete | ResourceAction::Evict => {
                let (args, input, output_to) = if matches!(action, ResourceAction::Evict) {
                    let (args, request) = eviction_request(resource);
                    (args, Some(request), OutputTo::Eviction)
                } else {
                    (resource_args("delete", resource, words(&["--wait=false"])), None, OutputTo::Action)
                };
                if self.demo && resource.kind == "Pod" {
                    self.confirm = Some(Action { target: ActionTarget::SamplePod(resource.clone()), args, input, output_to });
                } else {
                    self.queue(args, input, output_to);
                }
            }
            ResourceAction::CopyReference => {
                let reference = if resource.namespace.is_empty() { format!("{}/{}", resource.kind, resource.name) }
                    else { format!("{}/{}/{}", resource.namespace, resource.kind, resource.name) };
                ctx.copy_text(reference);
                self.notice = "Resource reference copied".into();
            }
            #[cfg(target_os = "macos")]
            ResourceAction::Shell | ResourceAction::Attach =>
                self.open_container_terminal(resource, container, matches!(action, ResourceAction::Attach)),
        }
    }

    #[cfg(target_os = "macos")]
    fn open_container_terminal(&mut self, resource: &Resource, container: &str, attach: bool) {
        if container.is_empty() || self.busy { return; }
        let args = pod_terminal_args(resource, container, attach);
        if self.demo {
            self.output = format!("Sample workspace. Connect a cluster to start a container session.\n\n$ kubectl {}", args.join(" "));
            self.console = true;
            return;
        }
        let Some(target) = self.target.clone() else { return };
        self.busy = true;
        self.output = "Opening Terminal…".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Reply::Output(OutputTo::Console, target.open_terminal(&args)));
        });
        self.console = true;
    }

    fn open_pod_logs(&mut self, pod: &Resource, container: String) {
        self.console = false;
        if self.pod_logs.as_ref().is_some_and(|logs| same_object(&logs.pod.raw, &pod.raw) && logs.container == container) {
            return;
        }
        let mut logs = PodLogs::new(pod.clone(), container);
        logs.follow(self.target.as_ref());
        self.pod_logs = Some(logs);
    }

    fn log_dock(&mut self, ctx: &egui::Context) {
        if self.console { return; }
        let Some(logs) = &mut self.pod_logs else { return };
        let max_height = (ctx.screen_rect().height() * 0.66).max(185.0);
        let expanded = logs.expanded;
        let dock = egui::TopBottomPanel::bottom(if expanded { "pod-logs-expanded" } else { "pod-logs-dock" });
        let dock = if expanded { dock.exact_height(max_height) } else {
            dock.resizable(true).default_height(300.0_f32.min(max_height)).height_range(185.0..=max_height)
        };
        let event = dock.frame(egui::Frame::new().fill(pod_logs::CHROME).inner_margin(egui::Margin { left: 28, right: 24, top: 11, bottom: 17 }))
            .show(ctx, |ui| logs.show(ui, self.target.as_ref())).inner;
        match event {
            Some(pod_logs::Event::Close) => self.pod_logs = None,
            Some(pod_logs::Event::Pod) => {
                if let Some(logs) = &self.pod_logs { self.select(logs.pod.clone()); }
            }
            Some(pod_logs::Event::Namespace) => {
                if let Some(logs) = &self.pod_logs {
                    let resource = self.snapshot.resources.iter().find(|item| item.kind == "Namespace" && item.name == logs.pod.namespace).cloned();
                    self.page = "Namespaces".into();
                    self.search.clear();
                    self.selected = None;
                    if let Some(resource) = resource { self.select(resource); }
                }
            }
            None => {}
        }
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
                    ui.colored_label(muted(), forward.stream.status.as_deref().unwrap_or(state));
                    if !output.is_empty() {
                        egui::CollapsingHeader::new("Session output").show(ui, |ui| { ui.monospace(output); });
                    }
                });
            });
        }
        if !visible {
            let browse = card(ui, |ui| {
                ui.label(RichText::new("Sessions").size(18.0).strong());
                ui.separator();
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    ui.strong("No active sessions");
                    ui.colored_label(muted(), "Open a Pod or Service and select Port Forward to start a session.");
                    let browse = ui.button("Browse pods").clicked();
                    ui.add_space(16.0);
                    browse
                }).inner
            });
            if browse { self.nav_open.insert("Workloads", true); self.open_page("Pods"); }
        }
        if self.demo {
            card(ui, |ui| {
                ui.label(RichText::new("Example session").size(18.0).strong());
                ui.colored_label(muted(), "Illustrative session · no connection is open");
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Service / gateway");
                    ui.label("production");
                    ui.monospace("127.0.0.1:8080 → 80/TCP");
                    badge(ui, "Listening", blue());
                });
            });
        }
        if let Some(index) = remove { self.forwards.remove(index); }
    }

    fn manifest_editor(&mut self, ui: &mut egui::Ui) {
        let context = self.workspace_context().to_owned();
        let Some(manifest) = &mut self.manifest else { return };
        let mut submit = None;
        let readonly = manifest.original.as_ref().is_some_and(|original| original["kind"] == "Event");
        let close = ui.horizontal(|ui| {
            ui.heading(if manifest.original.is_some() { "Resource YAML" } else { "Apply manifest" });
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled(!self.busy, egui::Button::new("Back to Workspace")).clicked()
            }).inner
        }).inner;
        ui.label(format!("Context: {context}"));
        if self.demo { ui.colored_label(muted(), "Changes apply only to the sample workspace until refresh."); }
        ui.separator();
        if close {
            self.manifest = None;
            return;
        }
        let editor_height = (ui.available_height() - 65.0).max(160.0);
        egui::ScrollArea::both().max_height(editor_height).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut manifest.yaml)
                .code_editor().desired_width(f32::INFINITY).desired_rows(18)
                .interactive(!readonly && !self.busy)
                .hint_text("Paste Kubernetes YAML here"));
        });
        if !manifest.feedback.is_empty() { ui.label(&manifest.feedback); }
        if !readonly {
            let button = if self.demo { "Save sample changes" } else if manifest.original.is_some() { "Save changes…" } else { "Apply…" };
            if ui.add_enabled(!self.busy && !manifest.yaml.trim().is_empty(), egui::Button::new(button)).clicked() {
                if self.demo {
                    self.save_sample_manifest();
                    return;
                }
                match manifest_request(manifest, &self.namespace) {
                    Ok(request) => submit = Some(request),
                    Err(error) => manifest.feedback = error,
                }
            }
        }
        if let Some((args, input)) = submit { self.queue(args, Some(input), OutputTo::ManifestSave); }
    }

    fn save_sample_manifest(&mut self) {
        let Some(manifest) = &mut self.manifest else { return };
        let Some(original) = &manifest.original else { return };
        let edited = match edited_manifest(&manifest.yaml, original) {
            Ok(edited) => edited,
            Err(error) => { manifest.feedback = error; return; }
        };
        let Some(current) = self.snapshot.resources.iter_mut().find(|resource| &resource.raw == original) else {
            manifest.feedback = "The sample resource changed. Reopen its YAML to try again.".into();
            return;
        };
        let mut updated = Resource::parse(edited);
        updated.cpu = current.cpu;
        updated.memory = current.memory;
        *current = updated.clone();
        if self.selected.as_ref().is_some_and(|resource| same_object(&updated.raw, &resource.raw)) {
            self.select(updated);
        }
        self.manifest = None;
        self.notice = "Sample YAML saved".into();
    }

    fn confirm_cluster_action(&mut self, ui: &mut egui::Ui, action: Action) {
        let verb = action.args.first().map(String::as_str).unwrap_or_default();
        let resource = if matches!(action.output_to, OutputTo::Action) {
            action.args.get(1).and_then(|api| CATALOG.iter().find(|entry| entry.api == api))
                .zip(action.args.get(2))
        } else {
            None
        };
        let eviction = if matches!(action.output_to, OutputTo::Eviction) {
            action.input.as_deref().and_then(|body| serde_json::from_str::<Value>(body).ok())
        } else { None };
        let label = match (verb, resource, action.output_to) {
            (_, _, OutputTo::Eviction) => "Evict Pod".into(),
            ("delete", Some((entry, _)), _) => format!("Delete {}", entry.kind),
            ("scale", Some((entry, _)), _) => format!("Scale {}", entry.kind),
            ("replace", _, OutputTo::ManifestSave) => "Save resource changes".into(),
            ("apply", _, OutputTo::ManifestSave) => "Apply manifest".into(),
            _ => "Run kubectl command".into(),
        };
        ui.heading(&label);
        let namespace = action.args.windows(2).find(|pair| pair[0] == "-n").map(|pair| pair[1].as_str());
        match &action.target {
            ActionTarget::Cluster(target) => {
                ui.label(format!("Cluster: {}", cluster_label(&target.context))).on_hover_text(&target.context);
            }
            ActionTarget::SamplePod(_) => { ui.label("Sample workspace · changes apply until refresh"); }
        }
        if let Some(request) = &eviction {
            ui.label(format!("Pod: {}", text(request, "/metadata/name")));
            ui.label(format!("Namespace: {}", text(request, "/metadata/namespace")));
        } else if let Some((entry, name)) = resource {
            ui.label(format!("{}: {name}", entry.kind));
            ui.label(format!("Namespace: {}", namespace.unwrap_or("Cluster-wide")));
        } else if matches!(action.output_to, OutputTo::ManifestSave) {
            if let Some(original) = self.manifest.as_ref().and_then(|manifest| manifest.original.as_ref()) {
                ui.label(format!("{}: {}", text(original, "/kind"), text(original, "/metadata/name")));
                ui.label(format!("Namespace: {}", namespace.unwrap_or("Cluster-wide")));
            } else {
                ui.label(format!("Namespace: {}", namespace.unwrap_or("From manifest or cluster context")));
            }
        }
        let destructive = eviction.is_some() || verb == "delete" && resource.is_some();
        if matches!(&action.target, ActionTarget::SamplePod(_)) {
            ui.colored_label(orange(), "This removes the Pod from the sample workspace.");
        } else if eviction.is_some() {
            ui.colored_label(orange(), "This requests graceful termination of the Pod, subject to disruption budgets.");
        } else if destructive {
            ui.colored_label(orange(), "This requests deletion of the resource from the cluster.");
        } else if verb == "scale" {
            if let Some(replicas) = action.args.iter().find_map(|arg| arg.strip_prefix("--replicas=")) {
                ui.label(format!("Requested replicas: {replicas}"));
            }
        }
        ui.separator();
        if matches!(&action.target, ActionTarget::Cluster(_)) {
            egui::CollapsingHeader::new("Command preview")
                .default_open(resource.is_none())
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal().show(ui, |ui| {
                        ui.monospace(format!("kubectl {}", action.args.join(" ")));
                    });
                });
        }
        if let Some(manifest) = &self.manifest {
            if matches!(action.output_to, OutputTo::ManifestSave) {
                egui::ScrollArea::both().max_height(360.0).show(ui, |ui| {
                    ui.monospace(&manifest.yaml);
                });
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                self.confirm = None;
            }
            let can_run = !self.busy && match &action.target {
                ActionTarget::Cluster(target) => self.target.as_ref() == Some(target),
                ActionTarget::SamplePod(pod) => self.demo && self.snapshot.resources.iter()
                    .any(|resource| same_object(&pod.raw, &resource.raw)),
            };
            let mut button = egui::Button::new(label);
            if destructive {
                button = button.stroke(Stroke::new(1.0_f32, orange()));
            }
            if ui.add_enabled(can_run, button).clicked() {
                self.confirm = None;
                self.execute(action);
            }
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui, page: SettingsPage) {
        let (title, description) = match page {
            SettingsPage::Appearance => ("Settings", "Manage preferences for Cluster Observatory."),
            SettingsPage::Kubernetes => ("Kubeconfigs", "Manage the local files used to discover Kubernetes contexts."),
            SettingsPage::Connections => ("Cluster connections", "Connect using your existing Kubernetes credentials."),
            #[cfg(target_os = "macos")]
            SettingsPage::Updates => ("Settings", "Manage preferences for Cluster Observatory."),
        };
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                workspace_title(ui, title);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    match page {
                        SettingsPage::Connections => {
                            if settings_button(ui, "Manage kubeconfigs", Some("file"), 183.0, true, false).clicked() {
                                self.settings_page = Some(SettingsPage::Kubernetes);
                            }
                        }
                        SettingsPage::Kubernetes => {
                            if settings_button(ui, "Back to connections", None, 165.0, true, false).clicked() {
                                self.settings_page = Some(SettingsPage::Connections);
                            }
                        }
                        _ => {
                            if settings_button(ui, "Back to overview", None, 145.0, true, false).clicked() {
                                self.open_page("Overview");
                            }
                        }
                    }
                });
            });
            ui.add_space(1.0);
            ui.label(RichText::new(description).size(14.0).color(muted()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 11.0;
                for (tab, title, width) in [
                    (SettingsPage::Appearance, "Appearance", 117.0),
                    (SettingsPage::Kubernetes, "Kubernetes", 111.0),
                    (SettingsPage::Connections, "Connections", 118.0),
                    #[cfg(target_os = "macos")]
                    (SettingsPage::Updates, "Updates", 84.0),
                ] {
                    let selected = page == tab;
                    let font = if selected { theme::semibold(14.0) } else { FontId::proportional(14.0) };
                    let (tab_rect, response) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::click());
                    let color = if selected { blue() } else { muted() };
                    if response.has_focus() || response.hovered() {
                        ui.painter().rect_filled(tab_rect.shrink(2.0), 5.0, side());
                    }
                    ui.painter().text(tab_rect.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, title, font, color);
                    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, title));
                    if selected {
                        ui.painter().hline(response.rect.x_range(), response.rect.bottom() - 1.0, Stroke::new(2.0_f32, blue()));
                    }
                    if response.clicked() { self.settings_page = Some(tab); }
                }
            });
            settings_divider(ui);
        });
        ui.add_space(5.0);
        let Some(page) = self.settings_page else { return };
        egui::ScrollArea::vertical().id_salt("settings-content").show(ui, |ui| match page {
            SettingsPage::Appearance => self.appearance_settings(ui),
            SettingsPage::Kubernetes => self.kubeconfig_settings.show(ui),
            SettingsPage::Connections => self.connection_settings(ui),
            #[cfg(target_os = "macos")]
            SettingsPage::Updates => card(ui, |ui| self.updates.show_settings(ui)),
        });
    }

    fn appearance_settings(&mut self, ui: &mut egui::Ui) {
        let before = self.appearance;
        egui::Frame::new().fill(panel()).stroke(Stroke::new(1.0_f32, border()))
            .corner_radius(7).inner_margin(23).show(ui, |ui| {
            ui.label(RichText::new("Appearance").font(theme::semibold(22.0)));
            ui.colored_label(muted(), "Choose a color theme for the app. Changes apply immediately.");
            ui.add_space(14.0);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                ui.columns(2, |columns| {
                for (column, preference) in columns.iter_mut().zip([theme::Preference::Light, theme::Preference::Dark]) {
                    let selected = self.appearance == preference;
                    egui::Frame::new().fill(if selected { theme::selection() } else { side() })
                        .stroke(Stroke::new(1.0_f32, if selected { blue() } else { border() }))
                        .corner_radius(7).inner_margin(18).show(column, |ui| {
                            ui.set_min_height(262.0);
                            appearance_preview(ui, preference == theme::Preference::Dark);
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.appearance, preference, RichText::new(preference.label()).font(theme::semibold(16.0)));
                                if selected {
                                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                        badge(ui, "Current", blue());
                                    });
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.add_space(20.0);
                                ui.colored_label(muted(), RichText::new(if preference == theme::Preference::Light { "Light surfaces and dark text" }
                                    else { "Dark surfaces and light text" }).size(13.0));
                            });
                        });
                }
                });
            });
            ui.add_space(11.0);
            let system_selected = self.appearance == theme::Preference::System;
            egui::Frame::new().fill(if system_selected { theme::selection() } else { side() })
                .stroke(Stroke::new(1.0_f32, if system_selected { blue() } else { border() }))
                .corner_radius(6).inner_margin(egui::Margin::symmetric(18, 10)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height(51.0);
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.appearance, theme::Preference::System, RichText::new("System").font(theme::semibold(16.0)));
                        if system_selected {
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| { badge(ui, "Current", blue()); });
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.colored_label(muted(), RichText::new(if cfg!(target_os = "macos") { "Follows your macOS appearance" }
                            else { "Follows your operating system’s appearance" }).size(13.0));
                    });
                });
            ui.add_space(12.0);
            ui.colored_label(muted(), RichText::new("Theme applies to every cluster and workspace on this device.").size(13.0));
        });
        if self.appearance != before {
            if let Some(dir) = &self.appearance_dir {
                if let Err(error) = self.appearance.save(dir) {
                    self.notice = format!("Could not save appearance: {error}");
                }
            }
            ui.ctx().request_repaint();
        }
    }

    fn connection_settings(&mut self, ui: &mut egui::Ui) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new().fill(theme::selection()).corner_radius(7)
                .inner_margin(egui::Margin::symmetric(20, 15)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 200.0).max(120.0));
                            ui.set_min_height(53.0);
                            ui.label(RichText::new("Sample workspace").font(theme::semibold(16.0)));
                            ui.add_space(5.0);
                            ui.colored_label(muted(), RichText::new("Explore a cluster with illustrative resources and metrics.").size(13.0));
                        });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if settings_button(ui, "Open sample workspace", None, 190.0, !self.busy, true).clicked() {
                                self.switch(None);
                                self.settings_page = None;
                            }
                        });
                    });
                });

            ui.add_space(24.0);
            settings_panel().show(ui, |ui| {
                ui.set_min_height(190.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Local contexts").font(theme::semibold(19.0)));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if settings_button(ui, "Scan local kubeconfigs", Some("refresh"), 191.0, !self.busy, false).clicked() {
                            self.kubeconfig.clear();
                            self.context.clear();
                            self.discover(false);
                        }
                    });
                });
                ui.colored_label(muted(), RichText::new("Scan your kubeconfigs to discover saved contexts.").size(13.0));
                ui.add_space(22.0);
                settings_divider(ui);
                ui.add_space(20.0);
                if self.contexts.is_empty() {
                    ui.horizontal_top(|ui| {
                        let (icon, _) = ui.allocate_exact_size(Vec2::new(20.0, 22.0), Sense::hover());
                        nav_icon(ui.painter(), icon.center(), "cluster", blue());
                        ui.vertical(|ui| {
                            ui.label(RichText::new("No contexts loaded").font(theme::semibold(15.0)));
                            ui.add_space(7.0);
                            ui.colored_label(muted(), RichText::new("Scan local kubeconfigs or read a specific file below.").size(13.0));
                        });
                    });
                } else {
                    ui.add_enabled_ui(!self.busy, |ui| {
                        egui::ComboBox::from_id_salt("context-picker")
                            .selected_text(if self.context.is_empty() { "Choose a context" } else { &self.context })
                            .width(350.0).show_ui(ui, |ui| {
                                for entry in &self.contexts {
                                    let source = if entry.config.is_empty() { "kubectl defaults" } else { &entry.config };
                                    let selected = self.context == entry.context && self.kubeconfig == entry.config;
                                    if ui.selectable_label(selected, &entry.context).on_hover_text(source).clicked() {
                                        self.context = entry.context.clone();
                                        self.kubeconfig = entry.config.clone();
                                    }
                                }
                            });
                    });
                }
            });

            ui.add_space(24.0);
            settings_panel().show(ui, |ui| {
                ui.set_min_height(249.0);
                ui.label(RichText::new("Manual configuration").font(theme::semibold(19.0)));
                ui.add_space(27.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 36.0;
                    ui.columns(3, |columns| {
                        let [executable, config, context] = columns else { unreachable!() };
                        connection_field(executable, "Kubectl executable", &mut self.exe, "", !self.busy, true);
                        if connection_field(config, "Kubeconfig path (optional)", &mut self.kubeconfig, "Use kubectl defaults", !self.busy, false).changed() {
                            self.context.clear();
                        }
                        config.add_space(15.0);
                        if settings_button(config, "Read file", None, 88.0, !self.busy && !self.kubeconfig.trim().is_empty(), false).clicked() {
                            self.discover(false);
                        }
                        connection_field(context, "Context", &mut self.context, "Enter context name", !self.busy, false);
                    });
                });
                ui.add_space(28.0);
                settings_divider(ui);
                ui.add_space(17.0);
                ui.horizontal(|ui| {
                    if self.context.trim().is_empty() {
                        ui.colored_label(muted(), RichText::new("Enter a context name to connect.").size(13.0));
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let can_connect = !self.busy && !self.context.trim().is_empty() && !self.exe.trim().is_empty();
                        if settings_button(ui, "Connect cluster", None, 146.0, can_connect, true).clicked() {
                            let target = Target { exe: self.exe.trim().into(), config: self.kubeconfig.trim().into(), context: self.context.trim().into() };
                            if self.target.as_ref() == Some(&target) { self.start_fetch(); } else { self.switch(Some(target)); }
                            self.settings_page = None;
                        }
                    });
                });
            });
            if !self.notice.is_empty() { ui.add_space(8.0); ui.label(&self.notice); }
        });
    }
    fn console(&mut self, ctx: &egui::Context) {
        if !self.console { return; }
        egui::TopBottomPanel::bottom("console").resizable(true).default_height(245.0)
            .frame(egui::Frame::new().fill(theme::sidebar()).inner_margin(14))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Command panel").color(theme::sidebar_text()).strong());
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui_icons::button(ui, "close", "Close command panel", true, false).clicked() { self.console = false; }
                        if ui.small_button("Copy output").clicked() { ctx.copy_text(self.output.clone()); }
                    });
                });
                let run = ui.horizontal(|ui| {
                    ui.label(RichText::new("kubectl").color(theme::sidebar_text()).monospace());
                    let width = (ui.available_width() - 135.0).max(60.0);
                    let response = ui.add_sized([width, 27.0], egui::TextEdit::singleline(&mut self.command).font(egui::TextStyle::Monospace));
                    let enabled = !self.busy && self.confirm.is_none();
                    ui.add_enabled(enabled, primary_button("Run command")).clicked() ||
                        (enabled && response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                }).inner;
                if run {
                    if self.demo { self.output = "Connect a cluster to run kubectl commands.".into(); }
                    else {
                        let args: Vec<_> = self.command.split_whitespace().map(str::to_owned).collect();
                        if args.is_empty() { self.output = "Enter kubectl arguments, for example: get pods -A".into(); }
                        else { self.queue(args, None, OutputTo::Console); }
                    }
                }
                egui::Frame::new().fill(Color32::from_rgb(12, 24, 38)).corner_radius(5).inner_margin(10).show(ui, |ui| {
                    egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                        let output = if self.output.is_empty() && self.demo { "Connect a cluster to run kubectl commands." } else { &self.output };
                        ui.label(RichText::new(output).color(theme::sidebar_text()).monospace());
                    });
                });
            });
    }

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
    Field(TableField),
}

impl ResourceColumn {
    fn for_table(entry: &ResourceType, all_namespaces: bool) -> Vec<Self> {
        use ResourceColumn::*;
        use TableField as F;
        match entry.kind {
            "Pod" => vec![Name, Namespace, Status, Ready, Restarts, Node, Cpu, Memory, Containers, ControlledBy, Qos, Age],
            "Deployment" => vec![Name, Namespace, Ready, Field(F::Updated), Field(F::Available), Pods, Replicas, Age, Status],
            "Service" => vec![Name, Namespace, ServiceType, ClusterIp, Ports, ExternalIp, Selector, Age, Status],
            "Event" => vec![EventType, Field(F::Reason), Namespace, InvolvedObject, Message, Source, Count, Age, LastSeen],
            "Node" => vec![Name, Status, Field(F::NodeRole), Cpu, Memory, Field(F::PodCapacity)],
            "StatefulSet" => vec![Name, Namespace, Ready, Replicas, Status],
            "DaemonSet" | "ReplicaSet" => vec![Name, Namespace, Field(F::Desired), Field(F::ReadyCount), Status, Ready],
            "Job" => vec![Name, Namespace, Field(F::Completions), Status],
            "CronJob" => vec![Name, Namespace, Field(F::Schedule), Field(F::Suspend), Status],
            "CustomResourceDefinition" => vec![Name, Field(F::ApiGroup), Field(F::Scope), Field(F::ResourceKind), Status],
            "ConfigMap" => vec![Name, Namespace, Field(F::Keys), Status],
            "Secret" => vec![Name, Namespace, Field(F::SecretType), Field(F::Keys), Status],
            "HorizontalPodAutoscaler" => vec![Name, Namespace, Field(F::ScaleTarget), Field(F::ReplicaRange), Status],
            "Ingress" => vec![Name, Namespace, Field(F::IngressClass), Field(F::Hosts), Status],
            "NetworkPolicy" => vec![Name, Namespace, Field(F::PodSelector), Field(F::PolicyTypes), Status],
            "PersistentVolumeClaim" => vec![Name, Namespace, Status, Field(F::Requested)],
            "PersistentVolume" => vec![Name, Field(F::Capacity), Status, Field(F::StorageClass)],
            "StorageClass" => vec![Name, Field(F::Provisioner), Field(F::ReclaimPolicy), Field(F::BindingMode), Status],
            "ServiceAccount" => vec![Name, Namespace, Field(F::SecretReferences), Status],
            "Role" => vec![Name, Namespace, Field(F::Rules), Status],
            "RoleBinding" => vec![Name, Namespace, Field(F::RoleRef), Status],
            "ClusterRole" => vec![Name, Field(F::Rules), Status],
            "ClusterRoleBinding" => vec![Name, Field(F::RoleRef), Field(F::Subjects), Status],
            _ => {
                let mut columns = vec![Name];
                if entry.namespaced && all_namespaces { columns.push(Namespace); }
                columns.push(Status);
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
            Self::Field(field) => field.title(),
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
            Self::Field(field) => field.width(),
        }
    }

    fn default_width(self, kind: &str) -> f32 {
        match (kind, self) {
            ("Pod", Self::Name) => 260.0,
            ("Service", Self::Name) => 158.0,
            ("Pod", Self::Namespace) => 130.0,
            ("Service", Self::Namespace) => 106.0,
            ("Event", Self::Namespace) => 110.0,
            ("Pod", Self::Status) => 154.0,
            ("Service", Self::Status) => 105.0,
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
                    ("memory", resource.memory, "GiB")
                };
                let total = resource_quantity(&resource.raw["status"]["allocatable"][key], key)
                    .or_else(|| resource_quantity(&resource.raw["status"]["capacity"][key], key));
                let format_value = |value: Option<f64>, precision| {
                    value.map_or_else(|| "—".into(), |value| {
                        format!("{value:.precision$}")
                    })
                };
                let usage_precision = 1;
                let total_precision = if key == "cpu" && total.is_some_and(|value| value.fract() != 0.0) { 3 } else { 0 };
                format!("{} / {} {unit}", format_value(usage, usage_precision), format_value(total, total_precision))
            }
            Self::Cpu => resource.cpu.map(|cpu| {
                if resource.kind == "Pod" { format!("{:.0}m", cpu * 1000.0) } else { format!("{cpu:.3} cores") }
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
            Self::Field(field) => field.value(resource).unwrap_or_else(|| "—".into()),
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
            Self::Field(field) if field.is_count() => {
                let value = |resource: &Resource| field.value(resource)?.parse::<u64>().ok();
                compare_table_values(value(left), value(right), descending, Ord::cmp)
            }
            Self::Field(field) => compare_table_values(field.value(left), field.value(right), descending, Ord::cmp),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TableField {
    Updated, Available, Reason, NodeRole, PodCapacity, Desired, ReadyCount, Completions,
    Schedule, Suspend, ApiGroup, Scope, ResourceKind, Keys, SecretType, ScaleTarget, ReplicaRange,
    IngressClass, Hosts, PodSelector, PolicyTypes, Requested, Capacity, StorageClass,
    Provisioner, ReclaimPolicy, BindingMode, SecretReferences, Rules, RoleRef, Subjects,
}

impl TableField {
    fn title(self) -> &'static str {
        use TableField::*;
        match self {
            Updated => "Up to date", Available => "Available", Reason => "Reason", NodeRole => "Role",
            PodCapacity => "Pod capacity", Desired => "Desired", ReadyCount => "Ready",
            Completions => "Completions", Schedule => "Schedule", Suspend => "Suspend",
            ApiGroup => "Group", Scope => "Scope", ResourceKind => "Kind", Keys => "Keys",
            SecretType => "Type", ScaleTarget => "Reference", ReplicaRange => "Replicas",
            IngressClass => "Class", Hosts => "Hosts", PodSelector => "Pod selector", PolicyTypes => "Policy types",
            Requested => "Requested", Capacity => "Capacity", StorageClass => "Storage class",
            Provisioner => "Provisioner", ReclaimPolicy => "Reclaim policy", BindingMode => "Binding mode",
            SecretReferences => "Secrets", Rules => "Rules", RoleRef => "Role", Subjects => "Subjects",
        }
    }

    fn is_count(self) -> bool {
        matches!(self, Self::Updated | Self::Available | Self::PodCapacity | Self::Desired |
            Self::ReadyCount | Self::Keys | Self::SecretReferences | Self::Rules)
    }

    fn width(self) -> f32 {
        use TableField::*;
        match self {
            Updated | Available | PodCapacity | Desired | ReadyCount | Completions | Keys | SecretReferences | Rules => 104.0,
            ApiGroup | Hosts | PodSelector | Provisioner | Subjects => 180.0,
            _ => 126.0,
        }
    }

    fn value(self, resource: &Resource) -> Option<String> {
        use TableField::*;
        let raw = &resource.raw;
        let scalar = |path: &str| match raw.pointer(path)? {
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        };
        let len = |path: &str| raw.pointer(path)?.as_array().map(|values| values.len().to_string());
        let reference = |path: &str| {
            let item = raw.pointer(path)?;
            Some(format!("{}/{}", item["kind"].as_str()?, item["name"].as_str()?))
        };
        match self {
            Updated => scalar("/status/updatedReplicas"),
            Available => scalar("/status/availableReplicas"),
            Reason => scalar("/reason"),
            NodeRole => Some(if is_master(resource) { "control plane" } else { "worker" }.into()),
            PodCapacity => scalar("/status/allocatable/pods").or_else(|| scalar("/status/capacity/pods")),
            Desired => scalar(if resource.kind == "DaemonSet" { "/status/desiredNumberScheduled" } else { "/spec/replicas" }),
            ReadyCount => scalar(if resource.kind == "DaemonSet" { "/status/numberReady" } else { "/status/readyReplicas" }),
            Completions => Some(format!("{}/{}", num(raw, "/status/succeeded"), raw.pointer("/spec/completions").and_then(Value::as_u64).unwrap_or(1))),
            Schedule => scalar("/spec/schedule"),
            Suspend => Some(if raw["spec"]["suspend"] == true { "Yes" } else { "No" }.into()),
            ApiGroup => scalar("/spec/group"), Scope => scalar("/spec/scope"), ResourceKind => scalar("/spec/names/kind"),
            Keys => {
                if let Some(count) = raw["keyCount"].as_u64() { return Some(count.to_string()); }
                let data = raw["data"].as_object().map_or(0, |data| data.len());
                let binary = raw["binaryData"].as_object().map_or(0, |data| data.len());
                Some((data + binary).to_string())
            }
            SecretType => scalar("/type"),
            ScaleTarget => reference("/spec/scaleTargetRef"),
            ReplicaRange => Some(format!("{} / {}", scalar("/status/currentReplicas")?, scalar("/spec/maxReplicas")?)),
            IngressClass => scalar("/spec/ingressClassName").or_else(|| scalar("/metadata/annotations/kubernetes.io~1ingress.class")),
            Hosts => {
                let mut hosts: Vec<_> = raw.pointer("/spec/rules")?.as_array()?.iter().filter_map(|rule| rule["host"].as_str()).collect();
                hosts.sort_unstable();
                hosts.dedup();
                let first = hosts.first()?;
                Some(if hosts.len() == 1 { (*first).into() } else { format!("{first} + {} more", hosts.len() - 1) })
            }
            PodSelector => {
                let selector = raw.pointer("/spec/podSelector")?;
                let mut labels: Vec<_> = selector["matchLabels"].as_object().into_iter().flatten()
                    .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?))).collect();
                for requirement in selector["matchExpressions"].as_array().into_iter().flatten() {
                    let values = requirement["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
                    labels.push(format!("{} {} {}", requirement["key"].as_str().unwrap_or_default(),
                        requirement["operator"].as_str().unwrap_or_default(), values.join(",")));
                }
                Some(if labels.is_empty() { "All pods".into() } else { labels.join(", ") })
            }
            PolicyTypes => Some(raw.pointer("/spec/policyTypes")?.as_array()?.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            Requested => scalar("/spec/resources/requests/storage"), Capacity => scalar("/spec/capacity/storage"),
            StorageClass => scalar("/spec/storageClassName"), Provisioner => scalar("/provisioner"),
            ReclaimPolicy => scalar("/reclaimPolicy"), BindingMode => scalar("/volumeBindingMode"),
            SecretReferences => Some(len("/secrets").unwrap_or_else(|| "0".into())),
            Rules => len("/rules"),
            RoleRef => reference("/roleRef"),
            Subjects => {
                let subjects: Vec<_> = raw["subjects"].as_array()?.iter().filter_map(|item| {
                    let kind = item["kind"].as_str()?;
                    let name = item["name"].as_str()?;
                    Some(match item["namespace"].as_str() {
                        Some(namespace) => format!("{kind}: {namespace}/{name}"),
                        None => format!("{kind}: {name}"),
                    })
                }).collect();
                Some(subjects.join(", "))
            }
        }
    }
}

fn default_hidden_columns() -> HashSet<(&'static str, ResourceColumn)> {
    use ResourceColumn::*;
    [
        ("Pod", &[Memory, Containers, ControlledBy, Qos, Age][..]),
        ("Deployment", &[Pods, Replicas, Age, Status][..]),
        ("Service", &[ExternalIp, Selector, Age, Status][..]),
        ("Event", &[Source, Count, Age, LastSeen][..]),
        ("StatefulSet", &[Status][..]), ("DaemonSet", &[Status, Ready][..]), ("ReplicaSet", &[Status, Ready][..]),
        ("CronJob", &[Status][..]), ("ConfigMap", &[Status][..]), ("Secret", &[Status][..]),
        ("CustomResourceDefinition", &[Status][..]), ("HorizontalPodAutoscaler", &[Status][..]),
        ("Ingress", &[Status][..]), ("NetworkPolicy", &[Status][..]), ("StorageClass", &[Status][..]),
        ("ServiceAccount", &[Status][..]), ("Role", &[Status][..]), ("RoleBinding", &[Status][..]),
        ("ClusterRole", &[Status][..]), ("ClusterRoleBinding", &[Status][..]),
    ].into_iter().flat_map(|(kind, columns)| columns.iter().map(move |&column| (kind, column))).collect()
}

fn app_config_dir() -> Option<PathBuf> {
    let home_dir = || std::env::var_os("HOME").filter(|path| !path.is_empty()).map(PathBuf::from);
    let base_dir = if cfg!(target_os = "macos") {
        home_dir().map(|dir| dir.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").filter(|path| !path.is_empty()).map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()).map(PathBuf::from)
            .or_else(|| home_dir().map(|dir| dir.join(".config")))
    };
    base_dir.map(|dir| dir.join("Cluster Observatory"))
}

fn table_width_file(dir: &std::path::Path, kind: &str, column: ResourceColumn) -> PathBuf {
    match column {
        ResourceColumn::Field(field) => dir.join(format!("{kind}.field.{field:?}")),
        _ => dir.join(format!("{kind}.{}", column.title())),
    }
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
                ResourceColumn::Ready if entry.kind == "Pod" => read(ResourceColumn::Containers),
                ResourceColumn::Ready if entry.kind == "Deployment" => read(ResourceColumn::Pods),
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
    let temporary = path.with_extension(format!("{}.{}.tmp", path.extension().and_then(|value| value.to_str()).unwrap_or("width"), std::process::id()));
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
    let status = !header && matches!(column, ResourceColumn::Status | ResourceColumn::EventType);
    let name = !header && matches!(column, ResourceColumn::Name);
    let numeric = matches!(column, ResourceColumn::Ready | ResourceColumn::Cpu | ResourceColumn::Memory
        | ResourceColumn::Containers | ResourceColumn::Restarts | ResourceColumn::Age | ResourceColumn::Pods | ResourceColumn::Replicas | ResourceColumn::Count);
    let color = if header { muted() } else if status { status_color(&label) } else { accent.unwrap_or(if name { text_color() } else { muted() }) };
    let font = if header { theme::semibold(12.0) }
        else if numeric { FontId::monospace(12.0) }
        else { FontId::proportional(if name || column == ResourceColumn::Namespace { 13.0 } else { 12.0 }) };

    let mut job = egui::text::LayoutJob::simple_singleline(label, font, color);
    if column == ResourceColumn::InvolvedObject && accent.is_some() {
        job.sections[0].format.underline = Stroke::new(1.0_f32, color);
    }
    let padding = if status { 36.0 } else { 16.0 };
    job.wrap.max_width = (cell.width() - padding - 8.0).max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    let x = cell.left() + padding;
    let position = Pos2::new(x, cell.center().y - galley.size().y / 2.0);
    let painter = painter.with_clip_rect(cell);
    if status {
        let pill = Rect::from_min_size(position, galley.size()).expand2(Vec2::new(8.0, 3.0));
        let pill = Rect::from_min_max(pill.min - Vec2::new(5.0, 0.0), pill.max);
        painter.rect_filled(pill, 5.0, if color == green() { theme::success_fill() } else if color == orange() { theme::warning_fill() } else { side() });
        painter.circle_filled(egui::pos2(pill.left() + 5.0, pill.center().y), 2.0, color);
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
        r#"go-template={{range .items}}{{.metadata.name}}{{"\t"}}{{.metadata.namespace}}{{"\t"}}{{.metadata.uid}}{{"\t"}}{{or .type "Opaque"}}{{"\t"}}{{if .data}}{{len .data}}{{else}}0{{end}}{{"\n"}}{{end}}"#
    } else { "json" };
    let base = words(&["get", entry.api, "-o", format]);
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
            if let [name, namespace, uid, kind, keys] = fields.as_slice() {
                let keys = keys.parse::<u64>().map_err(|_| "Unexpected Secret key count".to_owned())?;
                Ok(json!({"apiVersion":"v1", "metadata":{"name": name, "namespace": namespace, "uid": uid}, "type":kind, "keyCount":keys}))
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

#[derive(Clone, Copy)]
enum ResourceAction {
    Details, Logs, Edit, Delete, Evict, CopyReference,
    #[cfg(target_os = "macos")]
    Shell,
    #[cfg(target_os = "macos")]
    Attach,
}

fn row_menu(ui: &mut egui::Ui, resource: &Resource, connected: bool) -> Option<ResourceAction> {
    ui.set_min_width(148.0);
    let pod = resource.kind == "Pod";
    let mut result = None;
    let mut item = |ui: &mut egui::Ui, label, enabled, action| {
        if ui.add_enabled(enabled, egui::Button::new(label).frame(false)).clicked() { result = Some(action); }
    };
    item(ui, "Show Details", true, ResourceAction::Details);
    if pod {
        #[cfg(target_os = "macos")]
        {
            let terminal = connected && !containers(resource).is_empty();
            item(ui, "Attach Pod", terminal, ResourceAction::Attach);
            item(ui, "Shell", terminal, ResourceAction::Shell);
        }
        item(ui, "Evict…", connected, ResourceAction::Evict);
        ui.menu_button("Share", |ui| item(ui, "Copy resource reference", true, ResourceAction::CopyReference));
        item(ui, "Logs", true, ResourceAction::Logs);
    }
    item(ui, if resource.kind == "Event" { "View YAML…" } else { "Edit YAML…" }, connected, ResourceAction::Edit);
    if resource.kind != "Event" { item(ui, "Delete…", connected, ResourceAction::Delete); }
    result
}

fn eviction_request(pod: &Resource) -> (Vec<String>, String) {
    let path = format!("/api/v1/namespaces/{}/pods/{}/eviction", pod.namespace, pod.name);
    let mut request = json!({"apiVersion":"policy/v1","kind":"Eviction","metadata":{"name":pod.name,"namespace":pod.namespace}});
    if let Some(uid) = pod.raw.pointer("/metadata/uid").and_then(Value::as_str) {
        request["deleteOptions"] = json!({"preconditions":{"uid":uid}});
    }
    (words(&["create", "--raw", &path, "-f", "-"]), request.to_string())
}

fn pod_args(verb: &str, resource: &Resource, container: &str, extra: &[&str]) -> Vec<String> {
    let mut args = words(&[verb, &resource.name, "-n", &resource.namespace, "-c", container]);
    args.extend(words(extra));
    args
}

#[cfg(target_os = "macos")]
fn pod_terminal_args(pod: &Resource, container: &str, attach: bool) -> Vec<String> {
    if !attach { return pod_args("exec", pod, container, &["-it", "--", "sh"]); }
    let mut args = pod_args("attach", pod, container, &[]);
    let spec = ["containers", "initContainers", "ephemeralContainers"].into_iter()
        .filter_map(|kind| pod.raw["spec"][kind].as_array()).flatten()
        .find(|spec| spec["name"] == container);
    if let Some(spec) = spec.filter(|spec| spec["stdin"] == true) {
        args.push("-i".into());
        if spec["tty"] == true { args.push("-t".into()); }
    }
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

fn default_container(pod: &Resource) -> String {
    let names = containers(pod);
    if let Some(name) = pod.raw["metadata"]["annotations"]["kubectl.kubernetes.io/default-container"].as_str() {
        if names.iter().any(|container| container == name) { return name.to_owned(); }
    }
    names.into_iter().next().unwrap_or_default()
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
    manifest_from_resource(original)
}

fn manifest_from_resource(original: Value) -> Result<Manifest, String> {
    let yaml = serde_yaml_ng::to_string(&original).map_err(|error| error.to_string())?;
    Ok(Manifest { yaml, original: Some(original), feedback: String::new() })
}

fn edited_manifest(yaml: &str, original: &Value) -> Result<Value, String> {
    let edited: Value = serde_yaml_ng::from_str(yaml).map_err(|error| error.to_string())?;
    if !same_object(original, &edited) || original["apiVersion"] != edited["apiVersion"] {
        return Err("Keep the resource's API version, kind, name, namespace and UID.".into());
    }
    Ok(edited)
}

fn manifest_request(manifest: &Manifest, namespace: &str) -> Result<(Vec<String>, String), String> {
    if let Some(original) = &manifest.original {
        let edited = edited_manifest(&manifest.yaml, original)?;
        let version = text(original, "/metadata/resourceVersion");
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
// The resource's own scope is shown independently of the table's filter.
fn sidebar_divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline((rect.left() + 18.0)..=(rect.right() - 18.0), rect.center().y,
        Stroke::new(1.0_f32, theme::sidebar_divider()));
}

// A full-width navigator row keeps text, icons and counts in fixed columns.
fn nav_row(ui:&mut egui::Ui,label:&str,icon:&str,depth:usize,selected:bool,count:Option<usize>,branch:Option<bool>)->egui::Response{
    let height = if depth == 1 { 35.0 } else if depth == 2 { 29.0 } else { 34.0 };
    let(rect,response)=ui.allocate_exact_size(Vec2::new(ui.available_width(),height),Sense::click());
    let painter=ui.painter();
    if selected||response.hovered()||response.has_focus(){
        painter.rect_filled(rect.shrink2(Vec2::new(10.0, 2.0)), 5.0, if selected { theme::sidebar_selected() } else { theme::sidebar_divider() });
    }
    let color=if selected{Color32::WHITE}else{theme::sidebar_text()};
    let left = rect.left() + if depth == 0 { 14.0 } else if depth == 1 { 14.0 } else { 37.0 };
    if let Some(open) = branch.filter(|_| icon != "cluster-root" || response.hovered() || response.has_focus()) {
        let center = Pos2::new(rect.right() - 32.0, rect.center().y);
        let points=if open{vec![center+Vec2::new(-3.0,-2.0),center+Vec2::new(0.0,1.0),center+Vec2::new(3.0,-2.0)]}
            else{vec![center+Vec2::new(-1.0,-3.0),center+Vec2::new(2.0,0.0),center+Vec2::new(-1.0,3.0)]};
        painter.add(egui::Shape::line(points,Stroke::new(1.3_f32,color)));
    }
    if depth < 2 { nav_icon(painter, Pos2::new(left + 15.0, rect.center().y), icon, color); }
    let text_start = left + if depth < 2 { 32.0 } else { 10.0 };
    let trailing = if count.is_some() || branch.is_some() { 37.0 } else { 12.0 };
    let text_rect = Rect::from_min_max(Pos2::new(text_start, rect.top()), Pos2::new(rect.right() - trailing, rect.bottom()));
    let font_size = if depth == 2 { 12.0 } else { 13.0 };
    let font = if selected || branch.is_some() { theme::semibold(font_size) } else { FontId::proportional(font_size) };
    let mut title = egui::text::LayoutJob::simple_singleline(label.to_owned(), font, color);
    title.wrap.max_width = text_rect.width().max(1.0);
    title.wrap.max_rows = 1;
    title.wrap.break_anywhere = true;
    let title = painter.layout_job(title);
    let title_pos = text_rect.left_center() - Vec2::new(0.0, title.size().y / 2.0);
    painter.with_clip_rect(text_rect).galley(title_pos, title, color);
    if let Some(count)=count{
        painter.text(Pos2::new(rect.right()-24.0,rect.center().y),Align2::RIGHT_CENTER,count.to_string(),FontId::proportional(11.0),theme::sidebar_text());
    }
    response.widget_info(||egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel,true,selected,label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
pub(crate) fn nav_icon(p:&egui::Painter,c:Pos2,kind:&str,color:Color32){
    let stroke=Stroke::new(1.25_f32,color);
    let line=|a:Vec2,b:Vec2|{p.line_segment([c+a,c+b],stroke);};
    let box_at=|x:f32,y:f32,w:f32,h:f32|{p.rect_stroke(Rect::from_min_size(c+Vec2::new(x,y),Vec2::new(w,h)),1.0,stroke,egui::StrokeKind::Inside);};
    match kind{
        "overview"=>{for x in [-6.0,1.0]{for y in [-6.0,1.0]{box_at(x,y,5.0,5.0);}}}
        "nodes"=>{for y in [-6.0,1.0]{box_at(-7.0,y,14.0,5.0);p.circle_filled(c+Vec2::new(4.0,y+2.5),0.8,color);}}
        "workloads"|"pod"|"cluster"|"cluster-root"=>{
            let points:Vec<_>=(0..6).map(|i|{let a=std::f32::consts::TAU*i as f32/6.0;c+Vec2::new(a.sin()*7.0,a.cos()*7.0)}).collect();
            p.add(egui::Shape::closed_line(points,stroke));
            if kind!="pod"{for i in 0..3{let a=std::f32::consts::TAU*i as f32/3.0;line(Vec2::ZERO,Vec2::new(a.sin()*7.0,a.cos()*7.0));}}
        }
        "config"=>{for(y,x)in [(-4.0,-2.0),(1.0,3.0),(6.0,-3.0)]{line(Vec2::new(-7.0,y),Vec2::new(7.0,y));p.circle_filled(c+Vec2::new(x,y),2.0,side());p.circle_stroke(c+Vec2::new(x,y),2.0,stroke);}}
        "network"|"topology"=>{box_at(-2.5,-7.0,5.0,5.0);line(Vec2::new(0.0,-2.0),Vec2::new(0.0,2.0));line(Vec2::new(-5.0,2.0),Vec2::new(5.0,2.0));for x in [-5.0,5.0]{box_at(x-2.5,3.0,5.0,5.0);}}
        "layers" => {
            p.add(egui::Shape::closed_line(vec![c + Vec2::new(0.0, -6.0), c + Vec2::new(7.0, -2.0),
                c + Vec2::new(0.0, 2.0), c + Vec2::new(-7.0, -2.0)], stroke));
            for y in [1.0, 4.0] {
                p.line(vec![c + Vec2::new(-7.0, y), c + Vec2::new(0.0, y + 4.0), c + Vec2::new(7.0, y)], stroke);
            }
        }
        "lock" => {
            box_at(-5.0, -1.0, 10.0, 8.0);
            p.line(vec![c + Vec2::new(-3.0, -1.0), c + Vec2::new(-3.0, -5.0), c + Vec2::new(-1.5, -7.0),
                c + Vec2::new(1.5, -7.0), c + Vec2::new(3.0, -5.0), c + Vec2::new(3.0, -1.0)], stroke);
            line(Vec2::new(0.0, 2.0), Vec2::new(0.0, 4.0));
        }
        "plug" => {
            p.line(vec![c + Vec2::new(-4.0, -2.0), c + Vec2::new(-4.0, 1.0), c + Vec2::new(-2.0, 4.0),
                c + Vec2::new(2.0, 4.0), c + Vec2::new(4.0, 1.0), c + Vec2::new(4.0, -2.0)], stroke);
            line(Vec2::new(-5.5, -2.0), Vec2::new(5.5, -2.0));
            for x in [-2.0, 2.0] { line(Vec2::new(x, -6.0), Vec2::new(x, -2.0)); }
            line(Vec2::new(0.0, 4.0), Vec2::new(0.0, 8.0));
        }
        "storage" => {
            p.add(egui::Shape::ellipse_stroke(c + Vec2::new(0.0, -4.0), Vec2::new(6.0, 2.5), stroke));
            for x in [-6.0, 6.0] { line(Vec2::new(x, -4.0), Vec2::new(x, 4.0)); }
            for y in [0.0, 4.0] {
                p.line(vec![c + Vec2::new(-6.0, y), c + Vec2::new(-4.0, y + 1.8), c + Vec2::new(0.0, y + 2.5),
                    c + Vec2::new(4.0, y + 1.8), c + Vec2::new(6.0, y)], stroke);
            }
        }
        "namespace"=>{box_at(-6.0,-6.0,12.0,12.0);line(Vec2::new(-2.0,0.0),Vec2::new(2.0,0.0));line(Vec2::new(0.0,-2.0),Vec2::new(0.0,2.0));}
        "update" => {
            line(Vec2::new(0.0, -7.0), Vec2::new(0.0, 2.0));
            p.line(vec![c + Vec2::new(-3.5, -1.5), c + Vec2::new(0.0, 2.0), c + Vec2::new(3.5, -1.5)], stroke);
            p.line(vec![c + Vec2::new(-6.0, 3.0), c + Vec2::new(-6.0, 6.0), c + Vec2::new(6.0, 6.0), c + Vec2::new(6.0, 3.0)], stroke);
        }
        "warning" => {
            p.add(egui::Shape::closed_line(vec![c + Vec2::new(0.0,-6.0), c + Vec2::new(6.5,5.5), c + Vec2::new(-6.5,5.5)], stroke));
            line(Vec2::new(0.0,-2.3),Vec2::new(0.0,1.0));
            p.circle_filled(c + Vec2::new(0.0,3.0), 0.7, color);
        }
        "search" => { p.circle_stroke(c + Vec2::new(-1.0,-1.0), 4.5, stroke); line(Vec2::new(2.5,2.5),Vec2::new(6.0,6.0)); }
        "chevron" => { p.line(vec![c + Vec2::new(-3.5,-1.5), c + Vec2::new(0.0,2.0), c + Vec2::new(3.5,-1.5)], stroke); }
        "refresh" => {
            p.line((0..=18).map(|i| {
                let angle = (0.0_f32 + i as f32 * 17.0).to_radians();
                c + Vec2::new(angle.cos() * 5.4, angle.sin() * 5.4)
            }).collect(), stroke);
            p.line(vec![c + Vec2::new(5.4,-5.5), c + Vec2::new(5.4,0.0), c + Vec2::new(0.5,0.0)], stroke);
        }
        "file" | "changelog" => {
            box_at(-5.0, -7.0, 10.0, 14.0);
            for y in [-3.0, 0.0, 3.0] {
                line(Vec2::new(-2.0, y), Vec2::new(2.0, y));
            }
        }
        "events"=>{p.circle_stroke(c,7.0,stroke);line(Vec2::new(0.0,-4.0),Vec2::new(0.0,0.0));line(Vec2::ZERO,Vec2::new(3.0,2.0));}
        "terminal"=>{line(Vec2::new(-6.0,-4.0),Vec2::new(-2.0,0.0));line(Vec2::new(-2.0,0.0),Vec2::new(-6.0,4.0));line(Vec2::new(0.0,5.0),Vec2::new(6.0,5.0));}
        _=>{}
    }
}
fn workspace_title(ui: &mut egui::Ui, title: &str) {
    let width = (ui.available_width() - 205.0).max(90.0);
    ui.allocate_ui_with_layout(Vec2::new(width, 43.0), egui::Layout::left_to_right(Align::Center), |ui| {
        ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(32.0)).extra_letter_spacing(-0.8)).truncate());
    });
}

fn settings_panel() -> egui::Frame {
    egui::Frame::new().fill(panel()).stroke(Stroke::new(1.0_f32, border()))
        .corner_radius(7).inner_margin(19)
}

fn settings_divider(ui: &mut egui::Ui) {
    let (line, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(line.x_range(), line.center().y, Stroke::new(1.0_f32, border()));
}

fn settings_button(ui: &mut egui::Ui, label: &str, icon: Option<&str>, width: f32, enabled: bool, primary: bool) -> egui::Response {
    chrome_button(ui, label, icon, width, enabled, primary, false)
}

fn chrome_button(ui: &mut egui::Ui, label: &str, icon: Option<&str>, width: f32, enabled: bool, primary: bool, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 34.0), if enabled { Sense::click() } else { Sense::hover() });
    paint_chrome_button(ui, rect, &response, label, icon, enabled, primary, active);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
}

fn paint_chrome_button(ui: &egui::Ui, rect: Rect, response: &egui::Response, label: &str, icon: Option<&str>, enabled: bool, primary: bool, active: bool) {
    let color = if !enabled { muted() } else if primary { theme::primary_text() } else if active { blue() } else { text_color() };
    let fill = if !enabled && primary { theme::disabled_control() } else if primary { theme::primary() }
        else if active || enabled && response.hovered() { theme::selection() } else if !enabled { side() } else { panel() };
    let outline = if enabled && response.has_focus() { blue() } else if primary || active { fill } else { border() };
    ui.painter().rect(rect, 6.0, fill, Stroke::new(1.0_f32, outline), egui::StrokeKind::Inside);
    let mut x = rect.left() + 12.0;
    let y = rect.center().y;
    if let Some(icon) = icon {
        nav_icon(ui.painter(), Pos2::new(x + 6.0, y), icon, color);
        x += 19.0;
    }
    ui.painter().text(Pos2::new(x, y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
}

fn connection_field(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, enabled: bool, mono: bool) -> egui::Response {
    let label = ui.label(RichText::new(label).size(13.0));
    ui.add_space(10.0);
    ui.scope(|ui| {
        ui.visuals_mut().extreme_bg_color = side();
        let font = if mono { FontId::monospace(13.0) } else { FontId::proportional(13.0) };
        let edit = egui::TextEdit::singleline(text).hint_text(RichText::new(hint).color(muted()))
            .font(font).margin(egui::Margin::symmetric(12, 9)).desired_width(f32::INFINITY).interactive(enabled);
        ui.add_sized([ui.available_width(), 38.0], edit).labelled_by(label.id)
    }).inner
}

fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(theme::primary_text())).fill(theme::primary())
}

fn appearance_preview(ui: &mut egui::Ui, dark: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 199.0), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    theme::paint(if dark { egui::Theme::Dark } else { egui::Theme::Light }, || {
        painter.rect_filled(rect, 5.0, bg());
        let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 31.0));
        painter.rect_filled(header, 3.0, panel());
        painter.text(rect.min + Vec2::new(12.0, 15.0), Align2::LEFT_CENTER, "example-production", FontId::proportional(9.0), text_color());
        let side_width = rect.width() * 0.23;
        let sidebar = Rect::from_min_max(egui::pos2(rect.left(), header.bottom()), egui::pos2(rect.left() + side_width, rect.bottom()));
        painter.rect_filled(sidebar, 1.0, theme::sidebar());
        for (index, label) in ["Pods", "Events", "Topology"].into_iter().enumerate() {
            let y = sidebar.top() + 24.0 + index as f32 * 24.0;
            if index == 0 {
                painter.rect_filled(Rect::from_min_size(egui::pos2(sidebar.left() + 4.0, y - 7.0), Vec2::new(side_width - 8.0, 14.0)), 3.0, theme::sidebar_selected());
            }
            painter.text(egui::pos2(sidebar.left() + 9.0, y), Align2::LEFT_CENTER, label, FontId::proportional(9.0), theme::sidebar_text());
        }
        let x = sidebar.right() + 10.0;
        let width = (rect.right() - x - 10.0).max(1.0);
        painter.text(egui::pos2(x, header.bottom() + 19.0), Align2::LEFT_CENTER, "Pods", FontId::proportional(15.0), text_color());
        painter.rect_filled(Rect::from_min_size(egui::pos2(x, header.bottom() + 38.0), Vec2::new(width, 23.0)), 3.0, theme::warning_fill());
        painter.text(egui::pos2(x + 5.0, header.bottom() + 50.0), Align2::LEFT_CENTER, "1 pod needs attention", FontId::proportional(8.0), orange());
        for (index, (name, status)) in [("Name", "Status"), ("payments-7d8f6c-1k4xz", "CrashLoopBackOff"), ("redis-7d8f6c-0k4xz", "Running")].into_iter().enumerate() {
            let y = header.bottom() + 70.0 + index as f32 * 24.0;
            painter.rect_filled(Rect::from_min_size(egui::pos2(x, y), Vec2::new(width, 24.0)), 2.0, if index == 1 { theme::selection() } else { panel() });
            painter.text(egui::pos2(x + 5.0, y + 12.0), Align2::LEFT_CENTER, name, FontId::proportional(8.0), text_color());
            painter.text(egui::pos2(x + width * 0.55, y + 12.0), Align2::LEFT_CENTER, status, FontId::proportional(8.0), if index == 1 { orange() } else { text_color() });
        }
    });
}

fn card<R>(ui:&mut egui::Ui,body:impl FnOnce(&mut egui::Ui)->R)->R{egui::Frame::new().fill(panel()).stroke(Stroke::new(1.0_f32, border())).corner_radius(7).inner_margin(16).show(ui,|ui|{ui.set_min_width(ui.available_width());body(ui)}).inner}
fn badge(ui:&mut egui::Ui,label:&str,color:Color32){egui::Frame::new().fill(theme::tint(color, 24)).corner_radius(4).inner_margin(egui::Margin::symmetric(7,3)).show(ui,|ui|{ui.label(RichText::new(label).color(color).size(10.0).strong());});}
fn legend(ui:&mut egui::Ui,color:Color32,label:&str){ui.colored_label(color,"●");ui.label(RichText::new(label).color(muted()).size(12.0));}
impl eframe::App for Observatory {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_pending_table_width();
    }

    fn update(&mut self,ctx:&egui::Context,_frame:&mut eframe::Frame) {
        let resolved = self.appearance.resolve(ctx.system_theme());
        if self.rendered_theme != Some(resolved) {
            theme::apply(ctx, resolved);
            self.rendered_theme = Some(resolved);
        }
        theme::paint(resolved, || self.draw(ctx));
    }
}

impl Observatory {
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll();
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.settings_page = if self.settings_page.is_some() { None } else { Some(SettingsPage::Appearance) };
        }
        if self.shot.is_none() && !self.busy && self.confirm.is_none() && self.manifest.is_none() && self.kubeconfig_settings.sync_due() {
            self.discover_source(String::new(), false);
        }
        #[cfg(target_os = "macos")]
        {
            self.updates.poll(ctx);
            self.updates.check_in_background(ctx);
        }
        if self.shot.is_none()&&!self.demo&&!self.busy&&self.confirm.is_none()&&self.last_refresh.elapsed()>=Duration::from_secs(REFRESH){self.start_fetch();}
        self.top(ctx);
        #[cfg(target_os = "macos")]
        self.process_usage.poll(ctx);
        egui::TopBottomPanel::bottom("status")
            .exact_height(32.0)
            .frame(egui::Frame::new().fill(panel()).inner_margin(egui::Margin { left: 20, right: 28, top: 2, bottom: 2 }))
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    #[cfg(target_os = "macos")]
                    {
                        if self.updates.footer(ui) { self.settings_page = Some(SettingsPage::Updates); }
                        ui.label(RichText::new(self.process_usage.label()).monospace().size(11.0).color(muted()))
                            .on_hover_text("This app process. CPU is a recent average (100% is one core). RAM is resident memory.");
                    }
                    #[cfg(not(target_os = "macos"))]
                    ui.colored_label(muted(), RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).size(12.0));
                    ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::new(7.0, 11.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 2.8, if self.demo { blue() } else { green() });
                        let workspace = if self.demo { "Sample workspace" }
                            else if self.snapshot.loaded > 0 { "Cluster workspace" }
                            else { "Local kubeconfigs" };
                        ui.label(RichText::new(workspace).size(12.0).color(muted()));
                        if self.demo {
                            ui.add_space((256.0 - ui.cursor().left() - ui.spacing().item_spacing.x).max(0.0));
                            ui.label(RichText::new("Illustrative cluster data").size(12.0).color(muted()));
                        }
                        if !self.notice.is_empty() {
                            ui.separator();
                            ui.add(egui::Label::new(RichText::new(&self.notice).size(11.0).color(muted())).truncate())
                                .on_hover_text(&self.notice);
                        }
                    });
                });
            });
        self.sidebar(ctx);
        if self.settings_page.is_none() && self.manifest.is_none() && self.confirm.is_none() {
            self.console(ctx);
            self.log_dock(ctx);
            self.details(ctx);
        }
        self.env_secrets.end_frame();
        if self.settings_page != Some(SettingsPage::Kubernetes) {
            self.kubeconfig_settings.clear_draft();
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(bg()).inner_margin(egui::Margin { left: 28, right: 24, top: 24, bottom: 10 })).show(ctx,|ui|{
            if let Some(action) = self.confirm.clone() {
                self.confirm_cluster_action(ui, action);
            } else if let Some(page) = self.settings_page {
                self.settings(ui, page);
            } else if self.manifest.is_some() {
                self.manifest_editor(ui);
            } else {
                self.workspace(ui);
            }
        });
        #[cfg(target_os = "macos")]
        self.updates.dialog(ctx);
        if !ctx.input(|input| input.pointer.primary_down()) {
            self.save_pending_table_width();
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
                && self.settings_page.is_none() && !self.console && self.pod_logs.is_none()
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
    #[cfg(target_os = "macos")]
    if let Ok(executable) = std::env::current_exe() {
        if let Some(bundle) = executable.ancestors().nth(3).filter(|path| path.extension().is_some_and(|ext| ext == "app")) {
            let bundled = bundle.join("Contents/MacOS/kubectl");
            if let Some(path) = bundled.to_str().filter(|_| bundled.is_file()) {
                return path.into();
            }
        }
    }
    if std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("kubectl").is_file())) {
        return "kubectl".into();
    }
    ["/opt/homebrew/bin/kubectl", "/usr/local/bin/kubectl"]
        .into_iter().find(|path| std::path::Path::new(path).is_file()).unwrap_or("kubectl").into()
}

fn main()->eframe::Result {
    let mut arguments = std::env::args().skip(1);
    let mut shot = None;
    let mut screenshot_view = None;
    let mut screenshot_theme = None;
    let mut sample = false;
    let default_exe = default_kubectl();
    let mut target = Target { exe: default_exe.clone(), ..Default::default() };
    while let Some(flag) = arguments.next() {
        if flag == "--sample" { sample = true; continue; }
        if !matches!(flag.as_str(), "--screenshot" | "--screenshot-view" | "--screenshot-theme" | "--kubeconfig" | "--context" | "--kubectl") {
            eprintln!("Unknown option {flag}");
            std::process::exit(2);
        }
        let value = arguments.next().filter(|arg| !arg.is_empty() && !arg.starts_with("--")).unwrap_or_else(|| {
            eprintln!("{flag} requires a value");
            std::process::exit(2);
        });
        match flag.as_str() {
            "--screenshot" => shot = Some(PathBuf::from(value)),
            "--screenshot-view" => {
                let known = matches!(value.as_str(), "overview" | "pod" | "pod-search" | "pod-events" | "pod-configuration" | "start-port-forward" |
                    "logs" | "logs-expanded" | "command" | "appearance" | "kubernetes" | "connections" | "updates" |
                    "topology" | "traffic" | "workloads" | "config" | "network" | "storage" | "access-control" |
                    "port-forwarding" | "ingress") ||
                    CATALOG.iter().any(|entry| entry.page.to_ascii_lowercase().replace(' ', "-") == value.trim_end_matches("-details"));
                if !known {
                    eprintln!("Unknown screenshot view: {value}");
                    std::process::exit(2);
                }
                screenshot_view = Some(value);
            }
            "--screenshot-theme" => screenshot_theme = Some(match value.as_str() {
                "system" => theme::Preference::System,
                "light" => theme::Preference::Light,
                "dark" => theme::Preference::Dark,
                _ => { eprintln!("Screenshot theme must be system, light, or dark"); std::process::exit(2); }
            }),
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
    if sample && !target.context.is_empty() {
        eprintln!("Use either --sample or --context.");
        std::process::exit(2);
    }
    if (shot.is_none() && screenshot_theme.is_some()) || (screenshot_view.is_some() && (shot.is_none() || !target.context.is_empty())) {
        eprintln!("Screenshot options require --screenshot; --screenshot-view requires sample data.");
        std::process::exit(2);
    }
    let target = (!target.context.is_empty()).then_some(target);
    let icon = image::load_from_memory_with_format(
        include_bytes!("../assets/app-icon.png"),
        image::ImageFormat::Png,
    )
    .expect("valid bundled app icon")
    .resize(256, 256, image::imageops::FilterType::Triangle)
    .into_rgba8();
    let (width, height) = icon.dimensions();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cluster Observatory")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1080.0, 720.0])
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };
    eframe::run_native("Cluster Observatory", options, Box::new(move |cc| {
        let mut app = Observatory::new(cc, shot, target, sample);
        if let Some(preference) = screenshot_theme { app.appearance = preference; }
        if let Some(view) = screenshot_view { app.prepare_screenshot(&view); }
        Ok(Box::new(app))
    }))
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn kubernetes_quantities(){assert!((quantity("250m",false)-0.25).abs()<1e-9);assert!((quantity("500000000n",false)-0.5).abs()<1e-9);assert!((quantity("1048576Ki",true)-1.0).abs()<1e-9);}
    #[test]
    fn every_resource_page_has_inspectable_sample_data() {
        let snapshot = demo();
        for entry in CATALOG {
            assert!(!snapshot.kind(entry.kind).is_empty(), "missing sample for {}", entry.page);
        }
        let ingress = snapshot.kind("Ingress")[0];
        assert!(array(&ingress.raw, "/spec/rules").len() > 1);
        assert!(array(&ingress.raw, "/spec/rules/0/http/paths").len() > 1);
        assert!(snapshot.kind("Secret")[0].raw["data"].as_object().unwrap().len() >= 10);
    }

    #[test]
    fn resource_rows_open_details_and_the_pod_action_menu_works_in_sample_mode() {
        let ctx = egui::Context::default();
        let creation = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = Observatory::new(&creation, Some(PathBuf::new()), None, false);
        let inspector = std::cell::Cell::new(false);
        let frame = |app: &mut Observatory, events| {
            let width = if inspector.get() { 1000.0 } else { 1100.0 };
            let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 650.0));
            ctx.run(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ctx| {
                if inspector.get() { app.details(ctx); }
                egui::CentralPanel::default().show(ctx, |ui| app.resources(ui));
            })
        };
        let text_pos = |output: &egui::FullOutput, label: &str| {
            output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() * 0.5),
                _ => None,
            })
        };
        let press = |pos, pressed| egui::Event::PointerButton {
            pos, pressed, button: egui::PointerButton::Primary, modifiers: egui::Modifiers::NONE,
        };
        let click = |app: &mut Observatory, pos| {
            frame(app, vec![egui::Event::PointerMoved(pos), press(pos, true)]);
            frame(app, vec![press(pos, false)])
        };
        app.page = "Deployments".into();
        let deployment = text_pos(&frame(&mut app, vec![]), "gateway").expect("gateway row");
        click(&mut app, deployment);
        assert!(app.selected.as_ref().is_some_and(|resource| resource.kind == "Deployment" && resource.name == "gateway"));

        app.page = "Pods".into();
        app.selected = None;
        let open_actions = |app: &mut Observatory| {
            let output = frame(app, vec![]);
            let pod = text_pos(&output, "gateway-7d8f6c-0k4xz").expect("gateway Pod row");
            let actions = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Circle(dot) if (dot.radius - 1.15).abs() < 0.01 && (dot.center.y - pod.y).abs() < 2.0 => Some(dot.center),
                _ => None,
            }).expect("visible row action menu");
            let header = text_pos(&output, "Actions").expect("Actions column header");
            assert!((header.x - actions.x).abs() < 1.0);
            click(app, actions);
        };
        open_actions(&mut app);
        let logs = text_pos(&frame(&mut app, vec![]), "Logs").expect("Logs item in the Pod row menu");
        click(&mut app, logs);
        assert!(app.pod_logs.as_ref().is_some_and(|logs| logs.pod.name == "gateway-7d8f6c-0k4xz" && logs.container == "gateway"));

        let choose = |app: &mut Observatory, label| {
            open_actions(app);
            let pos = text_pos(&frame(app, vec![]), label).expect(label);
            click(app, pos)
        };
        #[cfg(target_os = "macos")]
        {
            choose(&mut app, "Shell");
            assert!(app.console && app.output.contains("exec gateway-7d8f6c-0k4xz -n production -c gateway -it -- sh"));
            choose(&mut app, "Attach Pod");
            assert!(app.console && app.output.contains("attach gateway-7d8f6c-0k4xz -n production -c gateway"));
        }
        choose(&mut app, "Share");
        let copy = text_pos(&frame(&mut app, vec![]), "Copy resource reference").expect("Share submenu");
        let output = click(&mut app, copy);
        assert!(output.platform_output.commands.iter().any(|command| matches!(command,
            egui::OutputCommand::CopyText(reference) if reference == "production/Pod/gateway-7d8f6c-0k4xz")));

        choose(&mut app, "Edit YAML…");
        let manifest = app.manifest.as_mut().expect("sample YAML editor");
        manifest.yaml = manifest.yaml.replace("example/gateway:1.4.2", "example/gateway:1.4.3");
        app.save_sample_manifest();
        let pod = app.snapshot.kind("Pod").into_iter().find(|pod| pod.name == "gateway-7d8f6c-0k4xz").unwrap();
        assert_eq!(pod.raw["spec"]["containers"][0]["image"], "example/gateway:1.4.3");

        choose(&mut app, "Delete…");
        assert!(matches!(app.confirm.take(), Some(Action { target: ActionTarget::SamplePod(_), output_to: OutputTo::Action, .. })));
        choose(&mut app, "Evict…");
        let action = app.confirm.take().expect("sample eviction confirmation");
        assert!(matches!(&action.target, ActionTarget::SamplePod(pod) if pod.name == "gateway-7d8f6c-0k4xz"));
        app.execute(action);
        assert!(app.snapshot.kind("Pod").iter().all(|pod| pod.name != "gateway-7d8f6c-0k4xz"));
        assert!(app.pod_logs.is_none());

        inspector.set(true);
        ctx.enable_accesskit();
        let pod = app.snapshot.kind("Pod")[0].clone();
        app.select(pod.clone());
        let toolbar = |app: &mut Observatory, label| {
            let output = frame(app, vec![]);
            let update = output.platform_output.accesskit_update.expect("Inspector accessibility tree");
            let bounds = update.nodes.iter().find_map(|(_, node)| {
                (node.label() == Some(label) && !node.is_disabled()).then(|| node.bounds()).flatten()
            }).expect(label);
            click(app, Pos2::new(((bounds.x0 + bounds.x1) / 2.0) as f32, ((bounds.y0 + bounds.y1) / 2.0) as f32));
        };
        #[cfg(target_os = "macos")]
        {
            toolbar(&mut app, "Attach Pod");
            assert!(app.output.contains("kubectl attach"));
            toolbar(&mut app, "Open Shell in Terminal");
            assert!(app.output.contains("kubectl exec"));
        }
        toolbar(&mut app, "Logs");
        assert!(app.pod_logs.as_ref().is_some_and(|logs| logs.pod.name == pod.name));
        toolbar(&mut app, "Copy resource reference");
        assert_eq!(app.notice, "Resource reference copied");
        toolbar(&mut app, "Edit YAML…");
        assert!(app.manifest.take().is_some());
        toolbar(&mut app, "Evict Pod…");
        assert!(matches!(app.confirm.take(), Some(Action { output_to: OutputTo::Eviction, .. })));
        toolbar(&mut app, "Delete resource…");
        assert!(matches!(app.confirm.take(), Some(Action { output_to: OutputTo::Action, .. })));
    }

    #[test]
    fn eviction_is_scoped_and_has_a_uid_precondition() {
        let pod = Resource::parse(json!({"kind":"Pod","metadata":{"name":"payments","namespace":"demo","uid":"old-pod"}}));
        let (args, body) = eviction_request(&pod);
        assert_eq!(args, ["create", "--raw", "/api/v1/namespaces/demo/pods/payments/eviction", "-f", "-"]);
        let request: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(request["apiVersion"], "policy/v1");
        assert_eq!(request["deleteOptions"]["preconditions"]["uid"], "old-pod");
    }

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
    let plain = pod_logs::clean_log_text("\x1b[33mWARN\x1b[0m[time] retry\n\x1b[36mqueue\x1b[0m=false");
    assert_eq!(plain, "WARN[time] retry\nqueue=false");
    assert_eq!(pod_logs::log_level(plain.lines().next().unwrap()).map(|(level, _)| level), Some("WARN"));
    assert_eq!(pod_logs::log_level("  [INFO] started").map(|(level, _)| level), Some("[INFO]"));
}

#[cfg(test)]
#[test]
fn dragging_a_resource_header_resizes_without_sorting() {
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = Observatory::new(&creation, Some(PathBuf::from("test.png")), None, false);
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
            if top.x == bottom.x && stroke.color == border() {
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
                    if left.y == right.y && stroke.color == border() => Some(right.x),
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
fn saved_widths_do_not_leave_an_empty_column_before_actions() {
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = Observatory::new(&creation, Some(PathBuf::from("test.png")), None, false);
    app.page = "Pods".into();
    let entry = CATALOG.iter().find(|entry| entry.kind == "Pod").unwrap();
    for column in ResourceColumn::for_table(entry, true) {
        if !matches!(column, ResourceColumn::Name | ResourceColumn::Cpu) {
            app.hidden_columns.insert(("Pod", column));
        }
    }
    app.table_widths.insert(("Pod", ResourceColumn::Name), 180.0);
    app.table_widths.insert(("Pod", ResourceColumn::Cpu), 110.0);
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(960.0, 540.0));
    let output = ctx.run(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| app.resources(ui));
    });
    let header_y = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Text(text) if text.galley.text() == "Actions" => Some(text.pos.y + text.galley.size().y / 2.0),
        _ => None,
    }).expect("Actions header");
    let dividers = output.shapes.iter().filter(|shape| matches!(shape.shape,
        egui::Shape::LineSegment { points: [top, bottom], stroke }
            if top.x == bottom.x && top.y < header_y && bottom.y > header_y && stroke.color == border()
    )).count();
    assert_eq!(dividers, 2, "only Name/CPU and CPU/Actions dividers should be visible");
}

#[cfg(test)]
#[test]
fn details_scrollbar_leaves_room_for_a_secret_eye() {
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = Observatory::new(&creation, Some(PathBuf::from("test.png")), None, false);
    app.target = Some(Target { exe: "/usr/bin/false".into(), ..Default::default() });
    ctx.style_mut(|style| style.animation_time = 0.0);
    let mut env = vec![json!({
        "name": "LONG_ENVIRONMENT_VARIABLE",
        "valueFrom": { "secretKeyRef": {
            "name": "a-long-secret-name-used-to-check-wrapping",
            "key": "token",
        }},
    })];
    env.extend((0..30).map(|index| json!({"name":format!("VAR_{index}"), "value":"fixture"})));
    app.selected = Some(Resource::parse(json!({
        "kind": "Pod",
        "metadata": {"name":"app", "namespace":"test"},
        "spec": {"containers":[{"name":"app", "env":env}]},
    })));
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(480.0, 1000.0));
    let mut frame = |events| {
        ctx.run(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ctx| {
            app.details(ctx);
        })
    };
    let expander = frame(vec![]).shapes.into_iter().find_map(|shape| match shape.shape {
        egui::Shape::Text(text) if text.galley.text() == "31 variables" => {
            Some(text.pos + text.galley.size() / 2.0)
        }
        _ => None,
    }).expect("environment expander");
    let button = |pressed| egui::Event::PointerButton {
        pos: expander, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
    };
    frame(vec![egui::Event::PointerMoved(expander), button(true)]);
    frame(vec![button(false)]);
    let output = frame(vec![]);
    let (eye, clip) = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Circle(circle) if (circle.radius - 1.8).abs() < 0.01 => {
            Some((circle.center, shape.clip_rect))
        }
        _ => None,
    }).expect("Secret eye");
    let track = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Rect(rect) if rect.fill == ctx.style().visuals.extreme_bg_color
            && rect.rect.height() > 300.0 && rect.rect.width() < 12.0 => Some(rect.rect),
        _ => None,
    }).expect("drawer scrollbar track");
    assert!(clip.contains(eye + Vec2::new(7.0, 0.0)), "eye {eye:?}, clip {clip:?}, track {track:?}");
    assert!(eye.x + 9.5 < track.left());
    let hovered = frame(vec![egui::Event::PointerMoved(eye)]);
    assert_eq!(hovered.platform_output.cursor_icon, egui::CursorIcon::PointingHand);
}

#[cfg(test)]
#[test]
fn column_widths_restore_independently_for_each_table() {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("lenser-widths-{}-{timestamp}", std::process::id()));
    let ctx = egui::Context::default();
    let creation = eframe::CreationContext::_new_kittest(ctx);
    let mut first = Observatory::new(&creation, Some(PathBuf::from("test.png")), None, false);
    let mut second = Observatory::new(&creation, Some(PathBuf::from("test.png")), None, false);
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
