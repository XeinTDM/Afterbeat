use super::*;
use crate::{
    target_pixels::{Frame, Region, Signature},
    targets::VisualIdentity,
};

const ANCHOR_RADIUS: i32 = 360;
const MAX_WINDOWS: usize = 12;
const MAX_LANDMARKS: i32 = 96;
const MAX_CONTROLS: i32 = 48;
const ANCHOR_SCORE: f64 = 0.82;
const TARGET_SCORE: f64 = 0.86;
const NEAR_MATCH: f64 = 0.78;
const SEARCH_OFFSETS: [i32; 5] = [0, -2, 2, -4, 4];

pub(super) struct LiveEvidence {
    target: Region,
    landmark: Region,
    target_signature: Signature,
    anchor_signature: Signature,
}

impl LiveEvidence {
    pub(super) fn confirm(&self, hwnd: usize) -> bool {
        let window = hwnd as windows_sys::Win32::Foundation::HWND;
        if !self.target.visible(window) || !self.landmark.visible(window) {
            return false;
        }
        let Some(frame) = self.target.union(self.landmark).and_then(Frame::capture) else {
            return false;
        };
        // Confirm the scene that was just reviewed. Use a tighter tolerance than
        // the persisted fingerprint, which has to survive a new rendering scale.
        frame
            .signature(self.target)
            .is_some_and(|signature| signature.similarity(self.target_signature) >= 0.98)
            && frame
                .signature(self.landmark)
                .is_some_and(|signature| signature.similarity(self.anchor_signature) >= 0.98)
    }

    fn agrees_with(&self, other: &Self) -> bool {
        self.target == other.target
            && self.landmark == other.landmark
            && self.target_signature.similarity(other.target_signature) >= 0.98
            && self.anchor_signature.similarity(other.anchor_signature) >= 0.98
    }
}

impl Target {
    pub fn landmark_bounds(&self) -> Option<Zone> {
        let region = self.visual_check.as_ref()?.landmark;
        Some(region_bounds(region))
    }
}

enum Candidate {
    Found(Target),
    Absent,
    Unknown,
    Ambiguous,
}

impl Automation {
    pub(super) fn capture_visual(
        &self,
        selector: &Selector,
        point: Point,
    ) -> Result<VisualIdentity, String> {
        self.read_visual(selector, point).ok_or_else(|| {
            "Could not capture a distinctive target and a unique nearby label in this window."
                .into()
        })
    }

    fn read_visual(&self, selector: &Selector, point: Point) -> Option<VisualIdentity> {
        let native = NativeHit::at(point)?;
        let hwnd = HWND(native.window as _);
        if !window_matches(hwnd, &selector.window) {
            return None;
        }
        let dpi = i32::try_from(native.dpi).ok()?.clamp(96, 480);
        let root = unsafe { self.client.ElementFromHandle(hwnd) }.ok()?;
        let mut target_name = String::new();
        let target = if let Some(control) = &selector.control {
            let element = self
                .control_under_point(&root, point, control.role)
                .ok()??;
            if [UIA_EditControlTypeId, UIA_ComboBoxControlTypeId].contains(&role(&element).ok()?) {
                return None;
            }
            target_name = unsafe { element.CurrentName() }.ok()?.to_string();
            element_region(&element)?
        } else {
            Region::centered(point, scale(Point { x: 48, y: 28 }, dpi)?)
        };
        if !target.visible(hwnd.0) {
            return None;
        }
        let landmarks = self.landmarks(&root).ok()?;
        let count = unsafe { landmarks.Length() }.ok()?;
        if count > MAX_LANDMARKS {
            return None;
        }
        let mut nearby = Vec::new();
        for index in 0..count {
            let element = unsafe { landmarks.GetElement(index) }.ok()?;
            let Some(region) = element_region(&element) else {
                continue;
            };
            let offset = difference(target.center(), region.center());
            if distance(offset, dpi) > i64::from(ANCHOR_RADIUS).pow(2)
                || overlaps(target, region)
                || !region.visible(hwnd.0)
            {
                continue;
            }
            let name = unsafe { element.CurrentName() }.ok()?.to_string();
            if name.trim().is_empty()
                || name == target_name
                || unsafe { element.CurrentIsPassword() }.ok()?.as_bool()
            {
                continue;
            }
            let identity = ControlIdentity {
                role: role(&element).ok()?.0,
                identity: Identity::Name(name),
            };
            nearby.push((distance(offset, dpi), region, identity));
        }
        nearby.sort_by_key(|entry| entry.0);
        for (_, landmark, anchor) in nearby {
            if !matches!(
                unique(&self.matches(&root, &anchor).ok()?),
                ResolutionCount::One(_)
            ) {
                continue;
            }
            let frame = Frame::capture(target.union(landmark)?)?;
            let samples = target_samples(target, selector.control.is_some());
            let Some(target_patch) = frame.signature(samples).and_then(Signature::store) else {
                continue;
            };
            let Some(landmark_patch) = frame.signature(landmark).and_then(Signature::store) else {
                continue;
            };
            if NativeHit::at(point) != Some(native) {
                return None;
            }
            return Some(VisualIdentity {
                anchor,
                target_name,
                anchor_size: logical(landmark.size, dpi),
                target_size: logical(target.size, dpi),
                offset: logical(difference(target.center(), landmark.center()), dpi),
                target: target_patch,
                landmark: landmark_patch,
            });
        }
        None
    }

