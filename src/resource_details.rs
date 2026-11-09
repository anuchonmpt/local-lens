use super::{
    array, egui, event_for, service_ports, service_type, status_color, text, Align, Resource,
    ResourceColumn, RichText, Snapshot, Vec2, MUTED, SIDE, TEXT,
};
use crate::env_secrets::EnvSecrets;
use serde_json::Value;
use time::OffsetDateTime;

pub(super) enum Action<'a> {
    Forward(u16),
    Open(&'a Resource),
}

pub(super) fn section(ui: &mut egui::Ui, name: &str) {
    ui.add_space(11.0);
    ui.separator();
    ui.add_space(7.0);
    ui.label(RichText::new(name).size(17.0).strong().color(TEXT));
    ui.add_space(4.0);
}

fn columns(ui: &mut egui::Ui, widths: &[f32], mut cell: impl FnMut(&mut egui::Ui, usize)) {
    let gap = 10.0;
    let width = (ui.available_width() - gap * widths.len().saturating_sub(1) as f32).max(0.0);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.horizontal_top(|ui| {
            for (index, fraction) in widths.iter().enumerate() {
                ui.allocate_ui_with_layout(
                    Vec2::new(width * fraction, 18.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(width * fraction);
                        cell(ui, index);
                    },
                );
            }
        });
    });
}

fn property(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    let mut value = Some(value);
    columns(ui, &[0.29, 0.71], |ui, index| {
        if index == 0 {
            ui.add(egui::Label::new(RichText::new(label).color(MUTED)).wrap());
        } else if let Some(value) = value.take() {
            value(ui);
        }
    });
    ui.add_space(3.0);
    divider(ui);
}

fn divider(ui: &mut egui::Ui) {
    ui.add(egui::Separator::default().spacing(2.0).grow(0.0));
}

fn label(ui: &mut egui::Ui, value: impl Into<String>) {
    ui.add(egui::Label::new(value.into()).wrap());
}

fn row(ui: &mut egui::Ui, title: &str, value: impl Into<String>) {
    let value = value.into();
    if !value.is_empty() {
        property(ui, title, |ui| label(ui, value));
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(_) | Value::Bool(_) => value.to_string(),
        Value::Array(items) => items
            .iter()
            .map(scalar)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

fn fields(ui: &mut egui::Ui, raw: &Value, paths: &[(&str, &str)]) {
    for (title, path) in paths {
        if let Some(value) = raw.pointer(path) {
            row(ui, title, scalar(value));
        }
    }
}

fn map_pairs(value: &Value) -> Vec<String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
        .collect()
}

fn pills(ui: &mut egui::Ui, title: &str, entries: Vec<String>, expanded: bool) {
    if entries.is_empty() {
        return;
    }
    property(ui, title, |ui| {
        let contents = |ui: &mut egui::Ui| {
            for entry in &entries {
                let caption = if entry.chars().nth(115).is_some() {
                    format!("{}…", entry.chars().take(115).collect::<String>())
                } else {
                    entry.clone()
                };
                let response = ui.add(
                    egui::Label::new(RichText::new(caption).background_color(SIDE)).truncate(),
                );
                response.on_hover_text_at_pointer(entry);
            }
        };
        if expanded || entries.len() == 1 {
            contents(ui);
        } else {
            egui::CollapsingHeader::new(format!("{} {}", entries.len(), title.to_lowercase()))
                .id_salt(title)
                .show(ui, contents);
        }
    });
}

fn table(ui: &mut egui::Ui, headings: &[&str], widths: &[f32], rows: &[Vec<String>], empty: &str) {
    if rows.is_empty() {
        ui.add_space(5.0);
        ui.colored_label(MUTED, empty);
        return;
    }
    divider(ui);
    ui.add_space(4.0);
    columns(ui, widths, |ui, index| {
        ui.add(egui::Label::new(RichText::new(headings[index]).color(TEXT).strong()).wrap());
    });
    divider(ui);
    for row in rows {
        ui.add_space(4.0);
        columns(ui, widths, |ui, index| {
            let value = row
                .get(index)
                .filter(|value| !value.is_empty())
                .map(String::as_str)
                .unwrap_or("—");
            ui.add(egui::Label::new(RichText::new(value).color(MUTED)).wrap());
        });
        ui.add_space(3.0);
        divider(ui);
    }
}

fn created(resource: &Resource) -> String {
    let Some(timestamp) = resource.created else {
        return String::new();
    };
    let seconds = (OffsetDateTime::now_utc() - timestamp)
        .whole_seconds()
        .max(0);
    let days = seconds / 86400;
    let hours = (seconds / 3600) % 24;
    let minutes = (seconds / 60) % 60;
    let elapsed = if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    };
    format!(
        "{elapsed} ago ({})",
        text(&resource.raw, "/metadata/creationTimestamp")
    )
}

fn reference<'a>(
    snapshot: &'a Snapshot,
    kind: &str,
    namespace: &str,
    name: &str,
) -> Option<&'a Resource> {
    snapshot
        .resources
        .iter()
        .find(|item| item.kind == kind && item.namespace == namespace && item.name == name)
}

fn linked_row<'a>(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    target: Option<&'a Resource>,
) -> Option<&'a Resource> {
    if value.is_empty() {
        return None;
    }
    let mut clicked = false;
    property(ui, title, |ui| {
        if target.is_some() {
            clicked = ui.link(value).clicked();
        } else {
            label(ui, value);
        }
    });
    target.filter(|_| clicked)
}

