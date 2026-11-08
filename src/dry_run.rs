use crate::{
    App, DryArea, DryPoint,
    automation::spatial_gate,
    engine::Gate,
    mouse::{self, Point, Zone},
    point_label,
    settings::Settings,
};
use slint::{ModelRc, VecModel};

pub fn can_sample(settings: &Settings) -> bool {
    !settings.route_enabled && (settings.keyboard || !settings.fixed)
}

pub fn show(ui: &App, settings: &Settings, sample: Option<Point>) -> Result<(), &'static str> {
    let displays = mouse::display_areas();
    let Some(first) = displays.first() else {
        return Err("Windows did not report any displays for the preview.");
    };
    let desktop = displays.iter().fold(*first, |bounds, display| Zone {
        from: Point {
            x: bounds.from.x.min(display.from.x),
            y: bounds.from.y.min(display.from.y),
        },
        to: Point {
            x: bounds.to.x.max(display.to.x),
            y: bounds.to.y.max(display.to.y),
        },
    });
    let width = (i64::from(desktop.to.x) - i64::from(desktop.from.x) + 1) as f32;
    let height = (i64::from(desktop.to.y) - i64::from(desktop.from.y) + 1) as f32;
    let x = |value| ((i64::from(value) - i64::from(desktop.from.x)) as f32 / width).clamp(0.0, 1.0);
    let y =
        |value| ((i64::from(value) - i64::from(desktop.from.y)) as f32 / height).clamp(0.0, 1.0);
    let area = |zone: Zone, kind| {
        let left = x(zone.from.x.min(zone.to.x));
        let top = y(zone.from.y.min(zone.to.y));
        DryArea {
            x: left,
            y: top,
            w: x(zone.from.x.max(zone.to.x).saturating_add(1)) - left,
            h: y(zone.from.y.max(zone.to.y).saturating_add(1)) - top,
            kind,
        }
    };
    let areas: Vec<_> = displays
        .into_iter()
        .map(|zone| area(zone, 0))
        .chain(settings.inside_zone.map(|zone| area(zone, 1)))
        .chain(settings.stop_zone.map(|zone| area(zone, 2)))
        .collect();

    let points = if settings.route_enabled {
        settings.route.clone()
    } else if settings.fixed && !settings.keyboard {
        settings.point.into_iter().collect()
    } else {
        sample.into_iter().collect()
    };
    let radius = if settings.route_enabled {
        settings.radius().unwrap_or_default() as f32
    } else {
        0.0
    };
    let points: Vec<_> = points
        .into_iter()
        .map(|point| {
            let (blocked, message) = if mouse::on_monitor(point) {
                match spatial_gate(
                    point,
                    settings.edge_mode,
                    settings.inside_zone,
                    settings.stop_zone,
                ) {
                    Gate::Open => (false, "Spatial checks passed."),
                    Gate::Paused(_) => (true, "Would pause outside the inside-only zone."),
                    Gate::Stop(reason) => (true, reason),
                }
            } else {
                (true, "Point is outside your displays.")
            };
            DryPoint {
                x: x(point.x),
                y: y(point.y),
                radius_x: radius / width,
                radius_y: radius / height,
                blocked,
                location: point_label(point).into(),
                result: message.into(),
            }
        })
        .collect();
    ui.set_dry_sample_allowed(can_sample(settings));
    ui.set_dry_source(
        if settings.route_enabled {
            "Saved route centers"
        } else if settings.keyboard {
            "Pointer sample for keyboard safety zones"
        } else if settings.fixed {
            "Saved fixed point"
        } else {
            "Follow cursor sample"
        }
        .into(),
    );
    ui.set_dry_areas(ModelRc::new(VecModel::from(areas)));
    ui.set_dry_points(ModelRc::new(VecModel::from(points)));
    ui.set_dry_aspect(width / height);
    ui.invoke_show_dry_run();
    Ok(())
}
