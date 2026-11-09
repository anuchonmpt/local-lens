use crate::kubernetes::Target;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use eframe::egui;
use serde_json::Value;
use std::{collections::HashMap, sync::mpsc, thread};

type SecretValues = Result<Vec<(String, String)>, String>;

struct Reveal {
    pending: mpsc::Receiver<SecretValues>,
    value: Option<SecretValues>,
    seen: bool,
}

#[derive(Default)]
pub(super) struct EnvSecrets {
    reveals: HashMap<egui::Id, Reveal>,
}

impl EnvSecrets {
    pub fn end_frame(&mut self) {
        self.reveals
            .retain(|_, reveal| std::mem::take(&mut reveal.seen));
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        target: Option<&Target>,
        namespace: &str,
        secret: &str,
        key: Option<&str>,
        variable: &str,
        source: impl FnOnce(&mut egui::Ui),
    ) {
        let id = ui.make_persistent_id((namespace, secret, key, variable));
        let shown = self.reveals.contains_key(&id);
        let tooltip = format!("{} {variable}", if shown { "Hide" } else { "Reveal" });
        if let Some(reveal) = self.reveals.get_mut(&id) {
            reveal.seen = true;
            if reveal.value.is_none() {
                match reveal.pending.try_recv() {
                    Ok(value) => reveal.value = Some(value),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        reveal.value = Some(Err("Could not load Secret.".into()))
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
        }
        let value = self
            .reveals
            .get(&id)
            .and_then(|reveal| reveal.value.as_ref());
        if key.is_none() || !matches!(value, Some(Ok(_))) {
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
                Some(Ok(values)) => {
                    for (name, value) in values {
                        if key.is_none() {
                            ui.strong(name);
                        }
                        let value = if value.is_empty() { "(empty)" } else { value };
                        ui.add(
                            egui::Label::new(egui::RichText::new(value).color(crate::MUTED))
                                .wrap()
                                .selectable(true),
                        );
                    }
                    if values.is_empty() {
                        ui.label("(no keys)");
                    }
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
                let (sender, pending) = mpsc::channel();
                let (namespace, secret, key) = (
                    namespace.to_owned(),
                    secret.to_owned(),
                    key.map(str::to_owned),
                );
                let ctx = ui.ctx().clone();
                thread::spawn(move || {
                    let _ = sender.send(read_secret(&target, &namespace, &secret, key.as_deref()));
                    ctx.request_repaint();
                });
                self.reveals.insert(
                    id,
                    Reveal {
                        pending,
                        value: None,
                        seen: true,
                    },
                );
            }
        }
    }
}

fn read_secret(target: &Target, namespace: &str, name: &str, key: Option<&str>) -> SecretValues {
    let output = target
        .run(
            &[
                "get",
                "secret",
                "--namespace",
                namespace,
                "-o",
                "json",
                "--",
                name,
            ]
            .map(str::to_owned),
            None,
        )
        .map_err(|_| "Could not read Secret. Check your access and try again.".to_owned())?;
    decode_secret(&output, key)
}

fn decode_secret(output: &str, key: Option<&str>) -> SecretValues {
    let resource: Value =
        serde_json::from_str(output).map_err(|_| "Could not read Secret data.")?;
    let data = resource["data"].as_object();
    let decode = |name: &str, value: &Value| {
        let encoded = value.as_str().ok_or("Invalid Secret value.")?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid Secret value.")?;
        let text = String::from_utf8(bytes).unwrap_or_else(|_| "(non-text value)".into());
        Ok((name.to_owned(), text))
    };
    if let Some(key) = key {
        let value = data
            .and_then(|data| data.get(key))
            .ok_or("Key not found in Secret.")?;
        return Ok(vec![decode(key, value)?]);
    }
    data.into_iter()
        .flatten()
        .map(|(name, value)| decode(name, value))
        .collect()
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
    fn decoding_a_single_secret_key_handles_empty_values_and_ignores_other_keys() {
        let secret = r#"{"data":{"token":"Zml4dHVyZQ==","empty":"","other":"invalid base64"}}"#;
        assert_eq!(
            decode_secret(secret, Some("token")).unwrap(),
            vec![("token".into(), "fixture".into())]
        );
        assert_eq!(
            decode_secret(secret, Some("empty")).unwrap(),
            vec![("empty".into(), "".into())]
        );
        assert!(decode_secret(secret, Some("missing")).is_err());
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
            "#!/bin/sh\nprintf '%s' '{\"data\":{\"token\":\"Zml4dHVyZQ==\"}}'\n",
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
                        ui.horizontal(|ui| {
                            ui.label("TOKEN :");
                            secrets.show(
                                ui,
                                Some(&target),
                                "test",
                                "auth",
                                Some("token"),
                                "TOKEN",
                                |ui| {
                                    ui.label("secret(auth)");
                                },
                            );
                        });
                    });
                    secrets.end_frame();
                },
            )
        };
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
                assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == "secret(auth)")));
                break;
            }
            assert!(Instant::now() < deadline, "Secret value never appeared");
            thread::sleep(Duration::from_millis(10));
        }
        click(&mut secrets);
        assert!(secrets.reveals.is_empty());
    }
}