fn owner_links<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    resource: &Resource,
) -> Option<&'a Resource> {
    let owners = array(&resource.raw, "/metadata/ownerReferences");
    if owners.is_empty() {
        return None;
    }
    let mut open = None;
    property(ui, "Controlled By", |ui| {
        for owner in owners {
            let kind = text(&owner, "/kind");
            let name = text(&owner, "/name");
            let caption = format!("{kind} {name}");
            let target = reference(snapshot, &kind, &resource.namespace, &name)
                .or_else(|| reference(snapshot, &kind, "", &name))
                .filter(|item| {
                    owner["uid"]
                        .as_str()
                        .is_none_or(|uid| item.raw["metadata"]["uid"] == uid)
                });
            if let Some(target) = target {
                if ui.link(caption).clicked() {
                    open = Some(target);
                }
            } else {
                label(ui, caption);
            }
        }
    });
    open
}

fn owned_by(child: &Resource, parent: &Resource) -> bool {
    if child.namespace != parent.namespace {
        return false;
    }
    let uid = parent.raw.pointer("/metadata/uid").and_then(Value::as_str);
    child
        .raw
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|refs| {
            refs.iter().any(|owner| {
                uid.map_or_else(
                    || owner["kind"] == parent.kind && owner["name"] == parent.name,
                    |uid| owner["uid"] == uid,
                )
            })
        })
}

fn pods_for<'a>(
    snapshot: &'a Snapshot,
    resource: &Resource,
    revisions: &[&Resource],
) -> Vec<&'a Resource> {
    snapshot
        .kind("Pod")
        .into_iter()
        .filter(|pod| {
            if resource.kind == "Node" {
                pod.node == resource.name
            } else {
                owned_by(pod, resource) || revisions.iter().any(|revision| owned_by(pod, revision))
            }
        })
        .collect()
}

fn workload(kind: &str) -> bool {
    matches!(
        kind,
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" | "CronJob"
    )
}

