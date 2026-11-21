use crate::{
    App, DryArea, DryPoint,
    automation::spatial_gate,
    engine::Gate,
    mouse::{self, Point, Zone},
    point_label,
    settings::Settings,
    shared_output_label,
    targets::{Action, Step},
};
use slint::{ModelRc, VecModel};

pub fn can_sample(settings: &Settings) -> bool {
    #[cfg(target_os = "linux")]
    if crate::wayland::active() {
        return false;
    }
    !settings.route_enabled && (settings.keyboard || !settings.fixed)
}

pub fn show(ui: &App, settings: &Settings, sample: Option<Point>) -> Result<(), &'static str> {
    show_targets(ui, settings, sample, |_, _| Preview {
        point: None,
        bounds: None,
        landmark: None,
        message: "Check the desktop to find this control.",
        repair: false,
        blocked: true,
    })?;
    ui.invoke_show_dry_run();
    Ok(())
}

#[cfg(windows)]
pub fn show_smart(
    ui: &App,
    settings: &Settings,
    results: &[crate::smart_targets::Resolution],
) -> Result<(), &'static str> {
    use crate::smart_targets::Resolution;
    show_targets(ui, settings, None, |index, key| {
        let result = results.get(index).unwrap_or(&Resolution::Unavailable);
        let (blocked, message) = result.preview_status(key);
        let (point, bounds, landmark) = match result {
            Resolution::Ready(target)
            | Resolution::Recovered(target)
            | Resolution::Conflict(target) => (
                Some(target.point),
                Some(target.bounds),
                target.landmark_bounds(),
            ),
            _ => (None, None, None),
        };
        Preview {
            point,
            bounds,
            landmark,
            message,
            repair: matches!(result, Resolution::Recovered(_) | Resolution::Conflict(_)),
            blocked,
        }
    })
}

struct Preview {
    point: Option<Point>,
    bounds: Option<Zone>,
    landmark: Option<Zone>,
    message: &'static str,
    repair: bool,
    blocked: bool,
}

