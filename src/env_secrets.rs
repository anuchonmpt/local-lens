use crate::{kubernetes::Target, Resource};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use eframe::egui;
use std::{
    collections::{BTreeMap, HashMap},
    sync::mpsc,
    thread,
};

struct Request<T> {
    pending: mpsc::Receiver<Result<T, String>>,
    value: Option<Result<T, String>>,
    seen: bool,
}

impl<T: Send + 'static> Request<T> {
    fn start(
        ctx: &egui::Context,
        read: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Self {
        let (sender, pending) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = sender.send(read());
            ctx.request_repaint();
        });
        Self {
            pending,
            value: None,
            seen: true,
        }
    }

    fn poll(&mut self) {
        self.seen = true;
        if self.value.is_none() {
            match self.pending.try_recv() {
                Ok(value) => self.value = Some(value),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.value = Some(Err("Could not load Secret.".into()));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }
}

#[derive(Default)]
pub(super) struct EnvSecrets {
    reveals: HashMap<egui::Id, Request<String>>,
    key_lists: HashMap<egui::Id, Request<Vec<String>>>,
    details: HashMap<egui::Id, Request<SecretData>>,
}

struct SecretData {
    encoded: BTreeMap<String, String>,
    decoded: HashMap<String, Result<Option<String>, String>>,
}

impl EnvSecrets {
    pub fn end_frame(&mut self) {
        self.reveals
            .retain(|_, request| std::mem::take(&mut request.seen));
        self.key_lists
            .retain(|_, request| std::mem::take(&mut request.seen));
        self.details
            .retain(|_, request| std::mem::take(&mut request.seen));
    }

    pub fn show_data(&mut self, ui: &mut egui::Ui, target: Option<&Target>, secret: &Resource) {
        let Some(target) = target else {
            ui.label("Connect to a cluster to read Secret data.");
            return;
        };
        let id = ui.make_persistent_id((
            "secret-data",
            &target.config,
            &target.context,
            &secret.namespace,
            &secret.name,
            secret
                .raw
                .pointer("/metadata/uid")
                .and_then(serde_json::Value::as_str),
        ));
        let request = self.details.entry(id).or_insert_with(|| {
            let target = target.clone();
            let secret = secret.clone();
            Request::start(ui.ctx(), move || {
                let output = secret_output(&target, &secret.namespace, &secret.name, "json")?;
                let mut loaded: serde_json::Value =
                    serde_json::from_str(&output).map_err(|_| "Invalid Secret response.")?;
                if !super::same_object(&secret.raw, &loaded) {
                    return Err("This Secret changed. Refresh and reopen its details.".into());
                }
                let encoded = match loaded.get_mut("data").map(serde_json::Value::take) {
                    None | Some(serde_json::Value::Null) => BTreeMap::new(),
                    Some(data) => {
                        serde_json::from_value(data).map_err(|_| "Invalid Secret data.")?
                    }
                };
                Ok(SecretData {
                    encoded,
                    decoded: HashMap::new(),
                })
            })
        });
        request.poll();
        match request.value.as_mut() {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading Secret data…");
                });
            }
            Some(Err(error)) => {
                ui.colored_label(crate::ORANGE, error.as_str());
                if ui.button("Retry").clicked() {
                    self.details.remove(&id);
                    ui.ctx().request_repaint();
                }
            }
            Some(Ok(data)) => data.show(ui),
        }
    }

    pub fn keys(
        &mut self,
        ui: &egui::Ui,
        target: &Target,
        namespace: &str,
        secret: &str,
    ) -> Option<Result<&[String], &str>> {
        let id = ui.make_persistent_id(("secret-keys", namespace, secret));
        let request = self.key_lists.entry(id).or_insert_with(|| {
            let (target, namespace, secret) =
                (target.clone(), namespace.to_owned(), secret.to_owned());
            Request::start(ui.ctx(), move || {
                // Return only key names from kubectl; values are read on a row's reveal action.
                let names = secret_output(
                    &target,
                    &namespace,
                    &secret,
                    r#"go-template={{range $key, $_ := .data}}{{println $key}}{{end}}"#,
                )?;
                Ok(names.lines().map(str::to_owned).collect())
            })
        });
        request.poll();
        request
            .value
            .as_ref()
            .map(|result| result.as_ref().map(Vec::as_slice).map_err(String::as_str))
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        target: Option<&Target>,
        namespace: &str,
        secret: &str,
        key: &str,
        variable: &str,
        source: impl FnOnce(&mut egui::Ui),
    ) {
        let id = ui.make_persistent_id((namespace, secret, key, variable));
        let shown = self.reveals.contains_key(&id);
        let tooltip = format!("{} {variable}", if shown { "Hide" } else { "Reveal" });
        if let Some(reveal) = self.reveals.get_mut(&id) {
            reveal.poll();
        }
        let value = self
            .reveals
            .get(&id)
            .and_then(|reveal| reveal.value.as_ref());
        if !matches!(value, Some(Ok(_))) {
            source(ui);
        }
        if shown {
            match value {
                None => {
                    ui.spinner();
                }
                Some(Err(error)) => {
                    ui.colored_label(crate::ORANGE, error);
                }
                Some(Ok(value)) => {
                    let value = if value.is_empty() { "(empty)" } else { value };
                    ui.add(
                        egui::Label::new(egui::RichText::new(value).color(crate::MUTED))
                            .wrap()
                            .selectable(true),
                    );
                }
            }
        }
        if eye(
            ui,
            target.is_some() && !namespace.is_empty(),
            shown,
            &tooltip,
        ) {
            if shown {
                self.reveals.remove(&id);
                ui.ctx().request_repaint();
            } else if let Some(target) = target.cloned() {
                let (namespace, secret, key) =
                    (namespace.to_owned(), secret.to_owned(), key.to_owned());
                let reveal = Request::start(ui.ctx(), move || {
                    let key_literal =
                        serde_json::to_string(&key).map_err(|_| "Invalid Secret key.")?;
                    let format = r#"go-template={{range $key, $value := .data}}{{if eq $key KEY_LITERAL}}{{println $value}}{{end}}{{end}}"#
                        .replace("KEY_LITERAL", &key_literal);
                    let output = secret_output(&target, &namespace, &secret, &format)?;
                    decode_secret(&output)
                });
                self.reveals.insert(id, reveal);
            }
        }
    }
}

