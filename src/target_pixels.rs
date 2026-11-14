use crate::{mouse::Point, targets::Patch};
use std::{ffi::c_void, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::POINT,
    Foundation::{HWND, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, ReleaseDC, SRCCOPY,
        SelectObject,
    },
    UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, WindowFromPoint},
};

const COLS: usize = 20;
const ROWS: usize = 12;
const PIXELS: usize = COLS * ROWS;
const MAX_SIDE: i32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub origin: Point,
    pub size: Point,
}

impl Region {
    pub fn from_rect(rect: RECT) -> Option<Self> {
        let size = Point {
            x: rect.right.checked_sub(rect.left)?,
            y: rect.bottom.checked_sub(rect.top)?,
        };
        ((1..=MAX_SIDE).contains(&size.x) && (1..=MAX_SIDE).contains(&size.y)).then_some(Self {
            origin: Point {
                x: rect.left,
                y: rect.top,
            },
            size,
        })
    }

    pub fn centered(center: Point, size: Point) -> Self {
        Self {
            origin: Point {
                x: center.x - size.x / 2,
                y: center.y - size.y / 2,
            },
            size,
        }
    }

    pub fn center(self) -> Point {
        Point {
            x: self.origin.x + self.size.x / 2,
            y: self.origin.y + self.size.y / 2,
        }
    }

    pub fn visible(self, window: HWND) -> bool {
        [1, 4, 7].into_iter().all(|y| {
            [1, 4, 7].into_iter().all(|x| {
                let point = POINT {
                    x: self.origin.x + self.size.x * x / 8,
                    y: self.origin.y + self.size.y * y / 8,
                };
                unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) == window }
            })
        })
    }

    pub fn union(self, other: Self) -> Option<Self> {
        Self::from_rect(RECT {
            left: self.origin.x.min(other.origin.x),
            top: self.origin.y.min(other.origin.y),
            right: (self.origin.x + self.size.x).max(other.origin.x + other.size.x),
            bottom: (self.origin.y + self.size.y).max(other.origin.y + other.size.y),
        })
    }
}

pub struct Frame {
    region: Region,
    gray: Vec<u8>,
}

impl Frame {
    pub fn capture(region: Region) -> Option<Self> {
        let Point {
            x: width,
            y: height,
        } = region.size;
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return None;
        }
        unsafe {
            let screen = Screen(GetDC(null_mut()));
            if screen.0.is_null() {
                return None;
            }
            let memory = Memory(CreateCompatibleDC(screen.0));
            if memory.0.is_null() {
                return None;
            }
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize =
                size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits = null_mut::<c_void>();
            let bitmap = Bitmap(CreateDIBSection(
                screen.0,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                null_mut(),
                0,
            ));
            if bitmap.0.is_null() || bits.is_null() {
                return None;
            }
            let old = SelectObject(memory.0, bitmap.0);
            if old.is_null() || old as isize == -1 {
                return None;
            }
            let copied = BitBlt(
                memory.0,
                0,
                0,
                width,
                height,
                screen.0,
                region.origin.x,
                region.origin.y,
                SRCCOPY | CAPTUREBLT,
            ) != 0;
            let flushed = windows_sys::Win32::Graphics::Gdi::GdiFlush() != 0;
            SelectObject(memory.0, old);
            if !copied || !flushed {
                return None;
            }
            let bytes =
                std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize);
            let gray = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| {
                    ((29 * u16::from(b[0]) + 150 * u16::from(b[1]) + 77 * u16::from(b[2])) >> 8)
                        as u8
                })
                .collect();
            Some(Self { region, gray })
        }
    }

    pub fn signature(&self, region: Region) -> Option<Signature> {
        let x = usize::try_from(region.origin.x.checked_sub(self.region.origin.x)?).ok()?;
        let y = usize::try_from(region.origin.y.checked_sub(self.region.origin.y)?).ok()?;
        let w = usize::try_from(region.size.x).ok()?;
        let h = usize::try_from(region.size.y).ok()?;
        let stride = self.region.size.x as usize;
        if w < COLS
            || h < ROWS
            || x.checked_add(w)? > stride
            || y.checked_add(h)? > self.region.size.y as usize
        {
            return None;
        }
        let mut values = [0; PIXELS];
        for (index, value) in values.iter_mut().enumerate() {
            let col = index % COLS;
            let row = index / COLS;
            let (left, right) = (x + col * w / COLS, x + (col + 1) * w / COLS);
            let (top, bottom) = (y + row * h / ROWS, y + (row + 1) * h / ROWS);
            let mut sum = 0u32;
            for r in top..bottom {
                sum += self.gray[r * stride + left..r * stride + right]
                    .iter()
                    .map(|&n| u32::from(n))
                    .sum::<u32>();
            }
            *value = (sum / ((right - left) * (bottom - top)) as u32) as u8;
        }
        Some(Signature(values))
    }
}

