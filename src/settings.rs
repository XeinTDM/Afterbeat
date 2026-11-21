use crate::{
    engine::Schedule,
    mouse::{Button, Point, Zone},
    targets::Step,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug)]
pub struct Issue {
    pub field: &'static str,
    pub message: &'static str,
}

impl Issue {
    pub fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }
}

const MAX_INTERVAL: f64 = 86_400.0;
const MIN_INTERVAL: f64 = 0.0001;
pub const MAX_POINTS: usize = 256;
pub const MAX_RECORDING_EVENTS: usize = 10_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Settings {
    pub interval: String,
    pub count: String,
    pub button: Button,
    pub double: bool,
    pub triple: bool,
    pub scroll: bool,
    pub scroll_direction: crate::targets::Direction,
    pub scroll_ticks: String,
    pub limited: bool,
    pub fixed: bool,
    pub point: Option<Point>,
    pub point_wayland: bool,
    pub delay_seconds: u32,
    pub unit: u8,
    pub keyboard: bool,
    pub key: String,
    pub text_output: bool,
    pub text_character: String,
    pub hold_activation: bool,
    pub trigger: String,
    pub armed: bool,
    pub time_limit: String,
    pub press: String,
    pub hold_down: bool,
    pub random_rate: bool,
    pub low_cpu: bool,
    pub min_rate: String,
    pub max_rate: String,
    pub route_enabled: bool,
    pub route_once: bool,
    pub route_iterations: String,
    pub route_radius: String,
    pub route: Vec<Step>,
    pub recording_origin: crate::recording::Origin,
    pub target_timeout: String,
    pub capture_visual: bool,
    pub route_wayland: bool,
    pub edge_mode: u8,
    pub inside_zone: Option<Zone>,
    pub stop_zone: Option<Zone>,
    pub process_mode: u8,
    pub process_names: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval: "100".into(),
            count: "100".into(),
            button: Button::Left,
            double: false,
            triple: false,
            scroll: false,
            scroll_direction: crate::targets::Direction::Down,
            scroll_ticks: "1".into(),
            limited: false,
            fixed: false,
            point: None,
            point_wayland: false,
            delay_seconds: 3,
            unit: 0,
            keyboard: false,
            key: "Space".into(),
            text_output: false,
            text_character: "a".into(),
            hold_activation: false,
            trigger: "F6".into(),
            armed: true,
            time_limit: "0".into(),
            press: "0".into(),
            hold_down: false,
            random_rate: false,
            low_cpu: false,
            min_rate: "8".into(),
            max_rate: "12".into(),
            route_enabled: false,
            route_once: false,
            route_iterations: "0".into(),
            route_radius: "0".into(),
            route: Vec::new(),
            recording_origin: crate::recording::Origin::Windows,
            target_timeout: "10".into(),
            capture_visual: false,
            route_wayland: false,
            edge_mode: 0,
            inside_zone: None,
            stop_zone: None,
            process_mode: 0,
            process_names: String::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Library {
    pub profiles: BTreeMap<String, Settings>,
    pub sessions: u64,
    pub actions: u64,
    pub active_millis: u64,
}

impl Library {
    pub fn set_profile(&mut self, name: &str, settings: Settings) -> io::Result<()> {
        // Reserve only the extra digits that the lifetime counters can still grow by.
        let stats_headroom: u64 = [self.sessions, self.actions, self.active_millis]
            .into_iter()
            .map(|value| u64::from(u64::MAX.ilog10() - value.checked_ilog10().unwrap_or(0)))
            .sum();
        let previous = self.profiles.insert(name.into(), settings);
        if let Err(error) = encode_store(self, MAX_STORE - stats_headroom) {
            match previous {
                Some(settings) => self.profiles.insert(name.into(), settings),
                None => self.profiles.remove(name),
            };
            return Err(error);
        }
        Ok(())
    }
}

fn number(text: &str) -> Option<f64> {
    text.trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

impl Settings {
    pub fn change_unit(&mut self, unit: u8) {
        let old_interval = self.interval().ok();
        self.unit = unit.min(8);
        if let Some(interval) = old_interval {
            if interval.is_zero() && self.unit >= 5 {
                self.interval = "Max".into();
                return;
            }
            let seconds = interval.as_secs_f64();
            let unit_seconds = unit_seconds(self.unit).unwrap_or(1.0);
            let value = if self.unit < 5 {
                seconds / unit_seconds
            } else {
                unit_seconds / seconds
            };
            self.interval = value.to_string();
        }
    }

    pub fn interval(&self) -> Result<Duration, &'static str> {
        let unit = unit_seconds(self.unit).ok_or("Choose a valid timing unit.")?;
        if self.interval.trim().eq_ignore_ascii_case("max") {
            return Ok(Duration::ZERO);
        }
        let value = number(&self.interval).ok_or("Enter a valid interval or rate.")?;
        if value == 0.0 && self.unit < 5 {
            return Ok(Duration::ZERO);
        }
        let seconds = if self.unit < 5 {
            value * unit
        } else {
            unit / value
        };
        if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&seconds) {
            return Err("Set an interval between 0.1 ms and 1 day, or enter Max for max speed.");
        }
        Ok(Duration::from_secs_f64(seconds))
    }

    pub fn rates(&self) -> Result<Option<(f64, f64)>, &'static str> {
        if !self.random_rate {
            return Ok(None);
        }
        let low = number(&self.min_rate).unwrap_or_default();
        let high = number(&self.max_rate).unwrap_or_default();
        if !(MAX_INTERVAL.recip()..=MIN_INTERVAL.recip()).contains(&low)
            || !(low..=MIN_INTERVAL.recip()).contains(&high)
        {
            return Err("Set a valid minimum and maximum randomized rate in actions per second.");
        }
        Ok(Some((low, high)))
    }

    pub fn press_duration(&self) -> Result<Duration, &'static str> {
        let millis = number(&self.press)
            .filter(|v| (0.0..=60_000.0).contains(v))
            .ok_or("Set press duration between 0 and 60,000 ms.")?;
        Ok(Duration::from_secs_f64(millis / 1_000.0))
    }

    pub fn radius(&self) -> Result<i32, &'static str> {
        self.route_radius
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|v| (0..=200).contains(v))
            .ok_or("Set point radius between 0 and 200 px.")
    }

    pub fn action_limit(&self) -> Result<Option<u64>, Issue> {
        if !self.limited {
            return Ok(None);
        }
        self.count
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|v| (1..=100_000_000).contains(v))
            .map(Some)
            .ok_or(Issue::new(
                "repeat",
                "Set an action limit from 1 to 100,000,000.",
            ))
    }

    pub fn route_loop_limit(&self) -> Result<Option<u64>, Issue> {
        let value = self
            .route_iterations
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value <= 100_000_000)
            .ok_or(Issue::new(
                "route",
                "Set route loops from 0 to 100,000,000.",
            ))?;
        Ok((value > 0).then_some(value))
    }

    pub fn schedule(&self) -> Result<Schedule, Issue> {
        let inherits_cadence = self.inherits_route_cadence();
        let rates = if !inherits_cadence || (self.hold_down && !self.has_action_route()) {
            None
        } else {
            self.rates()
                .map_err(|message| Issue::new("rate", message))?
        };
        let interval =
            if !inherits_cadence || rates.is_some() || (self.hold_down && !self.has_action_route())
            {
                Duration::ZERO
            } else {
                self.interval()
                    .map_err(|message| Issue::new("cadence", message))?
            };
        let time = number(&self.time_limit)
            .filter(|v| (0.0..=MAX_INTERVAL).contains(v))
            .ok_or(Issue::new(
                "time",
                "Set a time limit from 0 to 86,400 seconds.",
            ))?;
        let mut limit = self.action_limit()?;
        if self.route_enabled {
            if self.route.is_empty() || self.route.len() > self.route_limit() {
                return Err(Issue::new(
                    "route",
                    "The route is empty or exceeds its step limit.",
                ));
            }
            if self.route_once && !self.has_action_route() {
                let points = self.route.len() as u64;
                limit = Some(limit.map_or(points, |count| count.min(points)));
            }
            if !self.route_once && !self.has_recording() {
                let iterations = self.route_loop_limit()?;
                if let Some(iterations) = iterations.filter(|_| !self.has_action_route()) {
                    let actions = iterations * self.route.len() as u64;
                    limit = Some(limit.map_or(actions, |count| count.min(actions)));
                }
            }
            if self.inside_zone.is_some() {
                return Err(Issue::new(
                    "inside",
                    "Clear the inside-only zone before running a route.",
                ));
            }
        }
        if self.fixed && !self.keyboard && self.inside_zone.is_some() {
            return Err(Issue::new(
                "inside",
                "Use Follow cursor or clear the inside-only zone.",
            ));
        }
        if self.hold_down
            && !self.has_action_route()
            && (self.limited || self.route_enabled || self.inside_zone.is_some())
        {
            return Err(Issue::new(
                "press",
                "Use continuous repeat and a single target for Hold output.",
            ));
        }
        if !self.has_action_route()
            && !self.keyboard
            && !self.scroll
            && self.click_count() > 1
            && self.hold_down
        {
            return Err(Issue::new(
                "click-type",
                "Select Single click for Hold output.",
            ));
        }
        Ok(Schedule {
            interval,
            limit,
            delay: Duration::from_secs(self.delay_seconds.into()),
            duration: (time > 0.0).then(|| Duration::from_secs_f64(time)),
            rates,
            low_cpu: self.low_cpu,
        })
    }

    pub fn normalize(&mut self) {
        if ![0, 1, 3, 10, 30, 60, 300, 900, 3600].contains(&self.delay_seconds) {
            self.delay_seconds = 3;
        }
        self.unit = self.unit.min(8);
        self.edge_mode = self.edge_mode.min(2);
        self.process_mode = self.process_mode.min(2);
        let limit = self.route_limit();
        self.route.truncate(limit);
    }

    pub fn has_recording(&self) -> bool {
        self.route.iter().any(|step| matches!(step, Step::Action(action) if matches!(action.action, crate::targets::Action::Recorded { .. })))
    }

    pub fn route_limit(&self) -> usize {
        if self.has_recording() {
            MAX_RECORDING_EVENTS
        } else {
            MAX_POINTS
        }
    }

    pub fn has_smart_route(&self) -> bool {
        self.route_enabled && self.route.iter().any(Step::is_smart)
    }

    pub fn click_count(&self) -> u8 {
        if self.triple {
            3
        } else if self.double {
            2
        } else {
            1
        }
    }

    pub fn scroll_ticks(&self) -> Result<u8, &'static str> {
        self.scroll_ticks
            .trim()
            .parse()
            .ok()
            .filter(|count| (1..=32).contains(count))
            .ok_or("Set the scroll amount from 1 to 32 ticks.")
    }

    pub fn has_action_route(&self) -> bool {
        self.route_enabled
            && self
                .route
                .iter()
                .any(|step| matches!(step, Step::Action(_)))
    }

    pub fn inherits_route_cadence(&self) -> bool {
        !self.has_action_route()
            || self
                .route
                .iter()
                .any(|step| !matches!(step, Step::Action(_)))
    }

    pub fn target_timeout(&self) -> Result<Duration, &'static str> {
        number(&self.target_timeout)
            .filter(|seconds| (0.1..=120.0).contains(seconds))
            .map(Duration::from_secs_f64)
            .ok_or("Set a target timeout from 0.1 to 120 seconds.")
    }
}