impl SecretData {
    fn show(&mut self, ui: &mut egui::Ui) {
        if self.encoded.is_empty() {
            ui.label("No data keys.");
        }
        for (key, encoded) in &self.encoded {
            ui.push_id(key, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(crate::MUTED, key);
                    let shown = self.decoded.contains_key(key);
                    let hint = if shown { "Show Base64 for" } else { "Decode" };
                    if eye(ui, true, shown, &format!("{hint} {key}")) {
                        if shown {
                            self.decoded.remove(key);
                        } else {
                            self.decoded.insert(key.clone(), decode_base64(encoded));
                        }
                    }
                });
                let value = match self.decoded.get(key) {
                    Some(Ok(Some(value))) => value,
                    Some(Ok(None)) => {
                        ui.colored_label(crate::MUTED, "Binary data · shown as Base64");
                        encoded
                    }
                    Some(Err(error)) => {
                        ui.colored_label(crate::ORANGE, error);
                        encoded
                    }
                    None => encoded,
                };
                let value = if value.is_empty() { "(empty)" } else { value };
                egui::Frame::new()
                    .fill(crate::BG)
                    .stroke(egui::Stroke::new(1.0_f32, crate::BORDER))
                    .corner_radius(3.0)
                    .inner_margin(7.0)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.add(
                            egui::Label::new(egui::RichText::new(value).monospace())
                                .wrap()
                                .selectable(true),
                        );
                    });
                ui.add_space(6.0);
            });
        }
    }
}

fn secret_output(
    target: &Target,
    namespace: &str,
    name: &str,
    format: &str,
) -> Result<String, String> {
    target
        .run(
            &[
                "get",
                "secret",
                "--namespace",
                namespace,
                "-o",
                format,
                "--",
                name,
            ]
            .map(str::to_owned),
            None,
        )
        .map_err(|_| "Could not read Secret. Check your access.".to_owned())
}

fn decode_secret(output: &str) -> Result<String, String> {
    let encoded = output
        .strip_suffix('\n')
        .ok_or("Key not found in Secret.")?;
    Ok(decode_base64(encoded)?.unwrap_or_else(|| "(non-text value)".into()))
}

fn decode_base64(encoded: &str) -> Result<Option<String>, String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid Base64 value.")?;
    Ok(String::from_utf8(bytes).ok())
}