    pub(super) fn resolve_visual(
        &self,
        selector: &Selector,
        clicked: Option<Target>,
    ) -> Resolution {
        let Some(visual) = &selector.visual else {
            return Resolution::Unavailable;
        };
        let Some(target_signature) = Signature::read(&visual.target) else {
            return Resolution::VisualMissing;
        };
        let Some(anchor_signature) = Signature::read(&visual.landmark) else {
            return Resolution::VisualMissing;
        };
        if let Some(clicked) = clicked {
            let (root, hwnd) = match self.clicked_window(&clicked) {
                Ok(found) => found,
                Err(status) => return status,
            };
            return match self.visual_candidate(
                selector,
                &root,
                hwnd,
                target_signature,
                anchor_signature,
                true,
            ) {
                Candidate::Absent => Resolution::Missing,
                _ => Resolution::Unavailable,
            };
        }

        let windows = match self.app_windows(&selector.window, false) {
            Ok(windows) if windows.len() <= MAX_WINDOWS => windows,
            Ok(_) => return Resolution::Ambiguous,
            Err(status) => return status,
        };
        let mut proposal = None;
        let mut unknown = false;
        for hwnd in windows {
            if unsafe { wm::IsIconic(hwnd) }.as_bool() {
                unknown = true;
                continue;
            }
            let Ok(root) = (unsafe { self.client.ElementFromHandle(hwnd) }) else {
                unknown = true;
                continue;
            };
            match self.visual_candidate(
                selector,
                &root,
                hwnd,
                target_signature,
                anchor_signature,
                false,
            ) {
                Candidate::Found(target) => {
                    if proposal.replace(target).is_some() {
                        return Resolution::Ambiguous;
                    }
                }
                Candidate::Absent => {}
                Candidate::Unknown => unknown = true,
                Candidate::Ambiguous => return Resolution::Ambiguous,
            }
        }
        let Some(mut candidate) = proposal else {
            return if unknown {
                Resolution::Unavailable
            } else {
                Resolution::VisualMissing
            };
        };
        if unknown {
            return Resolution::Ambiguous;
        }
        let strict = selector
            .control
            .as_ref()
            .map(|_| self.resolve_checked(selector, None));
        // UIA may spend time enumerating other windows after this match. Only
        // renew the observation once the complete scene still agrees.
        if !candidate
            .visual_check
            .as_ref()
            .is_some_and(|check| check.confirm(candidate.native.window))
            || NativeHit::at(candidate.point) != Some(candidate.native)
        {
            return Resolution::Unavailable;
        }
        candidate.observed_at = Instant::now();
        // Compare the exact selector against the independently resolved scene.
        // If an old ID now points at a different control, only an explicit repair can replace it.
        match strict {
            None if window_matches(HWND(candidate.native.window as _), &selector.window) => {
                Resolution::Ready(candidate)
            }
            None => Resolution::Recovered(candidate),
            Some(Ok(Resolution::Ready(strict))) if equivalent(&strict, &candidate) => {
                Resolution::Ready(candidate)
            }
            Some(Ok(Resolution::Ready(_))) => Resolution::Conflict(candidate),
            Some(Ok(Resolution::Missing | Resolution::Ambiguous)) => {
                Resolution::Recovered(candidate)
            }
            _ => Resolution::Unavailable,
        }
    }

    fn visual_candidate(
        &self,
        selector: &Selector,
        root: &IUIAutomationElement,
        hwnd: HWND,
        target_signature: Signature,
        anchor_signature: Signature,
        verifying: bool,
    ) -> Candidate {
        self.read_candidate(
            selector,
            root,
            hwnd,
            target_signature,
            anchor_signature,
            verifying,
        )
        .unwrap_or(Candidate::Unknown)
    }