struct Screen(HDC);
impl Drop for Screen {
    fn drop(&mut self) {
        unsafe { ReleaseDC(null_mut(), self.0) };
    }
}
struct Memory(HDC);
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { DeleteDC(self.0) };
    }
}
struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.0) };
    }
}

#[derive(Clone, Copy)]
pub struct Signature([u8; PIXELS]);

impl Signature {
    pub fn read(patch: &Patch) -> Option<Self> {
        if patch.0.len() != PIXELS * 2 {
            return None;
        }
        let mut values = [0; PIXELS];
        for (value, pair) in values.iter_mut().zip(patch.0.as_bytes().as_chunks::<2>().0) {
            let hex = |byte: u8| char::from(byte).to_digit(16).map(|n| n as u8);
            *value = hex(pair[0])? * 16 + hex(pair[1])?;
        }
        Some(Self(values))
    }

    pub fn store(self) -> Option<Patch> {
        let (_, energy) = self.stats();
        (energy >= (PIXELS * 36) as f64)
            .then(|| Patch(self.0.iter().map(|byte| format!("{byte:02x}")).collect()))
    }

    fn stats(self) -> (f64, f64) {
        let sum = self.0.iter().map(|&n| f64::from(n)).sum::<f64>();
        let mean = sum / PIXELS as f64;
        (
            mean,
            self.0.iter().map(|&n| (f64::from(n) - mean).powi(2)).sum(),
        )
    }

    pub fn similarity(self, other: Self) -> f64 {
        self.compare(other, 0, 0)
    }

    pub fn scale_similarity(self, other: Self) -> f64 {
        // Native text can move by a raster cell when Windows hints the font
        // anew. The physical region and anchor are checked independently.
        let direct = self.similarity(other);
        if direct >= 0.9 {
            return direct;
        }
        let blurred = (self.blurred(), other.blurred());
        (-1..=1)
            .flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)))
            .map(|(dx, dy)| {
                self.compare(other, dx, dy)
                    .max(blurred.0.compare(blurred.1, dx, dy))
            })
            .fold(direct, f64::max)
    }

    fn blurred(self) -> Self {
        Self(std::array::from_fn(|i| {
            let row = i / COLS;
            let col = i % COLS;
            let mut total = 0u16;
            let mut count = 0u16;
            for y in row.saturating_sub(1)..=(row + 1).min(ROWS - 1) {
                for x in col.saturating_sub(1)..=(col + 1).min(COLS - 1) {
                    total += u16::from(self.0[y * COLS + x]);
                    count += 1;
                }
            }
            (total / count) as u8
        }))
    }

    fn compare(self, other: Self, dx: i32, dy: i32) -> f64 {
        let (left, left_energy) = self.stats();
        let (right, right_energy) = other.stats();
        if left_energy < (PIXELS * 36) as f64 || right_energy < (PIXELS * 36) as f64 {
            return 0.0;
        }
        let cross = self
            .0
            .iter()
            .enumerate()
            .filter_map(|(index, &value)| {
                let row = index as i32 / COLS as i32 + dy;
                let col = index as i32 % COLS as i32 + dx;
                ((0..ROWS as i32).contains(&row) && (0..COLS as i32).contains(&col)).then(|| {
                    let pixel = other.0[row as usize * COLS + col as usize];
                    (f64::from(value) - left) * (f64::from(pixel) - right)
                })
            })
            .sum::<f64>();
        cross / (left_energy * right_energy).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_fingerprint_is_checked_and_rejects_flat_or_reversed_patterns() {
        let good = Signature(std::array::from_fn(|i| (30 + i % 120) as u8));
        let bright = Signature(good.0.map(|n| n + 50));
        let reversed = Signature(good.0.map(|n| 255 - n));
        let stored = good.store().unwrap();
        assert!(Signature::read(&stored).unwrap().similarity(bright) > 0.99);
        assert!(good.similarity(reversed) < 0.0);
        assert!(Signature([120; PIXELS]).store().is_none());
        assert!(Signature::read(&Patch("é".repeat(PIXELS))).is_none());
    }

    #[test]
    fn area_sampling_preserves_the_same_pattern_at_different_pixel_scales() {
        let make = |scale: usize, origin| {
            let region = Region {
                origin,
                size: Point {
                    x: (COLS * scale) as i32,
                    y: (ROWS * scale) as i32,
                },
            };
            let gray = (0..ROWS * scale)
                .flat_map(|y| {
                    (0..COLS * scale).map(move |x| {
                        if ((x / scale) * 3 + y / scale * 7) % 13 < 4 {
                            45
                        } else {
                            221
                        }
                    })
                })
                .collect();
            Frame { region, gray }
        };
        let initial = make(2, Point { x: -1200, y: -45 });
        let scaled = make(3, Point { x: 2000, y: 300 });
        let first = initial.signature(initial.region).unwrap();
        let second = scaled.signature(scaled.region).unwrap();
        assert_eq!(first.0, second.0);
    }
}
