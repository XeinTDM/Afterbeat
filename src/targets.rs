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
                _ => match &step.action {
                    Action::Recorded {
                        event:
                            crate::recording::Event::Move(point)
                            | crate::recording::Event::Scroll { point, .. }
                            | crate::recording::Event::Button { point, .. },
                    } => Some(*point),
                    _ => None,
                },
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
                    Action::Text { text } => format!("Type {}", display_label(text)),
                    Action::WaitImage {
                        search,
                        threshold,
                        timeout_ms,
                        output,
                        ..
                    } => format!(
                        "{} image in {}, {}, {} × {} · {}% · {} ms timeout",
                        output.label(),
                        search.x,
                        search.y,
                        search.width,
                        search.height,
                        threshold,
                        timeout_ms
                    ),
                    Action::Flow { operation } => operation.label(),
                    Action::WaitColor {
                        point,
                        rgb,
                        tolerance,
                        different,
                        stable_ms,
                        skip_next_on_timeout,
                        check_once,
                        skip_next_on_mismatch,
                        retry_clicks,
                    } => format!(
                        "{} pixel {}, {} · {} #{:06X} · tolerance {} · stable {} ms · retries {}{}",
                        if *check_once { "Check" } else { "Wait for" },
                        point.x,
                        point.y,
                        if *different { "outside" } else { "matches" },
                        rgb,
                        tolerance,
                        stable_ms,
                        retry_clicks,
                        if *skip_next_on_mismatch {
                            " · mismatch skips next step"
                        } else if *skip_next_on_timeout {
                            " · timeout skips next step"
                        } else {
                            ""
                        }
                    ),
                    Action::Recorded { event } => match event {
                        crate::recording::Event::Move(point) => {
                            format!("Move · {}", crate::point_label(*point))
                        }
                        crate::recording::Event::Scroll {
                            point,
                            horizontal,
                            delta,
                        } => format!(
                            "Scroll {} {delta} · {}",
                            if *horizontal {
                                "horizontal"
                            } else {
                                "vertical"
                            },
                            crate::point_label(*point)
                        ),
                        crate::recording::Event::Button {
                            button,
                            down,
                            point,
                        } => format!(
                            "{button:?} {} · {}",
                            if *down { "down" } else { "up" },
                            crate::point_label(*point)
                        ),
                        crate::recording::Event::Key {
                            code,
                            extended,
                            down,
                        } => format!(
                            "Key {code}{} {}",
                            if *extended { " (extended)" } else { "" },
                            if *down { "down" } else { "up" }
                        ),
                    },
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