pub(super) fn show<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    resource: &Resource,
    metrics: &mut super::detail_metrics::Metrics,
    target: Option<&super::Target>,
    secrets: &mut EnvSecrets,
) -> Option<Action<'a>> {
    let raw = &resource.raw;
    let mut revisions: Vec<_> = if resource.kind == "Deployment" {
        snapshot
            .kind("ReplicaSet")
            .into_iter()
            .filter(|child| owned_by(child, resource))
            .collect()
    } else if resource.kind == "CronJob" {
        snapshot
            .kind("Job")
            .into_iter()
            .filter(|child| owned_by(child, resource))
            .collect()
    } else {
        vec![]
    };
    let revision = |item: &Resource| {
        text(
            &item.raw,
            "/metadata/annotations/deployment.kubernetes.io~1revision",
        )
    };
    revisions.sort_by_key(|item| std::cmp::Reverse(revision(item).parse::<u64>().ok()));
    let pods = if workload(&resource.kind) || resource.kind == "Node" {
        pods_for(snapshot, resource, &revisions)
    } else {
        vec![]
    };
    if matches!(resource.kind.as_str(), "Pod" | "Node") || workload(&resource.kind) {
        metrics.show(ui, snapshot, resource, target);
    }
    section(ui, "Properties");
    row(ui, "Created", created(resource));
    row(ui, "Name", &resource.name);
    let mut open = linked_row(
        ui,
        "Namespace",
        &resource.namespace,
        reference(snapshot, "Namespace", "", &resource.namespace),
    );
    pills(ui, "Labels", map_pairs(&raw["metadata"]["labels"]), false);
    pills(
        ui,
        "Annotations",
        map_pairs(&raw["metadata"]["annotations"]),
        false,
    );
    open = owner_links(ui, snapshot, resource).or(open);
    if resource.kind == "Service" {
        pills(ui, "Selector", map_pairs(&raw["spec"]["selector"]), true);
        row(ui, "Type", service_type(raw));
        fields(ui, raw, &[("Session Affinity", "/spec/sessionAffinity")]);
    }
    if workload(&resource.kind) {
        let spec = &raw["spec"];
        let status = &raw["status"];
        match resource.kind.as_str() {
            "Deployment" => row(
                ui,
                "Replicas",
                format!(
                    "{} desired, {} updated, {} total, {} available, {} unavailable",
                    resource.replicas(),
                    status["updatedReplicas"].as_u64().unwrap_or(0),
                    status["replicas"].as_u64().unwrap_or(0),
                    status["availableReplicas"].as_u64().unwrap_or(0),
                    status["unavailableReplicas"].as_u64().unwrap_or(0)
                ),
            ),
            "StatefulSet" | "ReplicaSet" | "DaemonSet" => row(ui, "Ready", &resource.ready),
            _ => {}
        }
        pills(
            ui,
            "Selector",
            map_pairs(&spec["selector"]["matchLabels"]),
            true,
        );
        pills(
            ui,
            "Node Selector",
            map_pairs(&spec["template"]["spec"]["nodeSelector"]),
            true,
        );
        fields(
            ui,
            raw,
            &[
                ("Strategy Type", "/spec/strategy/type"),
                ("Update Strategy", "/spec/updateStrategy/type"),
                ("Service Name", "/spec/serviceName"),
                ("Schedule", "/spec/schedule"),
                ("Time Zone", "/spec/timeZone"),
                ("Suspended", "/spec/suspend"),
                ("Concurrency Policy", "/spec/concurrencyPolicy"),
                ("Parallelism", "/spec/parallelism"),
                ("Completions", "/spec/completions"),
                ("Succeeded", "/status/succeeded"),
                ("Failed", "/status/failed"),
                ("Last Schedule", "/status/lastScheduleTime"),
            ],
        );
    }
    property(ui, "Status", |ui| {
        ui.colored_label(status_color(&resource.status), &resource.status);
    });

    match resource.kind.as_str() {
        "Service" => {
            return service(ui, snapshot, resource)
                .map(Action::Forward)
                .or(open.map(Action::Open))
        }
        "Pod" => {
            open = linked_row(
                ui,
                "Node",
                &resource.node,
                reference(snapshot, "Node", "", &resource.node),
            )
            .or(open);
            fields(
                ui,
                raw,
                &[
                    ("Pod IP", "/status/podIP"),
                    ("QoS Class", "/status/qosClass"),
                ],
            );
            let ips = array(raw, "/status/podIPs")
                .iter()
                .map(|ip| text(ip, "/ip"))
                .collect::<Vec<_>>();
            row(ui, "Pod IPs", ips.join(", "));
            let account = text(raw, "/spec/serviceAccountName");
            open = linked_row(
                ui,
                "Service Account",
                &account,
                reference(snapshot, "ServiceAccount", &resource.namespace, &account),
            )
            .or(open);
            row(ui, "Ready", &resource.ready);
            row(ui, "Restarts", resource.restarts.to_string());
            pills(
                ui,
                "Node Selector",
                map_pairs(&raw["spec"]["nodeSelector"]),
                true,
            );
        }
        "Ingress" => fields(ui, raw, &[("Ingress Class", "/spec/ingressClassName")]),
        "Event" => {
            fields(ui, raw, &[("Reason", "/reason")]);
            row(ui, "Message", super::event_message(resource));
            let target = snapshot
                .resources
                .iter()
                .find(|item| event_for(resource, item));
            open = linked_row(
                ui,
                "Involved Object",
                &super::event_object_label(resource),
                target,
            )
            .or(open);
            row(ui, "Source", super::event_source(resource));
            row(ui, "Count", super::event_count(resource).to_string());
            row(
                ui,
                "First Seen",
                super::event_timestamp(super::resource_age_start(resource)),
            );
            row(
                ui,
                "Last Seen",
                super::event_timestamp(super::event_last_seen(resource)),
            );
        }
        _ => {}
    }
    pills(
        ui,
        "Conditions",
        array(raw, "/status/conditions")
            .iter()
            .map(|condition| {
                format!(
                    "{}: {}",
                    text(condition, "/type"),
                    text(condition, "/status")
                )
            })
            .collect(),
        true,
    );

    match resource.kind.as_str() {
        "Pod" => open = pod_sections(ui, snapshot, resource, target, secrets).or(open),
        "Ingress" => ingress(ui, raw),
        "Deployment" => {
            section(ui, "Deploy Revisions");
            let rows: Vec<_> = revisions
                .iter()
                .map(|item| {
                    vec![
                        revision(item),
                        item.name.clone(),
                        item.ready.clone(),
                        ResourceColumn::Age.value(item),
                    ]
                })
                .collect();
            table(
                ui,
                &["#", "Summary", "Pods", "Age"],
                &[0.10, 0.58, 0.16, 0.16],
                &rows,
                "No deploy revisions found",
            );
        }
        "CronJob" => {
            section(ui, "Jobs");
            let rows: Vec<_> = revisions
                .iter()
                .map(|job| {
                    vec![
                        job.name.clone(),
                        job.status.clone(),
                        ResourceColumn::Age.value(job),
                    ]
                })
                .collect();
            table(
                ui,
                &["Name", "Status", "Age"],
                &[0.60, 0.23, 0.17],
                &rows,
                "No jobs found",
            );
        }
        _ => other_sections(ui, resource),
    }
    let template = if resource.kind == "CronJob" {
        "/spec/jobTemplate/spec/template/spec"
    } else {
        "/spec/template/spec"
    };
    if let Some(template) = raw.pointer(template) {
        for (title, key) in [
            ("Init Container Environment", "initContainers"),
            ("Container Environment", "containers"),
        ] {
            let containers = template[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|container| {
                    ["env", "envFrom"].iter().any(|key| {
                        container[key]
                            .as_array()
                            .is_some_and(|items| !items.is_empty())
                    })
                })
                .collect::<Vec<_>>();
            if !containers.is_empty() {
                section(ui, title);
                for container in containers {
                    let name = container["name"].as_str().unwrap_or_default();
                    ui.strong(name);
                    open = ui
                        .push_id((title, name), |ui| {
                            container_environment(
                                ui,
                                snapshot,
                                &resource.namespace,
                                container,
                                target,
                                secrets,
                            )
                        })
                        .inner
                        .or(open);
                }
            }
        }
    }
    if matches!(
        resource.kind.as_str(),
        "Node" | "Deployment" | "DaemonSet" | "StatefulSet" | "ReplicaSet" | "Job"
    ) {
        section(ui, "Pods");
        let rows: Vec<_> = pods
            .iter()
            .map(|pod| {
                vec![
                    pod.name.clone(),
                    pod.namespace.clone(),
                    pod.ready.clone(),
                    pod.status.clone(),
                ]
            })
            .collect();
        table(
            ui,
            &["Name", "Namespace", "Ready", "Status"],
            &[0.49, 0.20, 0.12, 0.19],
            &rows,
            "No pods found",
        );
    }
    open.map(Action::Open)
}

