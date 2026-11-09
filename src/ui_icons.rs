use eframe::egui::{self, vec2, Pos2, Rect, Stroke};

pub fn button(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    enabled: bool,
    selected: bool,
) -> egui::Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .min_size(vec2(28.0, 26.0))
            .selected(selected),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, label)
    });
    let color = if enabled {
        ui.style().interact(&response).fg_stroke.color
    } else {
        ui.visuals().weak_text_color()
    };
    draw(
        ui.painter(),
        response.rect.center(),
        icon,
        Stroke::new(1.3_f32, color),
    );
    response.on_hover_text(label)
}

pub fn draw(painter: &egui::Painter, center: Pos2, icon: &str, stroke: Stroke) {
    let at = |x, y| center + vec2(x, y);
    let path = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(
            points.iter().map(|&(x, y)| at(x, y)).collect(),
            stroke,
        ))
    };
    let line = |x, y, x2, y2| painter.line_segment([at(x, y), at(x2, y2)], stroke);
    let rect = |a, b| {
        painter.rect_stroke(
            Rect::from_min_max(a, b),
            1.0,
            stroke,
            egui::StrokeKind::Inside,
        )
    };
    match icon {
        "close" => {
            line(-5., -5., 5., 5.);
            line(-5., 5., 5., -5.);
        }
        "more" => {
            for y in [-5., 0., 5.] {
                painter.circle_filled(at(0., y), 1.15, stroke.color);
            }
        }
        "logs" => {
            for (y, w) in [(-5., 6.), (0., 4.), (5., 6.)] {
                line(-6., y, w, y);
            }
        }
        "attach" => {
            rect(at(-7., -6.), at(7., 6.));
            painter.circle_stroke(at(-1., -1.), 2.8, stroke);
            line(1., 1., 4., 4.);
        }
        "share" => {
            line(-5., 0., 4., -5.);
            line(-5., 0., 4., 5.);
            for (x, y) in [(-5., 0.), (4., -5.), (4., 5.)] {
                painter.circle_filled(at(x, y), 1.65, stroke.color);
            }
        }
        "evict" => {
            rect(at(-6., -6.), at(6., 6.));
            line(-4., 0., 4., 0.);
            path(&[(1., -3.), (4., 0.), (1., 3.)]);
        }
        "terminal" => {
            rect(at(-7., -6.), at(7., 6.));
            path(&[(-5., -2.), (-2., 0.), (-5., 2.)]);
            line(0., 2., 4., 2.);
        }
        "filter" => {
            path(&[
                (-7., -5.),
                (7., -5.),
                (2., 0.),
                (2., 6.),
                (-2., 4.),
                (-2., 0.),
                (-7., -5.),
            ]);
        }
        "up" => {
            path(&[(-4., 2.), (0., -2.), (4., 2.)]);
        }
        "down" => {
            path(&[(-4., -2.), (0., 2.), (4., -2.)]);
        }
        "calendar" => {
            rect(at(-6., -5.), at(6., 6.));
            line(-6., -1., 6., -1.);
            for x in [-3., 3.] { line(x, -7., x, -3.); }
            for x in [-3., 0., 3.] { painter.circle_filled(at(x, 2.5), 0.6, stroke.color); }
        }
        "time" => {
            painter.circle_stroke(center, 6., stroke);
            path(&[(0., -3.), (0., 0.), (3., 1.)]);
        }
        "previous" => {
            path(&[(-7., -4.), (-7., 0.), (-3., 0.)]);
            painter.circle_stroke(at(1., 0.), 5.5, stroke);
            line(1., -3., 1., 1.);
            line(1., 1., 3., 1.);
        }
        "tag" => {
            path(&[
                (-6., -4.),
                (2., -4.),
                (6., 0.),
                (2., 4.),
                (-6., 4.),
                (-6., -4.),
            ]);
            painter.circle_filled(at(2., 0.), 1., stroke.color);
        }
        "wrap" => {
            line(-6., -4., 6., -4.);
            path(&[(-6., 0.), (5., 0.), (5., 5.), (-1., 5.), (1., 3.)]);
        }
        "latest" => {
            path(&[(-5., -5.), (0., 0.), (5., -5.)]);
            path(&[(-5., 0.), (0., 5.), (5., 0.)]);
        }
        "download" => {
            line(0., -6., 0., 2.);
            path(&[(-4., -1.), (0., 3.), (4., -1.)]);
            line(-6., 6., 6., 6.);
        }
        "expand" => {
            for (x, y) in [(-1., -1.), (-1., 1.), (1., -1.), (1., 1.)] {
                path(&[(2. * x, 6. * y), (6. * x, 6. * y), (6. * x, 2. * y)]);
            }
        }
        "restore" => {
            for (x, y) in [(-1., -1.), (-1., 1.), (1., -1.), (1., 1.)] {
                path(&[(6. * x, 2. * y), (2. * x, 2. * y), (2. * x, 6. * y)]);
            }
        }
        "edit" => {
            path(&[
                (-6., 6.),
                (-5., 2.),
                (3., -6.),
                (6., -3.),
                (-2., 5.),
                (-6., 6.),
            ]);
            line(1., -4., 4., -1.);
        }
        "delete" => {
            line(-6., -4., 6., -4.);
            line(-2., -6., 2., -6.);
            path(&[(-4., -2.), (-3., 6.), (3., 6.), (4., -2.)]);
        }
        _ => {}
    }
}