fn unit_seconds(unit: u8) -> Option<f64> {
    Some(match unit {
        0 => 0.001,
        1 | 5 => 1.0,
        2 | 6 => 60.0,
        3 | 7 => 3_600.0,
        4 | 8 => MAX_INTERVAL,
        _ => return None,
    })
}

#[cfg(windows)]
pub fn path() -> Option<PathBuf> {
    let local = PathBuf::from(env::var_os("LOCALAPPDATA")?);
    let current = local.join("Afterbeat");
    let legacy = local.join("PulseClick");
    let folder = if !current.is_dir() && legacy.is_dir() {
        legacy
    } else {
        current
    };
    Some(folder.join("settings.json"))
}

#[cfg(target_os = "linux")]
pub fn path() -> Option<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            let home = PathBuf::from(env::var_os("HOME")?);
            home.is_absolute().then(|| home.join(".config"))
        })?;
    Some(config.join("afterbeat/settings.json"))
}

#[cfg(target_os = "macos")]
pub fn path() -> Option<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME")?);
    home.is_absolute()
        .then(|| home.join("Library/Application Support/Afterbeat/settings.json"))
}

const MAX_STORE: u64 = 2 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PresetTransfer {
    format: String,
    version: u32,
    name: String,
    settings: Settings,
}