fn pod_sections<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    pod: &Resource,
    target: Option<&super::Target>,
    secrets: &mut EnvSecrets,
) -> Option<&'a Resource> {
    let raw = &pod.raw;
    let mut open = None;
    let volumes = array(raw, "/spec/volumes");
    if !volumes.is_empty() {
        section(ui, "Pod Volumes");
        for volume in volumes {
            let source = volume
                .as_object()
                .and_then(|values| values.keys().find(|key| *key != "name"))
                .cloned()
                .unwrap_or_default();
            row(ui, &text(&volume, "/name"), source);
        }
    }
    for (title, spec, status) in [
        (
            "Init Containers",
            "/spec/initContainers",
            "/status/initContainerStatuses",
        ),
        (
            "Containers",
            "/spec/containers",
            "/status/containerStatuses",
        ),
        (
            "Ephemeral Containers",
            "/spec/ephemeralContainers",
            "/status/ephemeralContainerStatuses",
        ),
    ] {
        let containers = array(raw, spec);
        if containers.is_empty() {
            continue;
        }
        let statuses = array(raw, status);
        section(ui, title);
        for container in containers {
            let name = text(&container, "/name");
            ui.strong(&name);
            if let Some(status) = statuses.iter().find(|status| status["name"] == name) {
                if let Some((state, _)) = status["state"]
                    .as_object()
                    .and_then(|state| state.iter().next())
                {
                    let ready = if status["ready"] == true {
                        ", ready"
                    } else {
                        ""
                    };
                    row(ui, "Status", format!("{state}{ready}"));
                }
                fields(ui, status, &[("Restarts", "/restartCount")]);
            }
            fields(
                ui,
                &container,
                &[
                    ("Image", "/image"),
                    ("Image Pull Policy", "/imagePullPolicy"),
                    ("Command", "/command"),
                    ("Arguments", "/args"),
                ],
            );
            let ports = array(&container, "/ports")
                .iter()
                .filter_map(|port| {
                    Some(format!(
                        "{}/{}",
                        port["containerPort"].as_u64()?,
                        port["protocol"].as_str().unwrap_or("TCP")
                    ))
                })
                .collect::<Vec<_>>();
            row(ui, "Ports", ports.join(", "));
            open = ui
                .push_id((title, &name), |ui| {
                    container_environment(ui, snapshot, &pod.namespace, &container, target, secrets)
                })
                .inner
                .or(open);
            pills(
                ui,
                "Mounts",
                array(&container, "/volumeMounts")
                    .iter()
                    .map(|mount| {
                        format!("{} → {}", text(mount, "/name"), text(mount, "/mountPath"))
                    })
                    .collect(),
                false,
            );
            row(
                ui,
                "Requests",
                map_pairs(&container["resources"]["requests"]).join(", "),
            );
            row(
                ui,
                "Limits",
                map_pairs(&container["resources"]["limits"]).join(", "),
            );
            ui.add_space(7.0);
        }
    }
    open
}

fn container_environment<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    namespace: &str,
    container: &Value,
    target: Option<&super::Target>,
    secrets: &mut EnvSecrets,
) -> Option<&'a Resource> {
    let vars = container["env"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let sources = container["envFrom"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if vars.is_empty() && sources.is_empty() {
        return None;
    }

    let mut open = None;
    property(ui, "Environment", |ui| {
        let explicit: std::collections::HashMap<_, _> = vars
            .iter()
            .enumerate()
            .filter_map(|(index, variable)| Some((variable["name"].as_str()?, index)))
            .collect();
        let mut imported = std::collections::BTreeMap::new();
        let mut pending = false;
        let mut unavailable = Vec::new();
        for source in sources {
            let prefix = source["prefix"].as_str().unwrap_or_default();
            for (kind, selector) in [
                ("ConfigMap", &source["configMapRef"]),
                ("Secret", &source["secretRef"]),
            ] {
                let Some(name) = selector["name"].as_str() else {
                    continue;
                };
                let mut add_key = |key: &str| {
                    let variable = format!("{prefix}{key}");
                    if !explicit.contains_key(variable.as_str()) {
                        // Later envFrom sources override earlier ones.
                        imported.insert(variable, (kind, selector, key.to_owned()));
                    }
                };
                if kind == "Secret" {
                    match target.map(|target| secrets.keys(ui, target, namespace, name)) {
                        Some(Some(Ok(keys))) => keys.iter().for_each(|key| add_key(key)),
                        Some(None) => pending = true,
                        Some(Some(Err(error))) => {
                            unavailable.push(format!("secret({name}) · {error}"))
                        }
                        None => unavailable.push(format!("secret({name}) · Keys unavailable")),
                    }
                } else if let Some(resource) = reference(snapshot, kind, namespace, name) {
                    if let Some(data) = resource.raw["data"].as_object() {
                        for (key, value) in data {
                            if value.is_string() {
                                add_key(key);
                            }
                        }
                    }
                } else {
                    unavailable.push(format!("configMap({name}) · Keys unavailable"));
                }
            }
        }
        let count = explicit.len() + imported.len();
        let noun = if count == 1 { "variable" } else { "variables" };
        let title = if pending {
            "Counting variables…".to_owned()
        } else if !unavailable.is_empty() {
            format!("{count} known {noun}")
        } else {
            format!("{count} {noun}")
        };
        egui::CollapsingHeader::new(title)
            .id_salt("environment")
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (index, variable) in vars.iter().enumerate() {
                    let Some(name) = variable["name"].as_str() else {
                        continue;
                    };
                    if explicit.get(name) != Some(&index) {
                        continue;
                    }
                    let from = &variable["valueFrom"];
                    if let Some((kind, selector)) = [
                        ("ConfigMap", &from["configMapKeyRef"]),
                        ("Secret", &from["secretKeyRef"]),
                    ]
                    .into_iter()
                    .find(|(_, selector)| selector.is_object())
                    {
                        open = ui
                            .push_id(("env", index), |ui| {
                                environment_reference(
                                    ui,
                                    snapshot,
                                    namespace,
                                    kind,
                                    selector,
                                    name,
                                    selector["key"].as_str(),
                                    target,
                                    secrets,
                                )
                            })
                            .inner
                            .or(open);
                    } else {
                        let value = environment_description(variable);
                        environment_line(ui, name, &value);
                    }
                }
                for message in unavailable {
                    ui.colored_label(super::ORANGE, message);
                }
                for (variable, (kind, selector, key)) in imported {
                    open = ui
                        .push_id(("envFrom", &variable, &key), |ui| {
                            environment_reference(
                                ui,
                                snapshot,
                                namespace,
                                kind,
                                selector,
                                &variable,
                                Some(&key),
                                target,
                                secrets,
                            )
                        })
                        .inner
                        .or(open);
                }
            });
    });
    open
}

