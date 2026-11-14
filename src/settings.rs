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
pub const MAX_POINTS: usize = 64;

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
    pub delay_seconds: u8,
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
    pub route_radius: String,
    pub route: Vec<Step>,
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
            route_radius: "0".into(),
            route: Vec::new(),
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

#[derive(Default, Deserialize, Serialize)]
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
        let mut limit = if self.limited {
            Some(
                self.count
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|v| (1..=100_000_000).contains(v))
                    .ok_or(Issue::new(
                        "repeat",
                        "Set an action limit from 1 to 100,000,000.",
                    ))?,
            )
        } else {
            None
        };
        if self.route_enabled {
            if self.route.is_empty() || self.route.len() > MAX_POINTS {
                return Err(Issue::new(
                    "route",
                    "Add between 1 and 64 points before running a route.",
                ));
            }
            if self.route_once && !self.has_action_route() {
                let points = self.route.len() as u64;
                limit = Some(limit.map_or(points, |count| count.min(points)));
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
                "Use continuous repeat and a single target for Hold down.",
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
                "Select Single click for Hold down.",
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
        if ![0, 1, 3].contains(&self.delay_seconds) {
            self.delay_seconds = 3;
        }
        self.unit = self.unit.min(8);
        self.edge_mode = self.edge_mode.min(2);
        self.process_mode = self.process_mode.min(2);
        self.route.truncate(MAX_POINTS);
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