pub(crate) fn pixel_transfer_version(settings: &Settings) -> u32 {
    use crate::targets::{Action, ImageOutput};
    if settings.route.iter().any(|step| match step {
        Step::Action(step) => match &step.action {
            Action::Flow { .. } => true,
            Action::WaitImage {
                output,
                scaled,
                template,
                ..
            } => *output != ImageOutput::Wait || *scaled || template.is_color(),
            _ => false,
        },
        _ => false,
    }) {
        return 8;
    }
    if settings.route.iter().any(|step| matches!(step, Step::Action(action) if matches!(action.action, crate::targets::Action::WaitImage { .. }))) { return 7; }
    if settings.route.iter().any(|step| {
        matches!(step,
        Step::Action(action) if matches!(action.action,
            crate::targets::Action::WaitColor { retry_clicks: 1.., .. }
        ))
    }) {
        return 6;
    }
    if settings.route.iter().any(|step| {
        matches!(step,
        Step::Action(action) if matches!(action.action,
            crate::targets::Action::WaitColor { check_once: true, .. }
                | crate::targets::Action::WaitColor { skip_next_on_mismatch: true, .. }
        ))
    }) {
        return 5;
    }
    if settings.route.iter().any(|step| {
        matches!(step,
        Step::Action(action) if matches!(action.action,
            crate::targets::Action::WaitColor { skip_next_on_timeout: true, .. }
        ))
    }) {
        return 4;
    }
    if settings.route.iter().any(|step| {
        matches!(step,
        Step::Action(action) if matches!(action.action,
            crate::targets::Action::WaitColor { stable_ms: 1.., .. }
        ))
    }) {
        return 3;
    }
    if settings.route.iter().any(|step| {
        matches!(step,
        Step::Action(action) if matches!(action.action,
            crate::targets::Action::WaitColor { tolerance: 1..=255, .. }
                | crate::targets::Action::WaitColor { different: true, .. }
        ))
    }) {
        2
    } else {
        1
    }
}