fn environment_description(variable: &Value) -> String {
    let from = &variable["valueFrom"];
    if let Some(field) = from.pointer("/fieldRef/fieldPath").and_then(Value::as_str) {
        format!("Pod field · {field}")
    } else if let Some(field) = from
        .pointer("/resourceFieldRef/resource")
        .and_then(Value::as_str)
    {
        let container = from
            .pointer("/resourceFieldRef/containerName")
            .and_then(Value::as_str)
            .unwrap_or("this container");
        format!("Container resource · {container} · {field}")
    } else if let Some(file) = from.get("fileKeyRef") {
        format!(
            "File · {} · {} · {}",
            text(file, "/volumeName"),
            text(file, "/path"),
            text(file, "/key")
        )
    } else if from.as_object().is_some_and(|source| !source.is_empty()) {
        "Value from another source".into()
    } else {
        variable["value"].as_str().unwrap_or_default().to_owned()
    }
}

fn environment_reference<'a>(
    ui: &mut egui::Ui,
    snapshot: &'a Snapshot,
    namespace: &str,
    kind: &str,
    selector: &Value,
    variable: &str,
    key: Option<&str>,
    target: Option<&super::Target>,
    secrets: &mut EnvSecrets,
) -> Option<&'a Resource> {
    let name = selector["name"].as_str().unwrap_or_default();
    let resource = reference(snapshot, kind, namespace, name);
    let data = (kind == "ConfigMap")
        .then(|| resource.and_then(|item| item.raw["data"].as_object()))
        .flatten();
    let value = key.and_then(|key| data?.get(key)?.as_str());
    let source = if kind == "Secret" {
        "secret"
    } else {
        "configMap"
    };
    let caption = format!("{source}({name})");
    let mut clicked = false;
    ui.horizontal_wrapped(|ui| {
        if let Some(value) = value {
            environment_line(ui, variable, value);
        } else {
            ui.add(egui::Label::new(RichText::new(format!("{variable} :")).color(MUTED)).wrap());
        }
        let hint = key.map_or_else(
            || format!("{kind} {name}"),
            |key| format!("{kind} {name} · {key}"),
        );
        let source = |ui: &mut egui::Ui| {
            if resource.is_some() {
                clicked = ui.link(caption).on_hover_text(hint).clicked();
            } else {
                ui.label(caption).on_hover_text(hint);
            }
        };
        if let ("Secret", Some(key)) = (kind, key) {
            secrets.show(ui, target, namespace, name, key, variable, source);
        } else {
            source(ui);
        }
        if selector["optional"] == true {
            ui.label("Optional");
        }
    });
    resource.filter(|_| clicked)
}

fn environment_line(ui: &mut egui::Ui, name: &str, value: &str) {
    let value = if value.is_empty() { "(empty)" } else { value };
    ui.add(
        egui::Label::new(RichText::new(format!("{name} : {value}")).color(MUTED))
            .wrap()
            .selectable(true),
    );
}