    fn read_candidate(
        &self,
        selector: &Selector,
        root: &IUIAutomationElement,
        hwnd: HWND,
        target_signature: Signature,
        anchor_signature: Signature,
        verifying: bool,
    ) -> Option<Candidate> {
        let visual = selector.visual.as_ref()?;
        let dpi = i32::try_from(unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd.0) })
            .ok()?
            .clamp(96, 480);
        let anchor = match unique(&self.matches(root, &visual.anchor).ok()?) {
            ResolutionCount::One(anchor) => anchor,
            ResolutionCount::None if verifying => return Some(Candidate::Unknown),
            ResolutionCount::None => return Some(Candidate::Absent),
            ResolutionCount::Many => return Some(Candidate::Ambiguous),
        };
        let landmark = element_region(&anchor)?;
        if !landmark.visible(hwnd.0) {
            return Some(Candidate::Unknown);
        }
        let render_scale = render_scale(landmark.size, visual.anchor_size, dpi)?;
        let offset = scale(visual.offset, render_scale)?;
        if distance(visual.offset, 96) > i64::from(ANCHOR_RADIUS).pow(2) {
            return None;
        }
        let center = sum(landmark.center(), offset);
        let target_size = scale(visual.target_size, render_scale)?;
        let expected = Region::centered(center, target_size);
        let mut regions = Vec::new();
        if let Some(control) = &selector.control {
            let name = &visual.target_name;
            let controls = if name.trim().is_empty() {
                self.matches(root, control).ok()?
            } else {
                self.matches(
                    root,
                    &ControlIdentity {
                        role: control.role,
                        identity: Identity::Name(name.clone()),
                    },
                )
                .ok()?
            };
            let count = unsafe { controls.Length() }.ok()?;
            if count > MAX_CONTROLS {
                return Some(Candidate::Ambiguous);
            }
            for index in 0..count {
                let element = unsafe { controls.GetElement(index) }.ok()?;
                if unsafe { element.CurrentIsOffscreen() }.ok()?.as_bool() {
                    continue;
                }
                let Some(region) = element_region(&element) else {
                    continue;
                };
                if compatible(region.size, target_size)
                    && distance(difference(region.center(), center), render_scale) <= 18 * 18
                {
                    regions.push((region, Some(element)));
                }
            }
        } else {
            for y in SEARCH_OFFSETS {
                for x in SEARCH_OFFSETS {
                    let delta = scale(Point { x, y }, render_scale)?;
                    regions.push((Region::centered(sum(center, delta), target_size), None));
                }
            }
        }
        let zone = regions
            .iter()
            .try_fold(landmark.union(expected)?, |zone, (region, _)| {
                zone.union(*region)
            })?;
        let frame = Frame::capture(zone)?;
        let landmark_live = frame.signature(landmark)?;
        if landmark_live.scale_similarity(anchor_signature) < ANCHOR_SCORE {
            return Some(Candidate::Unknown);
        }
        if regions.is_empty() {
            return Some(if expected.visible(hwnd.0) {
                Candidate::Absent
            } else {
                Candidate::Unknown
            });
        }

        let mut matches = Vec::new();
        let mut uncertain = false;
        for (region, element) in regions {
            if !region.visible(hwnd.0) {
                uncertain = true;
                continue;
            }
            let signature = frame.signature(target_samples(region, element.is_some()))?;
            let score = if element.is_some() {
                signature.scale_similarity(target_signature)
            } else {
                signature.similarity(target_signature)
            };
            if score >= NEAR_MATCH {
                matches.push((score, region, element, signature));
            }
        }
        if uncertain {
            return Some(Candidate::Unknown);
        }
        matches.sort_by(|left, right| right.0.total_cmp(&left.0));
        let Some((score, region, element, signature)) = matches.first() else {
            // A real enabled control with changed pixels is unresolved. An absent
            // graphic can be verified only while its original area is visible.
            return Some(if selector.control.is_none() && expected.visible(hwnd.0) {
                Candidate::Absent
            } else {
                Candidate::Unknown
            });
        };
        let region = *region;
        if *score < TARGET_SCORE {
            return Some(Candidate::Unknown);
        }
        // A near match can block a click, but cannot authorize one after the
        // target has moved out of its saved position relative to the anchor.
        if element.is_some() && distance(difference(region.center(), center), render_scale) > 8 * 8
        {
            return Some(Candidate::Unknown);
        }
        if matches.iter().skip(1).any(|(_, other, other_element, _)| {
            other_element.is_some()
                || distance(difference(other.center(), region.center()), render_scale) > 5 * 5
        }) {
            return Some(Candidate::Ambiguous);
        }
        if verifying {
            return Some(Candidate::Unknown);
        }
        let mut point = region.center();
        let mut control_window = 0;
        if let Some(element) = element {
            if !unsafe { element.CurrentIsEnabled() }.ok()?.as_bool() {
                return Some(Candidate::Unknown);
            }
            let mut clickable = POINT::default();
            if !unsafe { element.GetClickablePoint(&mut clickable) }
                .ok()?
                .as_bool()
            {
                return Some(Candidate::Unknown);
            }
            point = Point {
                x: clickable.x,
                y: clickable.y,
            };
            let hit = unsafe { self.client.ElementFromPoint(clickable) }.ok()?;
            if !self.hit_is_control(hit, element, root).ok()? {
                return Some(Candidate::Unknown);
            }
            control_window = unsafe { element.CurrentNativeWindowHandle() }.ok()?.0 as usize;
        } else if self.interactive_at(root, point).ok()?.is_some() {
            // A canvas fingerprint cannot authorize a click on a newly exposed
            // accessible control or a control that covers the saved graphic.
            return Some(Candidate::Unknown);
        }
        let native = NativeHit::at(point).filter(|hit| hit.window == hwnd.0 as usize)?;
        if !contains(region, point) || !on_monitor(point) {
            return Some(Candidate::Unknown);
        }
        Some(Candidate::Found(Target {
            point,
            bounds: region_bounds(region),
            native,
            control_window,
            observed_at: Instant::now(),
            visual_check: Some(Arc::new(LiveEvidence {
                target: target_samples(region, element.is_some()),
                landmark,
                target_signature: *signature,
                anchor_signature: landmark_live,
            })),
        }))
    }

    pub(super) fn repair(&self, step: ControlStep, shown: Target) -> Result<ControlStep, String> {
        let current = match self.resolve(&step.selector, None) {
            Resolution::Recovered(target) | Resolution::Conflict(target)
                if equivalent(&shown, &target)
                    && shown
                        .visual_check
                        .as_ref()
                        .zip(target.visual_check.as_ref())
                        .is_some_and(|(old, live)| old.agrees_with(live)) =>
            {
                target
            }
            _ => {
                return Err(
                    "The proposed target changed. Check the route again before repairing.".into(),
                );
            }
        };
        if !current.current() {
            return Err("The target changed while checking the repair. Check again.".into());
        }
        let mut repaired = self.capture(current.point, true)?;
        if repaired
            .selector
            .visual
            .as_ref()
            .map(|visual| &visual.anchor)
            != step.selector.visual.as_ref().map(|visual| &visual.anchor)
            || !matches!(self.resolve(&repaired.selector, None), Resolution::Ready(target) | Resolution::Recovered(target) if equivalent(&current, &target))
        {
            return Err(
                "The repaired selector could not confirm the same landmark and target.".into(),
            );
        }
        repaired.wait_for_disappearance = step.wait_for_disappearance;
        Ok(repaired)
    }

    pub(super) fn interactive_at(
        &self,
        root: &IUIAutomationElement,
        point: Point,
    ) -> windows::core::Result<Option<IUIAutomationElement>> {
        let hit = unsafe {
            self.client.ElementFromPoint(POINT {
                x: point.x,
                y: point.y,
            })
        }?;
        for element in self.ancestors(hit, root) {
            let element = element?;
            if interactive(role(&element)?) {
                return Ok(Some(element));
            }
        }
        Ok(None)
    }

    fn control_under_point(
        &self,
        root: &IUIAutomationElement,
        point: Point,
        expected: i32,
    ) -> windows::core::Result<Option<IUIAutomationElement>> {
        match self.interactive_at(root, point)? {
            Some(element) if role(&element)?.0 == expected => Ok(Some(element)),
            _ => Ok(None),
        }
    }

    fn landmarks(
        &self,
        root: &IUIAutomationElement,
    ) -> windows::core::Result<IUIAutomationElementArray> {
        unsafe {
            let condition = |id: UIA_CONTROLTYPE_ID| {
                self.client
                    .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(id.0))
            };
            let labels = self.client.CreateOrCondition(
                &condition(UIA_TextControlTypeId)?,
                &condition(UIA_HeaderItemControlTypeId)?,
            )?;
            root.FindAll(TreeScope_Descendants, &labels)
        }
    }
}