fn eye(ui: &mut egui::Ui, enabled: bool, shown: bool, label: &str) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(19.0, 17.0), egui::Sense::click());
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
        let color = ui.style().interact(&response).text_color();
        let stroke = egui::Stroke::new(1.2_f32, color);
        let center = rect.center();
        let points = [
            (-7.0, 0.0),
            (-4.0, -3.0),
            (0.0, -4.0),
            (4.0, -3.0),
            (7.0, 0.0),
            (4.0, 3.0),
            (0.0, 4.0),
            (-4.0, 3.0),
        ];
        ui.painter().add(egui::Shape::closed_line(
            points.map(|(x, y)| center + egui::vec2(x, y)).to_vec(),
            stroke,
        ));
        ui.painter().circle_filled(center, 1.8, color);
        if shown {
            ui.painter().line_segment(
                [
                    center + egui::vec2(-6.0, 5.0),
                    center + egui::vec2(6.0, -5.0),
                ],
                stroke,
            );
        }
        response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(label)
            .clicked()
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_a_secret_distinguishes_empty_and_missing_values() {
        assert_eq!(decode_secret("Zml4dHVyZQ==\n").unwrap(), "fixture");
        assert_eq!(decode_secret("\n").unwrap(), "");
        assert!(decode_secret("").is_err());
    }

    #[test]
    fn secret_data_switches_only_the_clicked_key_between_base64_and_text() {
        let mut data = SecretData {
            encoded: BTreeMap::from([
                ("first".into(), "Zml4dHVyZQ==".into()),
                ("second".into(), "b3RoZXI=".into()),
            ]),
            decoded: HashMap::new(),
        };
        let ctx = egui::Context::default();
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| data.show(ui));
                },
            )
        };
        let has_label = |output: &egui::FullOutput, label: &str| {
            output.shapes.iter().any(|shape| {
                matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == label)
            })
        };
        let initial = frame(vec![]);
        assert!(has_label(&initial, "Zml4dHVyZQ=="));
        assert!(has_label(&initial, "b3RoZXI="));
        assert!(!has_label(&initial, "fixture"));
        let pos = initial
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Circle(circle) if (circle.radius - 1.8).abs() < 0.01 => {
                    Some(circle.center)
                }
                _ => None,
            })
            .expect("first key's eye button");
        let pointer = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(pos), pointer(true)]);
        let revealed = frame(vec![pointer(false)]);
        assert!(has_label(&revealed, "fixture"));
        assert!(has_label(&revealed, "b3RoZXI="));
        assert!(!has_label(&revealed, "other"));
        frame(vec![pointer(true)]);
        let hidden = frame(vec![pointer(false)]);
        assert!(has_label(&hidden, "Zml4dHVyZQ=="));
        assert!(!has_label(&hidden, "fixture"));
    }

    #[cfg(unix)]
    #[test]
    fn eye_loads_a_secret_only_on_click_and_forgets_it_when_hidden() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };

        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("kubectl");
        fs::write(
            &exe,
            r#"#!/bin/sh
case "$*" in
    *'eq $key "token"'*) printf '%s\n' Zml4dHVyZQ== ;;
    *'eq $key "other"'*) printf '%s\n' bm90LXJldmVhbGVk ;;
    *'println $key'*) printf '%s\n' token other ;;
    *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let target = Target {
            exe: exe.to_str().unwrap().into(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut secrets = EnvSecrets::default();
        let frame = |secrets: &mut EnvSecrets, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let keys = secrets
                            .keys(ui, &target, "test", "auth")
                            .and_then(Result::ok)
                            .unwrap_or_default()
                            .to_vec();
                        for key in keys {
                            ui.push_id(&key, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(format!("APP_{key} :"));
                                    secrets.show(
                                        ui,
                                        Some(&target),
                                        "test",
                                        "auth",
                                        &key,
                                        &key,
                                        |ui| {
                                            ui.label("secret(auth)");
                                        },
                                    );
                                });
                            });
                        }
                    });
                    secrets.end_frame();
                },
            )
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = frame(&mut secrets, vec![]);
            let rows: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text().starts_with("APP_") => {
                        Some(text.pos.y)
                    }
                    _ => None,
                })
                .collect();
            if rows.len() == 2 {
                assert!(rows[0] < rows[1], "each Secret key must have its own row");
                break;
            }
            assert!(Instant::now() < deadline, "Secret key rows never appeared");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(secrets.reveals.is_empty());
        let click = |secrets: &mut EnvSecrets| {
            let pos = frame(secrets, vec![])
                .shapes
                .into_iter()
                .find_map(|shape| match shape.shape {
                    egui::Shape::Circle(circle) if (circle.radius - 1.8).abs() < 0.01 => {
                        Some(circle.center)
                    }
                    _ => None,
                })
                .expect("eye button");
            let event = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(secrets, vec![egui::Event::PointerMoved(pos), event(true)]);
            frame(secrets, vec![event(false)]);
        };
        click(&mut secrets);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = frame(&mut secrets, vec![]);
            if output.shapes.iter().any(|shape| {
                matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == "fixture")
            }) {
                assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == "secret(auth)")));
                assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == "not-revealed")));
                break;
            }
            assert!(Instant::now() < deadline, "Secret value never appeared");
            thread::sleep(Duration::from_millis(10));
        }
        click(&mut secrets);
        assert!(secrets.reveals.is_empty());
    }
}