fn service(ui: &mut egui::Ui, snapshot: &Snapshot, resource: &Resource) -> Option<u16> {
    let raw = &resource.raw;
    section(ui, "Connection");
    fields(
        ui,
        raw,
        &[
            ("Cluster IP", "/spec/clusterIP"),
            ("Cluster IPs", "/spec/clusterIPs"),
            ("IP families", "/spec/ipFamilies"),
            ("IP family policy", "/spec/ipFamilyPolicy"),
        ],
    );
    if let Some(address) = ResourceColumn::ExternalIp.service_value(resource) {
        row(ui, "External IP", address);
    }
    let mut forward = None;
    if let Some(ports) = ResourceColumn::Ports.service_value(resource) {
        let tcp_ports: Vec<_> = service_ports(raw)
            .filter(|(_, port)| port["protocol"].as_str().unwrap_or("TCP") == "TCP")
            .filter_map(|(port, _)| u16::try_from(port).ok().filter(|port| *port > 0))
            .collect();
        property(ui, "Ports", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(ports);
                match tcp_ports.as_slice() {
                    [] => {}
                    [port] => {
                        if ui.button("Forward…").clicked() {
                            forward = Some(*port);
                        }
                    }
                    ports => {
                        ui.menu_button("Forward…", |ui| {
                            for port in ports {
                                if ui.button(format!("Port {port}")).clicked() {
                                    forward = Some(*port);
                                    ui.close_menu();
                                }
                            }
                        });
                    }
                }
            });
        });
    }

    section(ui, "Endpoint Slices");
    let mut slices: Vec<_> = snapshot
        .related
        .iter()
        .filter(|slice| {
            slice.kind == "EndpointSlice"
                && slice.namespace == resource.namespace
                && slice
                    .raw
                    .pointer("/metadata/labels/kubernetes.io~1service-name")
                    .and_then(Value::as_str)
                    == Some(&resource.name)
        })
        .collect();
    slices.sort_by(|a, b| a.name.cmp(&b.name));
    let rows: Vec<_> = slices
        .iter()
        .map(|slice| {
            let endpoints = array(&slice.raw, "/endpoints");
            let ready = endpoints
                .iter()
                .filter(|ep| ep["conditions"]["ready"].as_bool().unwrap_or(true))
                .count();
            let ports = array(&slice.raw, "/ports")
                .iter()
                .filter_map(|port| {
                    Some(format!(
                        "{}/{}",
                        port["port"].as_u64()?,
                        port["protocol"].as_str().unwrap_or("TCP")
                    ))
                })
                .collect::<Vec<_>>()
                .join(", ");
            vec![
                slice.name.clone(),
                format!("{ready}/{}", endpoints.len()),
                ports,
                text(&slice.raw, "/addressType"),
                ResourceColumn::Age.value(slice),
            ]
        })
        .collect();
    table(
        ui,
        &["Name", "Endpoints", "Ports", "Address Type", "Age"],
        &[0.32, 0.16, 0.21, 0.21, 0.10],
        &rows,
        if snapshot.detail_errors.contains_key("EndpointSlice") {
            "Endpoint slices unavailable"
        } else {
            "No endpoint slices found"
        },
    );
    if let Some(error) = snapshot.detail_errors.get("EndpointSlice") {
        ui.label(RichText::new("Endpoint Slice access details").color(MUTED))
            .on_hover_text(error);
    }

    section(ui, "Endpoints");
    let rows: Vec<_> = snapshot
        .related
        .iter()
        .filter(|endpoint| {
            endpoint.kind == "Endpoints"
                && endpoint.namespace == resource.namespace
                && endpoint.name == resource.name
        })
        .map(|endpoint| vec![endpoint.name.clone(), endpoint_addresses(&endpoint.raw)])
        .collect();
    table(
        ui,
        &["Name", "Endpoints"],
        &[0.48, 0.52],
        &rows,
        if snapshot.detail_errors.contains_key("Endpoints") {
            "Endpoints unavailable"
        } else {
            "No endpoints found"
        },
    );
    if let Some(error) = snapshot.detail_errors.get("Endpoints") {
        ui.label(RichText::new("Endpoints access details").color(MUTED))
            .on_hover_text(error);
    }
    forward
}