pub fn export_preset(path: &Path, name: &str, settings: &Settings) -> io::Result<()> {
    let transfer = PresetTransfer {
        format: "afterbeat-preset".into(),
        version: pixel_transfer_version(settings),
        name: name.into(),
        settings: settings.clone(),
    };
    let bytes = encode_store(&transfer, MAX_STORE)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

pub fn import_preset(path: &Path) -> io::Result<(String, Settings)> {
    let transfer: PresetTransfer = serde_json::from_slice(&read_bounded(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if transfer.format != "afterbeat-preset"
        || !(1..=8).contains(&transfer.version)
        || transfer.version < pixel_transfer_version(&transfer.settings)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported preset format or version.",
        ));
    }
    if transfer.settings.route.len() > transfer.settings.route_limit() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "The preset exceeds the route step limit.",
        ));
    }
    Ok((transfer.name, transfer.settings))
}

pub struct Loaded<T> {
    pub value: T,
    pub recovered: bool,
}

pub fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> io::Result<Loaded<T>> {
    let decode = |path: &Path| -> io::Result<T> {
        serde_json::from_slice(&read_bounded(path)?)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    };
    match decode(path) {
        Ok(value) => Ok(Loaded {
            value,
            recovered: false,
        }),
        Err(primary) => match decode(&path.with_extension("bak")) {
            Ok(value) => Ok(Loaded {
                value,
                recovered: true,
            }),
            Err(backup)
                if primary.kind() == io::ErrorKind::NotFound
                    && backup.kind() == io::ErrorKind::NotFound =>
            {
                Ok(Loaded {
                    value: T::default(),
                    recovered: false,
                })
            }
            Err(backup) if primary.kind() == io::ErrorKind::NotFound => Err(backup),
            Err(_) => Err(primary),
        },
    }
}