fn element_region(element: &IUIAutomationElement) -> Option<Region> {
    let rect = unsafe { element.CurrentBoundingRectangle() }.ok()?;
    Region::from_rect(windows_sys::Win32::Foundation::RECT {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    })
}

fn target_samples(region: Region, accessible: bool) -> Region {
    if !accessible {
        return region;
    }
    // Native control borders change with hover, focus, and default-button state.
    // Keep the identifying interior and leave border geometry to UI Automation.
    let x = (region.size.x / 8).min((region.size.x - 20).max(0) / 2);
    let y = (region.size.y / 5).min((region.size.y - 12).max(0) / 2);
    Region {
        origin: sum(region.origin, Point { x, y }),
        size: Point {
            x: region.size.x - 2 * x,
            y: region.size.y - 2 * y,
        },
    }
}

fn difference(a: Point, b: Point) -> Point {
    Point {
        x: a.x.saturating_sub(b.x),
        y: a.y.saturating_sub(b.y),
    }
}

fn sum(a: Point, b: Point) -> Point {
    Point {
        x: a.x.saturating_add(b.x),
        y: a.y.saturating_add(b.y),
    }
}

fn distance(offset: Point, dpi: i32) -> i64 {
    let Point { x, y } = logical(offset, dpi);
    i64::from(x).pow(2).saturating_add(i64::from(y).pow(2))
}

