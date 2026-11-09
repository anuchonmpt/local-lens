use crate::kubernetes::Target;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use eframe::egui;
use std::{collections::HashMap, sync::mpsc, thread};

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
}

impl EnvSecrets {
    pub fn end_frame(&mut self) {
        self.reveals
            .retain(|_, request| std::mem::take(&mut request.seen));
        self.key_lists
            .retain(|_, request| std::mem::take(&mut request.seen));
    }

    pub fn keys(
        &mut self,
        ui: &mut egui::Ui,
        target: &Target,
        namespace: &str,
        secret: &str,
    ) -> &[String] {
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
        match request.value.as_ref() {
            Some(Ok(keys)) => keys,
            Some(Err(error)) => {
                ui.colored_label(crate::ORANGE, format!("secret({secret}) · {error}"));
                &[]
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Loading keys from secret({secret})…"));
                });
                &[]
            }
        }
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
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid Secret value.")?;
    Ok(String::from_utf8(bytes).unwrap_or_else(|_| "(non-text value)".into()))
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
                        let keys = secrets.keys(ui, &target, "test", "auth").to_vec();
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