fn show_targets(
    ui: &App,
    settings: &Settings,
    sample: Option<Point>,
    preview: impl Fn(usize, bool) -> Preview,
) -> Result<(), &'static str> {
    let displays = mouse::display_areas();
    let Some(first) = displays.first() else {
        #[cfg(target_os = "linux")]
        if crate::wayland::active() {
            return Err("Enable Wayland input with displays to preview logical targets.");
        }
        return Err("The desktop did not report any displays for the preview.");
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
    let mut areas: Vec<_> = displays
        .into_iter()
        .map(|zone| area(zone, 0))
        .chain(settings.inside_zone.map(|zone| area(zone, 1)))
        .chain(settings.stop_zone.map(|zone| area(zone, 2)))
        .collect();

    let local_points = if settings.route_enabled {
        Vec::new()
    } else if settings.fixed && !settings.keyboard {
        settings.point.into_iter().map(Step::Point).collect()
    } else {
        sample.into_iter().map(Step::Point).collect()
    };
    let steps = if settings.route_enabled {
        &settings.route
    } else {
        &local_points
    };
    let radius =
        if settings.route_enabled && !settings.has_smart_route() && !settings.has_action_route() {
            settings.radius().unwrap_or_default() as f32
        } else {
            0.0
        };
    let shared_output = slint::SharedString::from(shared_output_label(settings));
    let points: Vec<_> = steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let counts_as_action = match step {
                Step::Action(action) => match &action.action {
                    Action::Wait | Action::WaitColor { .. } | Action::Flow { .. }
                    | Action::WaitImage { output: crate::targets::ImageOutput::Wait | crate::targets::ImageOutput::If, .. }
                    | Action::Recorded { event: crate::recording::Event::Move(_) } => false,
                    Action::Recorded { event: crate::recording::Event::Scroll { .. } } => true,
                    Action::Recorded { event: crate::recording::Event::Button { down, .. } | crate::recording::Event::Key { down, .. } } => *down,
                    _ => true,
                },
                _ => true,
            };
            let action_count = match step {
                Step::Action(action) => match &action.action {
                    Action::Text { text } => text.chars().count().min(i32::MAX as usize) as i32,
                    _ => i32::from(counts_as_action),
                },
                _ => 1,
            };
            let inherited_output = if settings.route_enabled && !matches!(step, Step::Action(_)) {
                shared_output.clone()
            } else {
                Default::default()
            };
            let key = matches!(step, Step::Action(step) if matches!(step.action, Action::Key { .. } | Action::Text { .. } | Action::Recorded { event: crate::recording::Event::Key { .. } }));
            let preview = if let Some(point) = step.point() {
                Preview {
                    point: Some(point),
                    bounds: None,
                    landmark: None,
                    message: "Spatial checks passed.",
                    repair: false,
                    blocked: false,
                }
            } else if step.is_smart() {
                preview(index, key)
            } else {
                Preview {
                    point: None,
                    bounds: None,
                    landmark: None,
                    repair: false,
                    blocked: matches!(step, Step::Action(action) if matches!(action.action, Action::Flow { .. } | Action::WaitImage { output: crate::targets::ImageOutput::If, .. })),
                    message: if matches!(step, Step::Action(action) if action.action == Action::Wait) {
                        "Waits without sending input."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::Flow { .. })) {
                        "Dry run stops here because workflow branches and loops are evaluated during playback."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitImage { .. })) {
                        "The image is not searched here; use the image preview in the step editor."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { retry_clicks: 1.., .. })) {
                        "The pixel is not checked here. Failure can retry the preceding click before the selected failure action."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { check_once: true, skip_next_on_mismatch: true, .. })) {
                        "The pixel is not checked here. If it does not match, the next step will be skipped."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { check_once: true, .. })) {
                        "Checks a pixel once during the run; the color is not checked here."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { skip_next_on_timeout: true, .. })) {
                        "The pixel is not checked here. On timeout the next step will be skipped."
                    } else if matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { .. })) {
                        "Waits for a pixel color during the run; the color is not checked here."
                    } else {
                        "Uses the focused app when the step runs."
                    },
                }
            };
            let Some(point) = preview.point else {
                return DryPoint {
                    blocked: preview.blocked,
                    counts_as_action,
                    action_count,
                    hidden: true,
                    location: step.label().into(),
                    inherited_output,
                    result: preview.message.into(),
                    ..Default::default()
                };
            };
            if let Some(bounds) = preview.bounds {
                areas.push(area(bounds, 3));
            }
            if let Some(landmark) = preview.landmark {
                areas.push(area(landmark, 4));
            }
            let (blocked, terminal, message) = if preview.blocked || key {
                // A pending control match has no confirmed destination. Keys use focus.
                (preview.blocked, false, preview.message)
            } else if mouse::on_monitor(point) {
                match spatial_gate(
                    point,
                    settings.edge_mode,
                    settings.inside_zone,
                    settings.stop_zone,
                ) {
                    Gate::Open => (preview.blocked, false, preview.message),
                    Gate::Paused(_) => (true, false, "Would pause outside the inside-only zone."),
                    Gate::Stop(reason) => (true, true, reason),
                }
            } else {
                (true, true, "Point is outside your displays.")
            };
            DryPoint {
                x: x(point.x),
                y: y(point.y),
                radius_x: radius / width,
                radius_y: radius / height,
                blocked,
                terminal,
                counts_as_action,
                action_count,
                hidden: false,
                repair: preview.repair,
                location: if !matches!(step, Step::Point(_)) {
                    step.label()
                } else {
                    point_label(point)
                }
                .into(),
                inherited_output,
                result: message.into(),
            }
        })
        .collect();
    ui.set_dry_sample_allowed(can_sample(settings));
    ui.set_dry_source(
        if settings.has_action_route() {
            "Route actions and targets"
        } else if settings.has_smart_route() {
            "Control targets and saved points"
        } else if settings.route_enabled {
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
    ui.set_dry_action_limit(settings.action_limit().ok().flatten().unwrap_or(0) as i32);
    ui.set_dry_aspect(width / height);
    Ok(())
}