fn logical(value: Point, dpi: i32) -> Point {
    let convert = |value: i32| (f64::from(value) * 96.0 / f64::from(dpi)).round() as i32;
    Point {
        x: convert(value.x),
        y: convert(value.y),
    }
}

fn scale(value: Point, dpi: i32) -> Option<Point> {
    let convert = |value: i32| (f64::from(value) * f64::from(dpi) / 96.0).round() as i32;
    (value.x.unsigned_abs() <= 1024 && value.y.unsigned_abs() <= 1024).then_some(Point {
        x: convert(value.x),
        y: convert(value.y),
    })
}

fn compatible(actual: Point, expected: Point) -> bool {
    let check = |actual: i32, expected: i32| {
        expected > 0 && (actual - expected).abs() <= (expected / 5).max(3)
    };
    check(actual.x, expected.x) && check(actual.y, expected.y)
}

fn render_scale(actual: Point, saved: Point, dpi: i32) -> Option<i32> {
    let expected = scale(saved, dpi)?;
    if compatible(actual, expected) {
        return Some(dpi);
    }
    if expected.x <= 0 || expected.y <= 0 {
        return None;
    }
    // Some applications zoom their contents independently of monitor DPI.
    // Require consistent scaling in both axes before projecting the target.
    let x = f64::from(actual.x) / f64::from(expected.x);
    let y = f64::from(actual.y) / f64::from(expected.y);
    ((0.65..=1.8).contains(&x) && (x - y).abs() <= 0.07)
        .then_some((f64::from(dpi) * (x + y) / 2.0).round() as i32)
}

fn overlaps(a: Region, b: Region) -> bool {
    a.origin.x < b.origin.x + b.size.x
        && b.origin.x < a.origin.x + a.size.x
        && a.origin.y < b.origin.y + b.size.y
        && b.origin.y < a.origin.y + a.size.y
}

fn contains(region: Region, point: Point) -> bool {
    let delta = difference(point, region.origin);
    (0..region.size.x).contains(&delta.x) && (0..region.size.y).contains(&delta.y)
}

fn region_bounds(region: Region) -> Zone {
    Zone {
        from: region.origin,
        to: sum(
            region.origin,
            Point {
                x: region.size.x - 1,
                y: region.size.y - 1,
            },
        ),
    }
}

fn equivalent(a: &Target, b: &Target) -> bool {
    a.native == b.native
        && a.control_window == b.control_window
        && a.bounds == b.bounds
        && distance(difference(a.point, b.point), a.native.dpi.max(96) as i32) <= 2 * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_geometry_is_dpi_relative_and_handles_negative_coordinates() {
        let reference = Point { x: -31, y: 57 };
        for dpi in [96, 120, 144, 192, 240] {
            let pixels = scale(reference, dpi).unwrap();
            assert_eq!(logical(pixels, dpi), reference);
            let anchor = Point { x: -2000, y: 112 };
            assert_eq!(difference(sum(anchor, pixels), anchor), pixels);
            assert!(compatible(
                scale(Point { x: 116, y: 28 }, dpi).unwrap(),
                scale(Point { x: 120, y: 30 }, dpi).unwrap()
            ));
        }
        assert!(scale(Point { x: i32::MIN, y: 0 }, 192).is_none());
        assert_eq!(
            render_scale(Point { x: 264, y: 44 }, Point { x: 176, y: 29 }, 96),
            Some(145)
        );
        assert_eq!(
            render_scale(Point { x: 264, y: 29 }, Point { x: 176, y: 29 }, 96),
            None
        );
    }
}
