//! Anti-aliased primitives for custom control chrome.
//!
//! GDI's `RoundRect`/`Ellipse` fill without antialiasing and distort as soon
//! as the corner radius approaches the shorter side: the 38x22 toggle track
//! with radius 22 degenerated into an octagon with stair-stepped edges. These
//! helpers rasterize into a premultiplied ARGB surface at device resolution and
//! `AlphaBlend` the result, so corners stay true circles at every DPI.
use std::{ffi::c_void, ptr::null_mut};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HDC, MM_TEXT,
    RestoreDC, SaveDC, SelectObject, SetMapMode,
};

/// Device-pixel rectangle. Half-open on the right/bottom edge: `right` and
/// `bottom` are exclusive, matching how the rasterizer walks pixel centres.
#[derive(Clone, Copy)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
    pub fn inset(self, amount: f32) -> Self {
        Self {
            left: self.left + amount,
            top: self.top + amount,
            right: self.right - amount,
            bottom: self.bottom - amount,
        }
    }
    fn width(self) -> f32 {
        self.right - self.left
    }
    fn height(self) -> f32 {
        self.bottom - self.top
    }
}

/// COLORREF (0x00BBGGRR) at full opacity, then premultiplied by `alpha`.
/// Channel order matches the 32bpp BGRA DIB layout used by `CreateDIBSection`.
pub fn premultiplied(color: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let r = (color & 255) * a / 255;
    let g = ((color >> 8) & 255) * a / 255;
    let b = ((color >> 16) & 255) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Linear blend between two COLORREFs. `weight == 0` keeps `base`, 255 keeps
/// `overlay`; used for the rim/fill split inside device symbols. Exact at both
/// ends so approved 1:1 rasters stay bit-identical.
pub fn mix(base: u32, overlay: u32, weight: u8) -> u32 {
    let t = u32::from(weight);
    let inverse = 255 - t;
    let r = (base & 255) * inverse / 255 + (overlay & 255) * t / 255;
    let g = ((base >> 8) & 255) * inverse / 255 + ((overlay >> 8) & 255) * t / 255;
    let b = ((base >> 16) & 255) * inverse / 255 + ((overlay >> 16) & 255) * t / 255;
    r | (g << 8) | (b << 16)
}

/// Premultiplied ARGB canvas. Zero-initialized (fully transparent).
pub struct Surface {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Surface {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width * height],
        }
    }

    fn over(&mut self, x: usize, y: usize, color: u32, coverage: f32) {
        let alpha = (coverage * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        if alpha == 0 {
            return;
        }
        let index = y * self.width + x;
        if alpha == 255 {
            self.pixels[index] = premultiplied(color, 255);
            return;
        }
        let source = premultiplied(color, alpha);
        let inverse = 255 - u32::from(alpha);
        let destination = self.pixels[index];
        let blend = |shift: u32| {
            ((source >> shift) & 255) + (((destination >> shift) & 255) * inverse / 255)
        };
        self.pixels[index] = (blend(24) << 24) | (blend(16) << 16) | (blend(8) << 8) | blend(0);
    }

    /// Signed distance from `(px, py)` to a rounded rectangle; negative inside.
    fn distance(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
        let radius = radius
            .min(rect.width().abs() * 0.5)
            .min(rect.height().abs() * 0.5)
            .max(0.0);
        let half_x = rect.width().abs() * 0.5 - radius;
        let half_y = rect.height().abs() * 0.5 - radius;
        let dx = (px - (rect.left + rect.right) * 0.5).abs() - half_x;
        let dy = (py - (rect.top + rect.bottom) * 0.5).abs() - half_y;
        let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
        outside + dx.max(dy).min(0.0) - radius
    }

    /// Fill a rounded rectangle. Edges are antialiased at device resolution;
    /// a radius larger than half the shorter side clamps to a true pill.
    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, color: u32) {
        let left = (rect.left.min(rect.right) - 1.0).floor().max(0.0) as usize;
        let top = (rect.top.min(rect.bottom) - 1.0).floor().max(0.0) as usize;
        let right = (rect.left.max(rect.right) + 1.0)
            .ceil()
            .min(self.width as f32) as usize;
        let bottom = (rect.top.max(rect.bottom) + 1.0)
            .ceil()
            .min(self.height as f32) as usize;
        for y in top..bottom {
            for x in left..right {
                let coverage = (0.5 - Self::distance(x as f32 + 0.5, y as f32 + 0.5, rect, radius))
                    .clamp(0.0, 1.0);
                self.over(x, y, color, coverage);
            }
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, color: u32) {
        self.fill_round_rect(rect, 0.0, color);
    }

    pub fn fill_circle(&mut self, center: (f32, f32), radius: f32, color: u32) {
        self.fill_round_rect(
            Rect::new(
                center.0 - radius,
                center.1 - radius,
                center.0 + radius,
                center.1 + radius,
            ),
            radius,
            color,
        );
    }

    /// Inside stroke: the ring occupies `[left, left + width]` and keeps the
    /// outer edge on the rounded rectangle. Only valid on a still-empty
    /// surface; the interior is punched out so the caller's background shows.
    pub fn stroke_round_rect(&mut self, rect: Rect, radius: f32, width: f32, color: u32) {
        if width <= 0.0 {
            return;
        }
        self.fill_round_rect(rect, radius, color);
        let inner = rect.inset(width);
        let inner_radius = (radius - width).max(0.0);
        let left = (inner.left - 1.0).floor().max(0.0) as usize;
        let top = (inner.top - 1.0).floor().max(0.0) as usize;
        let right = (inner.right + 1.0).ceil().min(self.width as f32) as usize;
        let bottom = (inner.bottom + 1.0).ceil().min(self.height as f32) as usize;
        for y in top..bottom {
            for x in left..right {
                let coverage = (0.5
                    - Self::distance(x as f32 + 0.5, y as f32 + 0.5, inner, inner_radius))
                .clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let keep = 1.0 - coverage;
                let index = y * self.width + x;
                let pixel = self.pixels[index];
                let scale = |shift: u32| (((pixel >> shift) & 255) as f32 * keep + 0.5) as u32;
                self.pixels[index] =
                    (scale(24) << 24) | (scale(16) << 16) | (scale(8) << 8) | scale(0);
            }
        }
    }

    /// Even-odd fill with 4x4 coverage sampling; used for the charging bolt,
    /// whose diagonal edges have no pixel grid to snap to.
    pub fn fill_polygon(&mut self, points: &[(f32, f32)], color: u32) {
        if points.len() < 3 {
            return;
        }
        let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in points {
            left = left.min(*x);
            top = top.min(*y);
            right = right.max(*x);
            bottom = bottom.max(*y);
        }
        let left = left.floor().max(0.0) as usize;
        let top = top.floor().max(0.0) as usize;
        let right = right.ceil().min(self.width as f32) as usize;
        let bottom = bottom.ceil().min(self.height as f32) as usize;
        const SAMPLES: usize = 4;
        for y in top..bottom {
            for x in left..right {
                let mut inside = 0;
                for sy in 0..SAMPLES {
                    for sx in 0..SAMPLES {
                        let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                        let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                        if Self::contains(points, px, py) {
                            inside += 1;
                        }
                    }
                }
                if inside > 0 {
                    self.over(x, y, color, inside as f32 / (SAMPLES * SAMPLES) as f32);
                }
            }
        }
    }

    fn contains(points: &[(f32, f32)], x: f32, y: f32) -> bool {
        let mut inside = false;
        let mut previous = points.len() - 1;
        for current in 0..points.len() {
            let (xi, yi) = points[current];
            let (xj, yj) = points[previous];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            previous = current;
        }
        inside
    }

    /// Composite onto `dc` at device-pixel `(x, y)`. Temporarily switches to
    /// `MM_TEXT` so the caller's logical mapping cannot rescale the blend.
    pub fn blend_onto(&self, dc: HDC, x: i32, y: i32) -> bool {
        if self.width == 0 || self.height == 0 {
            return true;
        }
        unsafe {
            let memory = CreateCompatibleDC(dc);
            if memory.is_null() {
                return false;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.width as i32,
                    biHeight: -(self.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = null_mut();
            let bitmap =
                CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(memory);
                return false;
            }
            std::ptr::copy_nonoverlapping(
                self.pixels.as_ptr(),
                bits.cast::<u32>(),
                self.pixels.len(),
            );
            let previous = SelectObject(memory, bitmap);
            let saved = SaveDC(dc);
            SetMapMode(dc, MM_TEXT);
            let ok = AlphaBlend(
                dc,
                x,
                y,
                self.width as i32,
                self.height as i32,
                memory,
                0,
                0,
                self.width as i32,
                self.height as i32,
                BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                },
            ) != 0;
            RestoreDC(dc, saved);
            SelectObject(memory, previous);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_matches_dib_channel_order() {
        // COLORREF 0x00BBGGRR with R=0x30 G=0x2D B=0x29 -> DIB 0xAARRGGBB.
        assert_eq!(premultiplied(0x00292d30, 255), 0xff302d29);
        assert_eq!(premultiplied(0x00292d30, 0), 0);
        assert_eq!(premultiplied(0x00292d30, 128), 0x80181614);
    }

    #[test]
    fn pill_track_is_a_true_capsule_with_antialiased_rim() {
        let mut surface = Surface::new(40, 24);
        surface.fill_round_rect(Rect::new(1.0, 1.0, 39.0, 23.0), 11.0, 0x00ffffff);
        // Vertically centered pixels just inside both ends are opaque.
        assert_eq!(surface.pixels[12 * 40 + 3], 0xffffffff);
        assert_eq!(surface.pixels[12 * 40 + 36], 0xffffffff);
        assert_eq!(surface.pixels[12 * 40 + 20], 0xffffffff);
        // The extreme end pixel is partly covered, which is the antialiasing.
        let end = surface.pixels[12 * 40 + 1] >> 24;
        assert!((180..255).contains(&end), "unexpected end coverage {end}");
        // Corner of the bounding box is outside the capsule.
        assert_eq!(surface.pixels[0], 0);
        assert_eq!(surface.pixels[23 * 40 + 39] >> 24, 0);
    }

    #[test]
    fn oversized_radius_clamps_instead_of_faceting() {
        // The pre-fix toggle asked for radius 22 on a 22-tall track; GDI
        // turned that into an octagon. The rasterizer must clamp to a pill.
        let mut surface = Surface::new(38, 22);
        surface.fill_round_rect(Rect::new(0.0, 0.0, 38.0, 22.0), 22.0, 0x00ff0000);
        assert_ne!(surface.pixels[11 * 38] >> 24, 0);
        assert_ne!(surface.pixels[11 * 38 + 37] >> 24, 0);
        assert_eq!(surface.pixels[0] >> 24, 0);
        assert_eq!(surface.pixels[21 * 38 + 37] >> 24, 0);
    }

    #[test]
    fn inside_stroke_is_one_crisp_pixel_at_integer_geometry() {
        let mut surface = Surface::new(40, 24);
        surface.stroke_round_rect(Rect::new(2.0, 2.0, 38.0, 22.0), 6.0, 1.0, 0x00ffffff);
        // The border occupies exactly the first pixel row of the straight edge.
        assert_eq!(surface.pixels[2 * 40 + 10], 0xffffffff);
        assert_eq!(surface.pixels[2 * 40 + 20], 0xffffffff);
        // ...and the neighbouring pixel outside stays clear.
        assert_eq!(surface.pixels[40 + 10], 0);
        // Interior is empty.
        assert_eq!(surface.pixels[12 * 40 + 20], 0);
    }

    #[test]
    fn polygon_fill_covers_its_center_and_antialiases_edges() {
        let mut surface = Surface::new(24, 24);
        surface.fill_polygon(&[(12.0, 2.0), (20.0, 22.0), (4.0, 22.0)], 0x00ffffff);
        assert_eq!(surface.pixels[14 * 24 + 12], 0xffffffff);
        assert_eq!(surface.pixels[0], 0);
        assert!(
            surface
                .pixels
                .iter()
                .any(|pixel| (1..255).contains(&(pixel >> 24)))
        );
    }

    #[test]
    fn mix_is_exact_at_both_ends() {
        assert_eq!(mix(0x00112233, 0x00445566, 0), 0x00112233);
        assert_eq!(mix(0x00112233, 0x00445566, 255), 0x00445566);
    }
}