fn encode_store(value: &impl Serialize, limit: u64) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Local data exceeds the 2 MiB limit.",
        ));
    }
    Ok(bytes)
}

pub fn save<T: Serialize + serde::de::DeserializeOwned>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = encode_store(value, MAX_STORE)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    match read_bounded(path) {
        Ok(previous) if serde_json::from_slice::<T>(&previous).is_ok() => {
            if previous == bytes {
                return Ok(());
            }
            write_atomic(&path.with_extension("bak"), &previous)?;
        }
        Ok(corrupt) => {
            let (quarantine, mut file) = reserve_quarantine(path)?;
            let written = file.write_all(&corrupt).and_then(|_| file.sync_all());
            drop(file);
            if written.is_err() {
                let _ = fs::remove_file(quarantine);
            }
            written?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) if err.kind() == io::ErrorKind::FileTooLarge => {
            // Move oversized data aside without allocating or copying it.
            let (quarantine, reservation) = reserve_quarantine(path)?;
            drop(reservation);
            if let Err(error) = fs::rename(path, &quarantine) {
                let _ = fs::remove_file(&quarantine);
                return Err(error);
            }
            let result = write_atomic(path, &bytes);
            if result.is_err() && !path.exists() {
                let _ = fs::rename(&quarantine, path);
            }
            return result;
        }
        Err(err) => return Err(err),
    }
    write_atomic(path, &bytes)
}