pub fn has_retryable_previous_click(route: &[Step], index: usize) -> bool {
    index.checked_sub(1).and_then(|previous| route.get(previous)).is_some_and(|step| {
        matches!(step, Step::Action(action) if matches!(action.action, Action::Click { .. }))
    })
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
    Click {
        button: Button,
        count: u8,
    },
    Wait,
    Scroll {
        direction: Direction,
        ticks: u8,
    },
    Key {
        key: String,
    },
    Text {
        text: String,
    },
    WaitImage {
        search: ImageRect,
        template: ImageTemplate,
        threshold: u8,
        timeout_ms: u32,
        #[serde(default)]
        output: ImageOutput,
        #[serde(default)]
        scaled: bool,
    },
    Flow {
        operation: crate::workflow::Operation,
    },
    WaitColor {
        point: Point,
        rgb: u32,
        #[serde(default)]
        tolerance: u8,
        #[serde(default)]
        different: bool,
        #[serde(default)]
        stable_ms: u32,
        #[serde(default)]
        skip_next_on_timeout: bool,
        #[serde(default)]
        check_once: bool,
        #[serde(default)]
        skip_next_on_mismatch: bool,
        #[serde(default)]
        retry_clicks: u8,
    },
    Recorded {
        event: crate::recording::Event,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageOutput {
    #[default]
    Wait,
    Move,
    Click,
    If,
}

impl ImageOutput {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wait => "Wait for",
            Self::Move => "Move to",
            Self::Click => "Click",
            Self::If => "If",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ImageRect {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl ImageRect {
    pub fn valid(self, limit: u16) -> bool {
        (1..=limit).contains(&self.width)
            && (1..=limit).contains(&self.height)
            && self.x.checked_add(i32::from(self.width) - 1).is_some()
            && self.y.checked_add(i32::from(self.height) - 1).is_some()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ImageTemplate {
    pub width: u16,
    pub height: u16,
    pub pixels: String,
}

impl ImageTemplate {
    pub fn is_color(&self) -> bool {
        (8..=64).contains(&self.width)
            && (8..=64).contains(&self.height)
            && self.pixels.len() == usize::from(self.width) * usize::from(self.height) * 6
    }

    pub fn valid(&self) -> bool {
        (8..=64).contains(&self.width)
            && (8..=64).contains(&self.height)
            && (self.pixels.len() == usize::from(self.width) * usize::from(self.height) * 2
                || self.is_color())
            && self.pixels.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
}

pub fn parse_image_rect(value: &str) -> Option<ImageRect> {
    let mut parts = value.split(',').map(str::trim);
    let rect = ImageRect {
        x: parts.next()?.parse().ok()?,
        y: parts.next()?.parse().ok()?,
        width: parts.next()?.parse().ok()?,
        height: parts.next()?.parse().ok()?,
    };
    parts.next().is_none().then_some(rect)
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

pub fn parse_pixel(value: &str) -> Option<(Point, u32)> {
    let mut parts = value.split(',').map(str::trim);
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let color = parts.next()?.strip_prefix('#')?;
    if parts.next().is_some() || color.len() != 6 || !color.is_ascii() {
        return None;
    }
    Some((Point { x, y }, u32::from_str_radix(color, 16).ok()?))
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
        if matches!(self.action, Action::Recorded { .. }) && self.delay_ms > 86_400_000 {
            return Err("Set the recorded delay from 0 to 86,400,000 ms.");
        }
        if !matches!(self.action, Action::Recorded { .. }) && self.delay_ms > 600_000 {
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
            Action::Text { text }
                if self.target.is_some()
                    || text.is_empty()
                    || text.chars().count() > 256
                    || text.chars().any(char::is_control) =>
            {
                Err(
                    "Text needs 1–256 printable characters and the foreground window as its target.",
                )
            }
            Action::WaitImage {
                search,
                template,
                threshold,
                timeout_ms,
                ..
            } if self.target.is_some()
                || !search.valid(512)
                || !template.valid()
                || search.width < template.width
                || search.height < template.height
                || !(70..=100).contains(threshold)
                || !(100..=600_000).contains(timeout_ms) =>
            {
                Err(
                    "Image wait needs a valid captured image, a search region up to 512 × 512, a threshold from 70 to 100%, and a timeout from 100 to 600,000 ms.",
                )
            }
            Action::Flow { operation } => operation.validate(self.target.is_some(), self.delay_ms),
            Action::WaitColor { rgb, .. } if self.target.is_some() || *rgb > 0xffffff => {
                Err("A pixel wait needs a screen coordinate and an RGB color.")
            }
            Action::WaitColor {
                tolerance: 255,
                different: true,
                ..
            } => Err("Different cannot match with a tolerance of 255. Choose 0 to 254."),
            Action::WaitColor { stable_ms, .. } if *stable_ms > 60_000 => {
                Err("Set pixel stability from 0 to 60,000 ms.")
            }
            Action::WaitColor {
                check_once: true,
                stable_ms,
                ..
            } if *stable_ms != 0 => Err("A one-time pixel check requires zero stability."),
            Action::WaitColor {
                check_once: true,
                skip_next_on_timeout: true,
                ..
            }
            | Action::WaitColor {
                check_once: false,
                skip_next_on_mismatch: true,
                ..
            } => Err("Choose a failure action for the selected pixel check mode."),
            Action::WaitColor { retry_clicks, .. } if *retry_clicks > 10 => {
                Err("Set click retries from 0 to 10.")
            }
            Action::Recorded { .. } if self.target.is_some() => {
                Err("A recorded event cannot have a separate target.")
            }
            Action::Recorded {
                event: crate::recording::Event::Key { code, .. },
            } if *code > 255 => Err("Enter a valid recorded key code."),
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
    fn image_steps_validate_size_threshold_timeout_and_payload() {
        let template = ImageTemplate {
            width: 8,
            height: 8,
            pixels: "ab".repeat(64),
        };
        let search = parse_image_rect(" -50, 10, 80, 50 ").unwrap();
        let mut step = ActionStep {
            action: Action::WaitImage {
                search,
                template,
                threshold: 90,
                timeout_ms: 1000,
                output: Default::default(),
                scaled: false,
            },
            target: None,
            delay_ms: 0,
        };
        assert!(step.validate().is_ok());
        assert_eq!(
            serde_json::from_str::<ActionStep>(&serde_json::to_string(&step).unwrap()).unwrap(),
            step
        );
        if let Action::WaitImage { threshold, .. } = &mut step.action {
            *threshold = 69;
        }
        assert!(step.validate().is_err());
        if let Action::WaitImage {
            threshold,
            timeout_ms,
            ..
        } = &mut step.action
        {
            *threshold = 90;
            *timeout_ms = 99;
        }
        assert!(step.validate().is_err());
        if let Action::WaitImage {
            timeout_ms,
            template,
            ..
        } = &mut step.action
        {
            *timeout_ms = 100;
            template.pixels = "zz".repeat(64);
        }
        assert!(step.validate().is_err());
        assert!(parse_image_rect("1, 2, 3, 4, 5").is_none());
        assert!(
            !ImageRect {
                x: i32::MAX,
                y: 0,
                width: 2,
                height: 8
            }
            .valid(512)
        );
    }

    #[test]
    fn pixel_condition_parses_and_validates_exact_rgb() {
        assert_eq!(
            parse_pixel(" -25, 10, #aB12ef "),
            Some((Point { x: -25, y: 10 }, 0xab12ef))
        );
        for value in [
            "1,2,abc123",
            "1,2,#fff",
            "1,2,#zz0000",
            "1,2,#123456,4",
            "x,2,#123456",
        ] {
            assert!(parse_pixel(value).is_none());
        }
        let mut step = ActionStep {
            action: Action::WaitColor {
                point: Point { x: 1, y: 2 },
                rgb: 0x123456,
                tolerance: 0,
                different: false,
                stable_ms: 0,
                skip_next_on_timeout: false,
                check_once: false,
                skip_next_on_mismatch: false,
                retry_clicks: 0,
            },
            target: None,
            delay_ms: 0,
        };
        assert!(step.validate().is_ok());
        let json = serde_json::to_string(&step).unwrap();
        assert_eq!(serde_json::from_str::<ActionStep>(&json).unwrap(), step);
        let legacy = r##"{"action":{"kind":"wait_color","point":{"x":1,"y":2},"rgb":1193046},"delay_ms":0}"##;
        assert_eq!(serde_json::from_str::<ActionStep>(legacy).unwrap(), step);
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x1000000,
            tolerance: 0,
            different: false,
            stable_ms: 0,
            skip_next_on_timeout: false,
            check_once: false,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 255,
            different: true,
            stable_ms: 0,
            skip_next_on_timeout: false,
            check_once: false,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 60_001,
            skip_next_on_timeout: false,
            check_once: false,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 1,
            skip_next_on_timeout: false,
            check_once: true,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 0,
            skip_next_on_timeout: true,
            check_once: true,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 0,
            skip_next_on_timeout: false,
            check_once: false,
            skip_next_on_mismatch: true,
            retry_clicks: 0,
        };
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 0,
            skip_next_on_timeout: false,
            check_once: true,
            skip_next_on_mismatch: true,
            retry_clicks: 0,
        };
        assert!(step.validate().is_ok());
        if let Action::WaitColor { retry_clicks, .. } = &mut step.action {
            *retry_clicks = 10;
        }
        assert!(step.validate().is_ok());
        if let Action::WaitColor { retry_clicks, .. } = &mut step.action {
            *retry_clicks = 11;
        }
        assert!(step.validate().is_err());
        step.action = Action::WaitColor {
            point: Point { x: 1, y: 2 },
            rgb: 0x123456,
            tolerance: 0,
            different: false,
            stable_ms: 0,
            skip_next_on_timeout: false,
            check_once: false,
            skip_next_on_mismatch: false,
            retry_clicks: 0,
        };
        step.target = Some(Destination::Point(Point { x: 1, y: 2 }));
        assert!(step.validate().is_err());
    }

    #[test]
    fn text_step_validates_and_preserves_unicode_and_spaces() {
        let mut step = ActionStep {
            action: Action::Text {
                text: " hello 🙂 ".into(),
            },
            target: None,
            delay_ms: 0,
        };
        assert!(step.validate().is_ok());
        let json = serde_json::to_string(&step).unwrap();
        assert_eq!(serde_json::from_str::<ActionStep>(&json).unwrap(), step);
        step.action = Action::Text {
            text: "line\nbreak".into(),
        };
        assert!(step.validate().is_err());
        step.action = Action::Text {
            text: "x".repeat(257),
        };
        assert!(step.validate().is_err());
        step.action = Action::Text { text: "ok".into() };
        step.target = Some(Destination::Point(Point { x: 0, y: 0 }));
        assert!(step.validate().is_err());
    }

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