fn endpoint_addresses(raw: &Value) -> String {
    let mut addresses = Vec::new();
    for subset in array(raw, "/subsets") {
        let ports = array(&subset, "/ports");
        for (field, suffix) in [("/addresses", ""), ("/notReadyAddresses", " (not ready)")] {
            for address in array(&subset, field) {
                let ip = text(&address, "/ip");
                if ip.is_empty() {
                    continue;
                }
                if ports.is_empty() {
                    addresses.push(format!("{ip}{suffix}"));
                } else {
                    for port in &ports {
                        let Some(port) = port["port"].as_u64() else {
                            continue;
                        };
                        let ip = if ip.contains(':') {
                            format!("[{ip}]")
                        } else {
                            ip.clone()
                        };
                        addresses.push(format!("{ip}:{port}{suffix}"));
                    }
                }
            }
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses.join(", ")
}

fn backend(value: &Value) -> String {
    if let Some(service) = value.get("service") {
        let name = text(service, "/name");
        let port = service["port"]
            .get("name")
            .or_else(|| service["port"].get("number"))
            .map(scalar)
            .unwrap_or_default();
        return if port.is_empty() {
            name
        } else {
            format!("{name}:{port}")
        };
    }
    if let Some(resource) = value.get("resource") {
        return format!("{} {}", text(resource, "/kind"), text(resource, "/name"))
            .trim()
            .into();
    }
    String::new()
}

fn ingress(ui: &mut egui::Ui, raw: &Value) {
    section(ui, "Rules");
    if let Some(default) = raw.pointer("/spec/defaultBackend") {
        row(ui, "Default Backend", backend(default));
    }
    let rules = array(raw, "/spec/rules");
    if rules.is_empty() {
        ui.colored_label(MUTED, "No host rules found");
    }
    for rule in rules {
        let host = rule["host"]
            .as_str()
            .filter(|host| !host.is_empty())
            .unwrap_or("*");
        ui.add_space(5.0);
        ui.strong(format!("Host: {host}"));
        let paths: Vec<_> = array(&rule, "/http/paths")
            .iter()
            .map(|path| {
                vec![
                    text(path, "/path"),
                    text(path, "/pathType"),
                    backend(&path["backend"]),
                ]
            })
            .collect();
        table(
            ui,
            &["Path", "Path Type", "Backend"],
            &[0.28, 0.24, 0.48],
            &paths,
            "No paths found",
        );
    }
    let tls = array(raw, "/spec/tls");
    if !tls.is_empty() {
        section(ui, "TLS");
        let rows: Vec<_> = tls
            .iter()
            .map(|entry| vec![scalar(&entry["hosts"]), text(entry, "/secretName")])
            .collect();
        table(
            ui,
            &["Hosts", "Secret"],
            &[0.60, 0.40],
            &rows,
            "No TLS configuration",
        );
    }
    section(ui, "Load-Balancer Ingress Points");
    let rows: Vec<_> = array(raw, "/status/loadBalancer/ingress")
        .iter()
        .map(|entry| vec![text(entry, "/ip"), text(entry, "/hostname")])
        .collect();
    table(
        ui,
        &["IP", "Hostname"],
        &[0.34, 0.66],
        &rows,
        "No ingress points found",
    );
}

fn other_sections(ui: &mut egui::Ui, resource: &Resource) {
    let raw = &resource.raw;
    let (title, paths): (&str, &[(&str, &str)]) = match resource.kind.as_str() {
        "Node" => (
            "Node Info",
            &[
                ("OS Image", "/status/nodeInfo/osImage"),
                ("Kernel Version", "/status/nodeInfo/kernelVersion"),
                ("Architecture", "/status/nodeInfo/architecture"),
                ("Kubelet Version", "/status/nodeInfo/kubeletVersion"),
                (
                    "Container Runtime",
                    "/status/nodeInfo/containerRuntimeVersion",
                ),
                ("Pod CIDRs", "/spec/podCIDRs"),
                ("Provider ID", "/spec/providerID"),
                ("Unschedulable", "/spec/unschedulable"),
            ],
        ),
        "Namespace" => (
            "Lifecycle",
            &[
                ("Phase", "/status/phase"),
                ("Finalizers", "/spec/finalizers"),
            ],
        ),
        "Secret" => ("Data", &[("Type", "/type"), ("Immutable", "/immutable")]),
        "ConfigMap" => ("Data", &[("Immutable", "/immutable")]),
        "HorizontalPodAutoscaler" => (
            "Scaling",
            &[
                ("Target Kind", "/spec/scaleTargetRef/kind"),
                ("Target Name", "/spec/scaleTargetRef/name"),
                ("Min Replicas", "/spec/minReplicas"),
                ("Max Replicas", "/spec/maxReplicas"),
                ("Current Replicas", "/status/currentReplicas"),
                ("Desired Replicas", "/status/desiredReplicas"),
                ("Last Scale Time", "/status/lastScaleTime"),
            ],
        ),
        "NetworkPolicy" => ("Policy", &[("Policy Types", "/spec/policyTypes")]),
        "PersistentVolumeClaim" => (
            "Storage",
            &[
                ("Storage Class", "/spec/storageClassName"),
                ("Volume", "/spec/volumeName"),
                ("Requested Storage", "/spec/resources/requests/storage"),
                ("Capacity", "/status/capacity/storage"),
                ("Access Modes", "/spec/accessModes"),
                ("Volume Mode", "/spec/volumeMode"),
            ],
        ),
        "PersistentVolume" => (
            "Storage",
            &[
                ("Storage Class", "/spec/storageClassName"),
                ("Capacity", "/spec/capacity/storage"),
                ("Access Modes", "/spec/accessModes"),
                ("Reclaim Policy", "/spec/persistentVolumeReclaimPolicy"),
                ("Volume Mode", "/spec/volumeMode"),
                ("Claim Namespace", "/spec/claimRef/namespace"),
                ("Claim", "/spec/claimRef/name"),
                ("CSI Driver", "/spec/csi/driver"),
            ],
        ),
        "StorageClass" => (
            "Provisioning",
            &[
                ("Provisioner", "/provisioner"),
                ("Reclaim Policy", "/reclaimPolicy"),
                ("Volume Binding Mode", "/volumeBindingMode"),
                ("Allow Volume Expansion", "/allowVolumeExpansion"),
                ("Mount Options", "/mountOptions"),
            ],
        ),
        "RoleBinding" | "ClusterRoleBinding" => (
            "Role Reference",
            &[
                ("Kind", "/roleRef/kind"),
                ("Name", "/roleRef/name"),
                ("API Group", "/roleRef/apiGroup"),
            ],
        ),
        "CustomResourceDefinition" => (
            "Definition",
            &[
                ("Group", "/spec/group"),
                ("Scope", "/spec/scope"),
                ("Kind", "/spec/names/kind"),
                ("Plural", "/spec/names/plural"),
                ("Short Names", "/spec/names/shortNames"),
                ("Stored Versions", "/status/storedVersions"),
            ],
        ),
        _ => ("", &[]),
    };
    if !title.is_empty() {
        section(ui, title);
        fields(ui, raw, paths);
    }
    match resource.kind.as_str() {
        "Node" => {
            let addresses = array(raw, "/status/addresses");
            if !addresses.is_empty() {
                section(ui, "Addresses");
                for address in addresses {
                    row(ui, &text(&address, "/type"), text(&address, "/address"));
                }
            }
            section(ui, "Capacity");
            let rows: Vec<_> = raw["status"]["capacity"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(key, value)| {
                    vec![
                        key.clone(),
                        scalar(value),
                        scalar(&raw["status"]["allocatable"][key]),
                    ]
                })
                .collect();
            table(
                ui,
                &["Resource", "Capacity", "Allocatable"],
                &[0.42, 0.29, 0.29],
                &rows,
                "No capacity data",
            );
        }
        "ConfigMap" => {
            let keys = |value: &Value| {
                value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, _)| key.clone())
                    .collect()
            };
            pills(ui, "Keys", keys(&raw["data"]), false);
            pills(ui, "Binary Keys", keys(&raw["binaryData"]), false);
        }
        "Secret" => {
            ui.colored_label(MUTED, "Secret values are available from Configuration.");
        }
        "ServiceAccount" => {
            section(ui, "References");
            for (title, pointer) in [
                ("Secrets", "/secrets"),
                ("Image Pull Secrets", "/imagePullSecrets"),
            ] {
                row(
                    ui,
                    title,
                    array(raw, pointer)
                        .iter()
                        .map(|entry| text(entry, "/name"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            fields(
                ui,
                raw,
                &[("Automount Token", "/automountServiceAccountToken")],
            );
        }
        "Role" | "ClusterRole" => {
            section(ui, "Rules");
            let rows: Vec<_> = array(raw, "/rules")
                .iter()
                .map(|rule| {
                    vec![
                        scalar(&rule["apiGroups"]),
                        scalar(&rule["resources"]),
                        scalar(&rule["verbs"]),
                        scalar(&rule["nonResourceURLs"]),
                    ]
                })
                .collect();
            table(
                ui,
                &["API Groups", "Resources", "Verbs", "Non-resource URLs"],
                &[0.22, 0.26, 0.28, 0.24],
                &rows,
                "No rules found",
            );
        }
        "RoleBinding" | "ClusterRoleBinding" => {
            section(ui, "Subjects");
            let rows: Vec<_> = array(raw, "/subjects")
                .iter()
                .map(|subject| {
                    vec![
                        text(subject, "/kind"),
                        text(subject, "/name"),
                        text(subject, "/namespace"),
                    ]
                })
                .collect();
            table(
                ui,
                &["Kind", "Name", "Namespace"],
                &[0.26, 0.50, 0.24],
                &rows,
                "No subjects found",
            );
        }
        "NetworkPolicy" => {
            pills(
                ui,
                "Pod Selector",
                map_pairs(&raw["spec"]["podSelector"]["matchLabels"]),
                true,
            );
            for (title, path) in [
                ("Ingress Rules", "/spec/ingress"),
                ("Egress Rules", "/spec/egress"),
            ] {
                if let Some(rules) = raw.pointer(path).and_then(Value::as_array) {
                    row(ui, title, rules.len().to_string());
                }
            }
        }
        "StorageClass" => pills(ui, "Parameters", map_pairs(&raw["parameters"]), false),
        _ => {}
    }
}

pub(super) fn events(ui: &mut egui::Ui, snapshot: &Snapshot, resource: &Resource) {
    section(ui, "Events");
    let mut events: Vec<_> = snapshot
        .kind("Event")
        .into_iter()
        .filter(|event| event_for(event, resource))
        .collect();
    events.sort_by_key(|event| std::cmp::Reverse(event.created));
    let rows: Vec<_> = events
        .iter()
        .map(|event| {
            vec![
                event.status.clone(),
                text(&event.raw, "/reason"),
                text(&event.raw, "/message"),
                ResourceColumn::Age.value(event),
            ]
        })
        .collect();
    table(
        ui,
        &["Type", "Reason", "Message", "Age"],
        &[0.15, 0.23, 0.50, 0.12],
        &rows,
        "No events found",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expanded_environment_shows_values_without_showing_secret_data() {
        let snapshot = Snapshot {
            resources: vec![
                Resource::parse(
                    json!({"kind":"ConfigMap", "metadata":{"namespace":"test","name":"app"}, "data":{"mode":"staging","region":"local"}}),
                ),
                Resource::parse(
                    json!({"kind":"Secret", "metadata":{"namespace":"test","name":"auth"}, "data":{"token":"secret-fixture"}}),
                ),
            ],
            ..Default::default()
        };
        let container = json!({
            "env": [
                {"name":"PORT","value":"8080"},
                {"name":"MODE","valueFrom":{"configMapKeyRef":{"name":"app","key":"mode"}}},
                {"name":"TOKEN","valueFrom":{"secretKeyRef":{"name":"auth","key":"token"}}},
                {"name":"EMPTY"},
                {"name":"APP_mode","value":"override"}
            ],
            "envFrom": [{"prefix":"APP_","configMapRef":{"name":"app"}}]
        });
        let ctx = egui::Context::default();
        ctx.style_mut(|style| style.animation_time = 0.0);
        let mut secrets = EnvSecrets::default();
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        container_environment(
                            ui,
                            &snapshot,
                            "test",
                            &container,
                            None,
                            &mut secrets,
                        );
                    });
                },
            )
        };
        let pos = frame(vec![])
            .shapes
            .into_iter()
            .find_map(|shape| match shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "6 variables" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("environment expander");
        let pointer = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(pos), pointer(true)]);
        frame(vec![pointer(false)]);
        let labels: Vec<_> = frame(vec![])
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        for expected in [
            "PORT : 8080",
            "MODE : staging",
            "APP_region : local",
            "APP_mode : override",
            "EMPTY : (empty)",
            "TOKEN :",
            "secret(auth)",
        ] {
            assert!(
                labels.iter().any(|label| label == expected),
                "missing {expected}: {labels:?}"
            );
        }
        assert!(!labels
            .iter()
            .any(|label| label.contains("secret-fixture") || label == "APP_mode : staging"));
    }
}