fn reserve_quarantine(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let mut suffix = 0u64;
    loop {
        let quarantine = if suffix == 0 {
            path.with_extension("corrupt")
        } else {
            path.with_extension(format!("corrupt.{suffix}"))
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&quarantine)
        {
            Ok(file) => return Ok((quarantine, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => suffix += 1,
            Err(error) => return Err(error),
        }
    }
}

fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    if file.metadata()?.len() <= MAX_STORE {
        file.take(MAX_STORE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 <= MAX_STORE {
            return Ok(bytes);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::FileTooLarge,
        "Local data exceeds the 2 MiB limit.",
    ))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_file_name(format!(".afterbeat-{}-{id}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_transfer_round_trips_and_never_overwrites() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "afterbeat-transfer-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preset.json");
        let settings = Settings {
            interval: "250".into(),
            ..Settings::default()
        };
        export_preset(&path, "Example", &settings).unwrap();
        assert_eq!(
            export_preset(&path, "Other", &settings).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            import_preset(&path).unwrap(),
            ("Example".to_owned(), settings)
        );
        let advanced_path = dir.join("advanced.json");
        let advanced = Settings {
            route: vec![Step::Action(Box::new(crate::targets::ActionStep {
                action: crate::targets::Action::WaitColor {
                    point: Point { x: 1, y: 2 },
                    rgb: 0x123456,
                    tolerance: 2,
                    different: true,
                    stable_ms: 0,
                    skip_next_on_timeout: false,
                    check_once: false,
                    skip_next_on_mismatch: false,
                    retry_clicks: 0,
                },
                target: None,
                delay_ms: 0,
            }))],
            ..Settings::default()
        };
        export_preset(&advanced_path, "Advanced", &advanced).unwrap();
        let mut encoded: serde_json::Value =
            serde_json::from_slice(&fs::read(&advanced_path).unwrap()).unwrap();
        assert_eq!(encoded["version"], 2);
        assert_eq!(
            import_preset(&advanced_path).unwrap(),
            ("Advanced".to_owned(), advanced.clone())
        );
        encoded["version"] = 1.into();
        fs::write(&advanced_path, serde_json::to_vec(&encoded).unwrap()).unwrap();
        assert_eq!(
            import_preset(&advanced_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut stable = advanced.clone();
        if let Step::Action(action) = &mut stable.route[0]
            && let crate::targets::Action::WaitColor { stable_ms, .. } = &mut action.action
        {
            *stable_ms = 250;
        }
        let stable_path = dir.join("stable.json");
        export_preset(&stable_path, "Stable", &stable).unwrap();
        let mut stable_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&stable_path).unwrap()).unwrap();
        assert_eq!(stable_json["version"], 3);
        assert_eq!(
            import_preset(&stable_path).unwrap(),
            ("Stable".to_owned(), stable.clone())
        );
        stable_json["version"] = 2.into();
        fs::write(&stable_path, serde_json::to_vec(&stable_json).unwrap()).unwrap();
        assert_eq!(
            import_preset(&stable_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut branched = stable.clone();
        if let Step::Action(action) = &mut branched.route[0]
            && let crate::targets::Action::WaitColor {
                skip_next_on_timeout,
                ..
            } = &mut action.action
        {
            *skip_next_on_timeout = true;
        }
        let branch_path = dir.join("branch.json");
        export_preset(&branch_path, "Branch", &branched).unwrap();
        let mut branch_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&branch_path).unwrap()).unwrap();
        assert_eq!(branch_json["version"], 4);
        assert_eq!(
            import_preset(&branch_path).unwrap(),
            ("Branch".to_owned(), branched.clone())
        );
        branch_json["version"] = 3.into();
        fs::write(&branch_path, serde_json::to_vec(&branch_json).unwrap()).unwrap();
        assert_eq!(
            import_preset(&branch_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut once = branched;
        if let Step::Action(action) = &mut once.route[0]
            && let crate::targets::Action::WaitColor {
                check_once,
                stable_ms,
                skip_next_on_timeout,
                skip_next_on_mismatch,
                ..
            } = &mut action.action
        {
            *check_once = true;
            *stable_ms = 0;
            *skip_next_on_timeout = false;
            *skip_next_on_mismatch = true;
        }
        let once_path = dir.join("once.json");
        export_preset(&once_path, "Once", &once).unwrap();
        let mut once_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&once_path).unwrap()).unwrap();
        assert_eq!(once_json["version"], 5);
        assert_eq!(
            import_preset(&once_path).unwrap(),
            ("Once".to_owned(), once.clone())
        );
        once_json["version"] = 4.into();
        fs::write(&once_path, serde_json::to_vec(&once_json).unwrap()).unwrap();
        assert_eq!(
            import_preset(&once_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut retried = once;
        if let Step::Action(action) = &mut retried.route[0]
            && let crate::targets::Action::WaitColor { retry_clicks, .. } = &mut action.action
        {
            *retry_clicks = 2;
        }
        let retry_path = dir.join("retry.json");
        export_preset(&retry_path, "Retry", &retried).unwrap();
        let mut retry_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&retry_path).unwrap()).unwrap();
        assert_eq!(retry_json["version"], 6);
        assert_eq!(
            import_preset(&retry_path).unwrap(),
            ("Retry".to_owned(), retried)
        );
        retry_json["version"] = 5.into();
        fs::write(&retry_path, serde_json::to_vec(&retry_json).unwrap()).unwrap();
        assert_eq!(
            import_preset(&retry_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let image = Settings {
            route: vec![Step::Action(Box::new(crate::targets::ActionStep {
                action: crate::targets::Action::WaitImage {
                    search: crate::targets::ImageRect {
                        x: 0,
                        y: 0,
                        width: 16,
                        height: 16,
                    },
                    template: crate::targets::ImageTemplate {
                        width: 8,
                        height: 8,
                        pixels: "00".repeat(64),
                    },
                    threshold: 95,
                    timeout_ms: 1000,
                    output: Default::default(),
                    scaled: false,
                },
                target: None,
                delay_ms: 0,
            }))],
            ..Settings::default()
        };
        let image_path = dir.join("image.json");
        export_preset(&image_path, "Image", &image).unwrap();
        let mut image_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&image_path).unwrap()).unwrap();
        assert_eq!(image_json["version"], 7);
        assert_eq!(
            import_preset(&image_path).unwrap(),
            ("Image".to_owned(), image)
        );
        image_json["version"] = 6.into();
        fs::write(&image_path, serde_json::to_vec(&image_json).unwrap()).unwrap();
        assert_eq!(
            import_preset(&image_path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::write(
            &path,
            br#"{"format":"afterbeat-preset","version":9,"name":"Example","settings":{}}"#,
        )
        .unwrap();
        assert_eq!(
            import_preset(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_presets_leave_the_library_unchanged_and_room_for_stats() {
        let mut library = Library::default();
        let original = Settings::default();
        library.set_profile("Original", original.clone()).unwrap();
        let extra = MAX_STORE as usize - encode_store(&library, MAX_STORE).unwrap().len();
        let oversized = Settings {
            process_names: "x".repeat(extra),
            ..original.clone()
        };
        assert!(library.set_profile("Original", oversized.clone()).is_err());
        assert!(library.set_profile("New", oversized).is_err());
        assert_eq!(library.profiles.len(), 1);
        assert_eq!(library.profiles["Original"], original);

        let large = Settings {
            process_names: "x".repeat(extra - 3 * u64::MAX.ilog10() as usize),
            ..original
        };
        library.set_profile("Original", large).unwrap();
        library.sessions = u64::MAX;
        library.actions = u64::MAX;
        library.active_millis = u64::MAX;
        assert_eq!(
            encode_store(&library, MAX_STORE).unwrap().len() as u64,
            MAX_STORE
        );
    }

    #[test]
    fn converting_units_preserves_the_cadence() {
        let mut settings = Settings::default();
        for unit in [5, 6, 1, 2, 3, 4, 8, 0] {
            settings.change_unit(unit);
            assert_eq!(settings.interval(), Ok(Duration::from_millis(100)));
        }
        settings.interval = "0".into();
        for unit in [5, 6, 7, 8, 4, 3, 2, 1, 0] {
            settings.change_unit(unit);
            assert_eq!(settings.interval(), Ok(Duration::ZERO));
        }
        settings.unit = 5;
        assert!(settings.interval().is_err());
    }

    #[test]
    fn restores_the_last_good_copy_and_preserves_each_damaged_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("afterbeat-{}-{suffix}", std::process::id()));
        let path = dir.join("settings.json");
        let first = Settings::default();
        let second = Settings {
            interval: "50".into(),
            ..first.clone()
        };
        save(&path, &first).unwrap();
        save(&path, &second).unwrap();
        for (extension, contents) in [
            ("corrupt", "{ truncated"),
            ("corrupt.1", "{ truncated again"),
        ] {
            fs::write(&path, contents).unwrap();
            let restored = load::<Settings>(&path).unwrap();
            assert!(restored.recovered);
            assert_eq!(restored.value, first);
            save(&path, &restored.value).unwrap();
            assert_eq!(
                fs::read_to_string(path.with_extension(extension)).unwrap(),
                contents
            );
        }
        let current = load::<Settings>(&path).unwrap();
        assert!(!current.recovered);
        assert_eq!(current.value, first);
        assert_eq!(
            fs::read(path.with_extension("corrupt")).unwrap(),
            b"{ truncated"
        );
        assert_eq!(
            load::<Settings>(&path.with_extension("bak")).unwrap().value,
            first
        );
        for file in [
            path.clone(),
            path.with_extension("bak"),
            path.with_extension("corrupt"),
            path.with_extension("corrupt.1"),
        ] {
            fs::remove_file(file).unwrap();
        }
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn recovers_an_oversized_primary_without_replacing_a_quarantined_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "afterbeat-recovery-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(path.with_extension("bak"), br#"{"interval":"83"}"#).unwrap();
        fs::write(path.with_extension("corrupt"), b"previous failure").unwrap();
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_STORE + 1)
            .unwrap();
        let recovered = load::<Settings>(&path).unwrap();
        assert!(recovered.recovered);
        save(&path, &recovered.value).unwrap();
        assert_eq!(load::<Settings>(&path).unwrap().value.interval, "83");
        assert_eq!(
            fs::read(path.with_extension("corrupt")).unwrap(),
            b"previous failure"
        );
        assert_eq!(
            fs::metadata(path.with_extension("corrupt.1"))
                .unwrap()
                .len(),
            MAX_STORE + 1
        );
        for file in [
            path.clone(),
            path.with_extension("bak"),
            path.with_extension("corrupt"),
            path.with_extension("corrupt.1"),
        ] {
            fs::remove_file(file).unwrap();
        }
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn old_settings_keep_legacy_interval_and_defaults() {
        let settings: Settings =
            serde_json::from_str(r#"{"interval":"0,5","limited":true,"count":"7"}"#).unwrap();
        assert!(!settings.low_cpu);
        assert_eq!(settings.interval(), Ok(Duration::from_micros(500)));
        assert_eq!(settings.schedule().unwrap().limit, Some(7));
        assert!(!settings.keyboard);
        assert!(settings.armed);
        assert!(settings.route.is_empty());
        assert_eq!(settings.recording_origin, crate::recording::Origin::Windows);
    }

    #[test]
    fn converts_rate_and_delay_units() {
        let mut settings = Settings {
            unit: 6,
            interval: "30".into(),
            ..Settings::default()
        };
        assert_eq!(settings.interval(), Ok(Duration::from_secs(2)));
        settings.unit = 4;
        settings.interval = "1".into();
        assert_eq!(settings.interval(), Ok(Duration::from_secs(86_400)));
        for invalid in ["-5", "NaN", "inf", "2", ""] {
            settings.interval = invalid.into();
            assert!(settings.interval().is_err());
        }
    }

    #[test]
    fn ordinary_routes_accept_256_steps_and_reject_more() {
        let mut settings = Settings {
            route_enabled: true,
            route: vec![Step::Point(Point { x: 0, y: 0 }); 256],
            ..Settings::default()
        };
        assert!(settings.schedule().is_ok());
        settings.route.push(Step::Point(Point { x: 1, y: 1 }));
        assert!(settings.schedule().is_err());
        settings.normalize();
        assert_eq!(settings.route.len(), 256);
    }

    #[test]
    fn start_delay_choices_survive_save_and_schedule() {
        for seconds in [0, 1, 3, 10, 30, 60, 300, 900, 3600] {
            let original = Settings {
                delay_seconds: seconds,
                ..Settings::default()
            };
            let json = serde_json::to_vec(&original).unwrap();
            let mut restored: Settings = serde_json::from_slice(&json).unwrap();
            restored.normalize();
            assert_eq!(restored.delay_seconds, seconds);
            assert_eq!(
                restored.schedule().unwrap().delay,
                Duration::from_secs(seconds.into())
            );
        }
    }

    #[test]
    fn route_loop_count_honors_action_limit_and_once_mode() {
        let mut settings = Settings {
            route_enabled: true,
            route: vec![
                Step::Point(Point { x: 0, y: 0 }),
                Step::Point(Point { x: 1, y: 1 }),
            ],
            route_iterations: "3".into(),
            ..Settings::default()
        };
        assert_eq!(settings.schedule().unwrap().limit, Some(6));
        settings.limited = true;
        settings.count = "5".into();
        assert_eq!(settings.schedule().unwrap().limit, Some(5));
        settings.limited = false;
        settings.route_once = true;
        assert_eq!(settings.schedule().unwrap().limit, Some(2));
        settings.route_once = false;
        settings.route_iterations = "0".into();
        assert_eq!(settings.schedule().unwrap().limit, None);
        settings.route_iterations = "100000001".into();
        assert_eq!(settings.schedule().err().unwrap().field, "route");
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.route_iterations, "100000001");
        settings
            .route
            .push(Step::Action(Box::new(crate::targets::ActionStep {
                action: crate::targets::Action::Wait,
                target: None,
                delay_ms: 1,
            })));
        assert!(settings.schedule().is_err());
        settings.route_iterations = "3".into();
        assert_eq!(settings.schedule().unwrap().limit, None);
    }

    #[test]
    fn route_once_uses_the_first_of_count_and_route_limits() {
        let mut settings = Settings {
            route_enabled: true,
            route_once: true,
            route: vec![
                Step::Point(Point { x: 0, y: 0 }),
                Step::Point(Point { x: 1, y: 1 }),
            ],
            ..Settings::default()
        };
        assert_eq!(settings.schedule().unwrap().limit, Some(2));
        settings.limited = true;
        settings.count = "1".into();
        assert_eq!(settings.schedule().unwrap().limit, Some(1));
    }
}
