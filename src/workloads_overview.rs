use super::{
    egui, Color32, Resource, RichText, Sense, Snapshot, Vec2, ALL_NAMESPACES, blue, border, green,
    muted, orange,
};
use std::collections::HashMap;

const WORKLOADS: [(&str, &str); 7] = [
    ("Pods", "Pod"),
    ("Deployments", "Deployment"),
    ("DaemonSets", "DaemonSet"),
    ("StatefulSets", "StatefulSet"),
    ("ReplicaSets", "ReplicaSet"),
    ("Jobs", "Job"),
    ("CronJobs", "CronJob"),
];

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    #[default]
    Unknown,
    Terminating,
    Succeeded,
    Running,
    Suspended,
    Pending,
    Failed,
}

impl Status {
    const ALL: [Self; 7] = [
        Self::Failed,
        Self::Terminating,
        Self::Suspended,
        Self::Running,
        Self::Pending,
        Self::Succeeded,
        Self::Unknown,
    ];

    fn display(self) -> (&'static str, Color32) {
        match self {
            Self::Unknown => ("Unknown / no pods", border()),
            Self::Terminating => ("Terminating", muted()),
            Self::Succeeded => ("Succeeded", blue()),
            Self::Running => ("Running / scheduled", green()),
            Self::Suspended => ("Suspended", orange()),
            Self::Pending => ("Pending", orange()),
            Self::Failed => ("Failed", Color32::from_rgb(221, 86, 90)),
        }
    }
}

fn uid(resource: &Resource) -> Option<&str> {
    resource
        .raw
        .pointer("/metadata/uid")?
        .as_str()
        .filter(|uid| !uid.is_empty())
}

fn owners(resource: &Resource) -> impl Iterator<Item = &str> {
    resource.raw["metadata"]["ownerReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reference| reference["uid"].as_str())
}

fn pod_status(pod: &Resource) -> Status {
    if pod.raw["metadata"]["deletionTimestamp"].as_str().is_some() {
        return Status::Terminating;
    }
    match pod.raw["status"]["phase"].as_str() {
        Some("Failed") => Status::Failed,
        Some("Pending") => Status::Pending,
        Some("Running") => Status::Running,
        Some("Succeeded") => Status::Succeeded,
        _ => Status::Unknown,
    }
}

pub(super) fn show(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    namespace: &str,
) -> Option<&'static str> {
    let scoped: Vec<_> = snapshot
        .resources
        .iter()
        .filter(|resource| namespace == ALL_NAMESPACES || resource.namespace == namespace)
        .collect();
    let mut by_owner: HashMap<&str, Status> = HashMap::new();
    for pod in scoped.iter().filter(|resource| resource.kind == "Pod") {
        for owner in owners(pod) {
            let status = by_owner.entry(owner).or_default();
            *status = (*status).max(pod_status(pod));
        }
    }
    // Deployments own ReplicaSets, which in turn own the Pods.
    for replica_set in scoped
        .iter()
        .filter(|resource| resource.kind == "ReplicaSet")
    {
        let Some(status) = uid(replica_set).and_then(|id| by_owner.get(id)).copied() else {
            continue;
        };
        for owner in owners(replica_set) {
            let parent = by_owner.entry(owner).or_default();
            *parent = (*parent).max(status);
        }
    }

    let mut page = None;
    ui.columns(2, |columns| {
        for (index, (label, kind)) in WORKLOADS.into_iter().enumerate() {
            let ui = &mut columns[usize::from(index >= 4)];
            let mut counts = [0_usize; Status::ALL.len()];
            for resource in scoped.iter().filter(|resource| resource.kind == kind) {
                let status = match kind {
                    "Pod" => pod_status(resource),
                    "CronJob" if resource.raw["spec"]["suspend"] == true => Status::Suspended,
                    "CronJob" => Status::Running,
                    _ => uid(resource)
                        .and_then(|id| by_owner.get(id))
                        .copied()
                        .unwrap_or_default(),
                };
                if let Some(index) = Status::ALL
                    .iter()
                    .position(|candidate| *candidate == status)
                {
                    counts[index] += 1;
                }
            }
            let total: usize = counts.iter().sum();
            ui.horizontal(|ui| {
                let link = egui::Link::new(RichText::new(format!("{label} ({total})")).color(blue()));
                if ui.add_sized([118.0, 20.0], link).clicked() {
                    page = Some(label);
                }
                let (bar, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width().max(0.0), 5.0),
                    Sense::hover(),
                );
                ui.painter().rect_filled(bar, 0.0, border());
                let mut left = bar.left();
                let mut legend = Vec::new();
                for (status, count) in Status::ALL
                    .into_iter()
                    .zip(counts)
                    .filter(|(_, count)| *count > 0)
                {
                    let (label, color) = status.display();
                    let right = left + bar.width() * count as f32 / total as f32;
                    let segment = egui::Rect::from_min_max(
                        egui::pos2(left, bar.top()),
                        egui::pos2(right.min(bar.right()), bar.bottom()),
                    );
                    ui.painter().rect_filled(segment, 0.0, color);
                    left = right;
                    legend.push(format!("{label}: {count}"));
                }
                response.on_hover_text(if legend.is_empty() {
                    "No resources".into()
                } else {
                    legend.join("\n")
                });
            });
        }
    });
    page
}
