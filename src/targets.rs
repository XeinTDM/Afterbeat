use crate::mouse::{Button, Point};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Step {
    Action(Box<ActionStep>),
    Control(Box<ControlStep>),
    Point(Point),
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("action").is_some() {
            serde_json::from_value(value)
                .map(|step| Self::Action(Box::new(step)))
                .map_err(D::Error::custom)
        } else if value.get("selector").is_some() {
            serde_json::from_value(value)
                .map(|step| Self::Control(Box::new(step)))
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Point)
                .map_err(D::Error::custom)
        }
    }
}

impl Step {
    pub fn is_smart(&self) -> bool {
        self.control().is_some()
    }

    pub fn point(&self) -> Option<Point> {
        match self {
            Self::Point(point) => Some(*point),
            Self::Action(step) => match &step.target {
                Some(Destination::Point(point)) => Some(*point),
                _ => None,
            },
            Self::Control(_) => None,
        }
    }

    pub fn control(&self) -> Option<&ControlStep> {
        match self {
            Self::Control(step) => Some(step),
            Self::Action(step) => match &step.target {
                Some(Destination::Control(control)) => Some(control),
                _ => None,
            },
            _ => None,
        }
    }

    #[cfg(windows)]
    pub fn control_mut(&mut self) -> Option<&mut ControlStep> {
        match self {
            Self::Control(step) => Some(step),
            Self::Action(step) => match &mut step.target {
                Some(Destination::Control(control)) => Some(control),
                _ => None,
            },
            _ => None,
        }
    }

    #[cfg(windows)]
    pub fn destination(&self) -> Option<Destination> {
        self.control()
            .cloned()
            .map(Box::new)
            .map(Destination::Control)
            .or_else(|| self.point().map(Destination::Point))
    }

    pub fn label(&self) -> String {
        match self {
            Self::Action(step) => {
                let mut label = match &step.action {
                    Action::Wait => format!("Wait {} ms", step.delay_ms),
                    Action::Click { button, count } => format!("{button:?} click ×{count}"),
                    Action::Scroll { direction, ticks } => format!("Scroll {direction:?} ×{ticks}"),
                    Action::Key { key } => format!("Press {}", display_label(key)),
                };
                if let Some(target) = &step.target {
                    label.push_str(" · ");
                    match target {
                        Destination::Point(point) => label.push_str(&crate::point_label(*point)),
                        Destination::Control(control) => {
                            label.push_str(&display_label(&control.selector.label))
                        }
                    }
                }
                if step.action != Action::Wait && step.delay_ms > 0 {
                    label.push_str(&format!(" · {} ms delay", step.delay_ms));
                }
                if self
                    .control()
                    .is_some_and(|control| control.wait_for_disappearance)
                {
                    label.push_str(" · Wait until gone");
                }
                label
            }
            Self::Point(point) => crate::point_label(*point),
            Self::Control(step) if step.wait_for_disappearance => {
                format!("{} · Wait until gone", display_label(&step.selector.label))
            }
            Self::Control(step) => display_label(&step.selector.label),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionStep {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Destination>,
    #[serde(default)]
    pub delay_ms: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Click { button: Button, count: u8 },
    Wait,
    Scroll { direction: Direction, ticks: u8 },
    Key { key: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub const ALL: [Self; 4] = [Self::Up, Self::Down, Self::Left, Self::Right];

    pub fn index(self) -> i32 {
        Self::ALL
            .iter()
            .position(|&direction| direction == self)
            .unwrap() as i32
    }

    pub fn from_index(index: i32) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or(Self::Down)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Destination {
    Control(Box<ControlStep>),
    Point(Point),
}

impl ActionStep {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.delay_ms > 600_000 {
            return Err("Set the step delay from 0 to 600,000 ms.");
        }
        match &self.action {
            Action::Wait if self.delay_ms == 0 || self.target.is_some() => {
                Err("A wait needs a positive duration and no target.")
            }
            Action::Click { count, .. } if self.target.is_none() || !(1..=3).contains(count) => {
                Err("A click needs a target and a count from 1 to 3.")
            }
            Action::Scroll { ticks, .. } if self.target.is_none() || !(1..=32).contains(ticks) => {
                Err("A scroll needs a target and a count from 1 to 32 wheel ticks.")
            }
            Action::Key { .. } if matches!(self.target, Some(Destination::Point(_))) => {
                Err("Use a control target or the foreground window for a key step.")
            }
            _ => Ok(()),
        }
    }
}

pub fn display_label(name: &str) -> String {
    let mut chars = name.chars();
    let mut label: String = chars
        .by_ref()
        .take(80)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ControlStep {
    pub selector: Selector,
    #[serde(default)]
    pub wait_for_disappearance: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Selector {
    pub window: WindowIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ControlIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual: Option<VisualIdentity>,
    pub label: String,
}

impl Selector {
    pub fn valid(&self) -> bool {
        self.control.is_some() || self.visual.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct VisualIdentity {
    pub anchor: ControlIdentity,
    pub target_name: String,
    pub anchor_size: Point,
    pub target_size: Point,
    pub offset: Point,
    pub target: Patch,
    pub landmark: Patch,
}

// Hex keeps each local grayscale fingerprint compact in settings and presets.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Patch(pub String);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WindowIdentity {
    pub executable: String,
    pub class: String,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ControlIdentity {
    pub role: i32,
    pub identity: Identity,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum Identity {
    AutomationId(String),
    Name(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_routes_keep_their_exact_coordinate_representation() {
        let json = r#"[{"x":-100,"y":25},{"x":2,"y":3}]"#;
        let steps: Vec<Step> = serde_json::from_str(json).unwrap();
        assert_eq!(steps[0].point(), Some(Point { x: -100, y: 25 }));
        assert_eq!(serde_json::to_string(&steps).unwrap(), json);
    }

    #[test]
    fn smart_steps_cannot_implicitly_fall_back_to_a_coordinate() {
        let json = r#"{"selector":{"window":{},"control":{},"label":"Save"}}"#;
        assert!(serde_json::from_str::<Step>(json).is_err());
    }

    #[test]
    fn saved_accessibility_controls_keep_their_identity() {
        let json = r#"{"selector":{"window":{"executable":"app.exe","class":"Window","title":"App"},"control":{"role":50000,"identity":{"AutomationId":"save"}},"label":"Save"},"wait_for_disappearance":true}"#;
        let Step::Control(step) = serde_json::from_str(json).unwrap() else {
            panic!("expected control")
        };
        assert!(
            step.selector.valid() && step.selector.visual.is_none() && step.wait_for_disappearance
        );
        assert_eq!(serde_json::to_string(&*step).unwrap(), json);
    }

    #[test]
    fn malformed_action_does_not_deserialize_as_a_legacy_point() {
        let json = r#"{"x":8,"y":9,"action":{"kind":"future"},"delay_ms":120}"#;
        assert!(serde_json::from_str::<Step>(json).is_err());
        let json = r#"{"action":{"kind":"scroll","direction":"Down","ticks":3},"target":{"x":8,"y":9},"delay_ms":120}"#;
        let step: Step = serde_json::from_str(json).unwrap();
        assert_eq!(step.point(), Some(Point { x: 8, y: 9 }));
        assert_eq!(serde_json::to_string(&step).unwrap(), json);
    }
}
