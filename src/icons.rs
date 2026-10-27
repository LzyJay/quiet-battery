//! Native owned device symbols and percentage tray meters, using approved v10 silhouettes.
use crate::device::DeviceKind;
use crate::shapes::{mix, premultiplied};
use std::{ffi::c_void, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, COLOR_BTNFACE, COLOR_BTNTEXT, CreateBitmap,
        CreateDIBSection, DIB_RGB_COLORS, DeleteObject, GetSysColor,
    },
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, HICON, ICONINFO, SPI_GETHIGHCONTRAST,
        SystemParametersInfoW,
    },
};

include!("../assets/generated/symbol_masks.rs");
include!("../assets/generated/tray_masks.rs");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryVisual {
    Percent(u8),
    Unknown,
    Offline,
}

#[derive(Clone, Copy)]
enum Appearance {
    Light,
    Dark,
    HighContrast { foreground: u32, face: u32 },
}
impl Appearance {
    fn current() -> Self {
        if high_contrast_enabled() {
            Self::HighContrast {
                foreground: unsafe { GetSysColor(COLOR_BTNTEXT) },
                face: unsafe { GetSysColor(COLOR_BTNFACE) },
            }
        } else if system_uses_light_theme() {
            Self::Light
        } else {
            Self::Dark
        }
    }
    // Colors below are Win32 COLORREF, not ARGB.
    fn colors(self) -> (u32, u32, u32) {
        match self {
            Self::Light => (0x002f3330, 0x004367b5, 0x00ecf2f5),
            Self::Dark => (0x00eaf8ff, 0x0077b4f1, 0x00252726),
            Self::HighContrast { foreground, face } => (foreground, foreground, face),
        }
    }
}

/// Exactly one currently rendered native icon per monitored device.
/// Replace the Shell reference before dropping the previous instance.
pub struct TrayIcon(OwnedIcon);
impl TrayIcon {
    pub fn software(pixels: i32) -> Result<Self, String> {
        use windows_sys::Win32::{
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{IMAGE_ICON, LoadImageW},
        };
        let icon = unsafe {
            LoadImageW(
                GetModuleHandleW(std::ptr::null()),
                101usize as *const u16,
                IMAGE_ICON,
                pixels,
                pixels,
                0,
            )
        };
        if icon.is_null() {
            Err("无法加载软件入口图标".into())
        } else {
            Ok(Self(OwnedIcon(icon)))
        }
    }
    pub fn new(pixels: i32, kind: DeviceKind, value: BatteryVisual) -> Result<Self, String> {
        Self::with_appearance(pixels, kind, value, Appearance::current())
    }
    fn with_appearance(
        pixels: i32,
        kind: DeviceKind,
        value: BatteryVisual,
        appearance: Appearance,
    ) -> Result<Self, String> {
        check_size(pixels)?;
        if matches!(value, BatteryVisual::Percent(n) if n > 100) {
            return Err("Battery value outside 0..100".into());
        }
        let image = tray_pixels(pixels as usize, kind, value, appearance);
        Ok(Self(OwnedIcon::from_pixels(pixels, &image)?))
    }
    pub fn get(&self) -> HICON {
        self.0.0
    }
}

/// Standalone v10 silhouette, with the same fill and rim as the tray glyph.
/// The warm paper window uses the light palette; high contrast uses system colors.
pub struct SymbolIcon(OwnedIcon);
impl SymbolIcon {
    pub fn new(pixels: i32, kind: DeviceKind) -> Result<Self, String> {
        let appearance = if high_contrast_enabled() {
            Appearance::current()
        } else {
            Appearance::Light
        };
        Self::with_appearance(pixels, kind, appearance)
    }
    fn with_appearance(
        pixels: i32,
        kind: DeviceKind,
        appearance: Appearance,
    ) -> Result<Self, String> {
        check_size(pixels)?;
        let image = symbol_pixels(pixels as usize, kind, appearance);
        Ok(Self(OwnedIcon::from_pixels(pixels, &image)?))
    }
    pub fn get(&self) -> HICON {
        self.0.0
    }
}
fn symbol_pixels(n: usize, kind: DeviceKind, appearance: Appearance) -> Vec<u32> {
    let (rim, fill, _) = appearance.colors();
    sample_layer(&SYMBOL_LAYERS, &SYMBOL_SIZES, kind, n)
        .into_iter()
        .map(|(alpha, inner)| premultiplied(mix(rim, fill, inner), alpha))
        .collect()
}

/// Interleaved `(alpha, inner)` silhouette for one device kind at `n` pixels.
/// Approved sizes are copied untouched, so the frozen regression rasters stay
/// bit-identical. Other sizes shrink through an area box filter and grow
/// through bilinear interpolation: the old nearest-neighbour sampling dropped
/// or duplicated whole rows and left 175% / 225% / 250% displays jagged.
fn sample_layer(layers: &[&[u8]], sizes: &[usize], kind: DeviceKind, n: usize) -> Vec<(u8, u8)> {
    // Prefer the smallest approved raster at or above the target: shrinking
    // preserves the drawn edge better than stretching a smaller silhouette.
    let slot = sizes
        .iter()
        .position(|size| *size >= n)
        .unwrap_or(sizes.len() - 1);
    let source = sizes[slot];
    let offset = kind_index(kind) * source * source * 2;
    let data = &layers[slot][offset..offset + source * source * 2];
    if n == source {
        return (0..n * n)
            .map(|index| (data[index * 2], data[index * 2 + 1]))
            .collect();
    }
    let mut output = vec![(0u8, 0u8); n * n];
    if n < source {
        // Summed-area tables keep every output pixel at four lookups.
        let stride = source + 1;
        let mut alpha_sum = vec![0u32; stride * stride];
        let mut inner_sum = vec![0u32; stride * stride];
        for y in 0..source {
            let (mut alpha_row, mut inner_row) = (0u32, 0u32);
            for x in 0..source {
                let at = (y * source + x) * 2;
                alpha_row += u32::from(data[at]);
                inner_row += u32::from(data[at + 1]);
                alpha_sum[(y + 1) * stride + x + 1] = alpha_sum[y * stride + x + 1] + alpha_row;
                inner_sum[(y + 1) * stride + x + 1] = inner_sum[y * stride + x + 1] + inner_row;
            }
        }
        let area = |sums: &[u32], x0: usize, y0: usize, x1: usize, y1: usize| {
            sums[y1 * stride + x1] + sums[y0 * stride + x0]
                - sums[y0 * stride + x1]
                - sums[y1 * stride + x0]
        };
        for y in 0..n {
            let y0 = y * source / n;
            let y1 = ((y + 1) * source / n).max(y0 + 1).min(source);
            for x in 0..n {
                let x0 = x * source / n;
                let x1 = ((x + 1) * source / n).max(x0 + 1).min(source);
                let count = ((x1 - x0) * (y1 - y0)) as u32;
                output[y * n + x] = (
                    (area(&alpha_sum, x0, y0, x1, y1) / count) as u8,
                    (area(&inner_sum, x0, y0, x1, y1) / count) as u8,
                );
            }
        }
        return output;
    }
    // Growth: bilinear over transparent padding, so the silhouette fades in at
    // its rim instead of turning into 2x2 blocks.
    let step = source as f32 / n as f32;
    let texel = |x: i32, y: i32, channel: usize| -> f32 {
        if x < 0 || y < 0 || x >= source as i32 || y >= source as i32 {
            0.0
        } else {
            f32::from(data[(y as usize * source + x as usize) * 2 + channel])
        }
    };
    for y in 0..n {
        let fy = (y as f32 + 0.5) * step - 0.5;
        let y0 = fy.floor();
        let wy = fy - y0;
        let y0 = y0 as i32;
        for x in 0..n {
            let fx = (x as f32 + 0.5) * step - 0.5;
            let x0 = fx.floor();
            let wx = fx - x0;
            let x0 = x0 as i32;
            let mut channels = [0u8; 2];
            for (channel, value) in channels.iter_mut().enumerate() {
                let top = texel(x0, y0, channel) * (1.0 - wx) + texel(x0 + 1, y0, channel) * wx;
                let bottom =
                    texel(x0, y0 + 1, channel) * (1.0 - wx) + texel(x0 + 1, y0 + 1, channel) * wx;
                *value = (top * (1.0 - wy) + bottom * wy).round().clamp(0.0, 255.0) as u8;
            }
            output[y * n + x] = (channels[0], channels[1]);
        }
    }
    output
}

fn kind_index(kind: DeviceKind) -> usize {
    match kind {
        DeviceKind::Mouse => 0,
        DeviceKind::Keyboard => 1,
        DeviceKind::Numpad => 2,
        DeviceKind::Headset => 3,
        DeviceKind::Earbuds => 4,
        DeviceKind::Gamepad => 5,
        DeviceKind::Touchpad => 6,
        DeviceKind::Trackball => 7,
        DeviceKind::Tablet => 8,
        DeviceKind::Pen => 9,
        DeviceKind::Microphone => 10,
        DeviceKind::Speaker => 11,
        DeviceKind::Remote => 12,
        DeviceKind::Camera => 13,
        DeviceKind::Presenter => 14,
        DeviceKind::Generic => 15,
    }
}
fn check_size(pixels: i32) -> Result<(), String> {
    if !(16..=256).contains(&pixels) {
        Err("Invalid native icon size".into())
    } else {
        Ok(())
    }
}
fn meter_fill(n: usize, p: u8) -> usize {
    let value = (n - 4) * usize::from(p);
    let whole = value / 100;
    let remainder = value % 100;
    whole + usize::from(remainder > 50 || (remainder == 50 && whole % 2 == 1))
}
fn tray_pixels(
    n: usize,
    kind: DeviceKind,
    value: BatteryVisual,
    appearance: Appearance,
) -> Vec<u32> {
    let (rim, color, bg) = appearance.colors();
    let mut image: Vec<u32> = sample_layer(&TRAY_LAYERS, &TRAY_SIZES, kind, n)
        .into_iter()
        .map(|(alpha, inner)| premultiplied(mix(rim, color, inner), alpha))
        .collect();
    let gauge = if n <= 20 { 4 } else { 5 };
    let x = n - gauge;
    let top = 1;
    let bottom = n - 2;
    for y in top..=bottom {
        for xx in x..n {
            image[y * n + xx] = premultiplied(
                if xx == x || xx == n - 1 || y == top || y == bottom {
                    rim
                } else {
                    bg
                },
                255,
            );
        }
    }
    match value {
        BatteryVisual::Percent(p) => {
            let height = meter_fill(n, p);
            for y in bottom - height..bottom {
                for xx in x + 1..n - 1 {
                    image[y * n + xx] = premultiplied(color, 255);
                }
            }
        }
        BatteryVisual::Unknown => {
            for y in top + 1..bottom {
                for xx in x + 1..n - 1 {
                    image[y * n + xx] = premultiplied(rim, 255);
                }
            }
        }
        BatteryVisual::Offline => {
            for y in 2..n - 2 {
                let xx = (n - gauge - 2) * (n - 3 - y) / (n - 4);
                image[y * n + xx] = premultiplied(rim, 255);
            }
        }
    }
    image
}

struct OwnedIcon(HICON);
impl OwnedIcon {
    fn from_pixels(pixels: i32, image: &[u32]) -> Result<Self, String> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: pixels,
                biHeight: -pixels,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        unsafe {
            let bitmap =
                CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                return Err("Could not create device icon bitmap".into());
            }
            std::ptr::copy_nonoverlapping(image.as_ptr(), bits.cast::<u32>(), image.len());
            let mask_bits = vec![0u8; (pixels as usize).div_ceil(16) * 2 * pixels as usize];
            let mask = CreateBitmap(pixels, pixels, 1, 1, mask_bits.as_ptr().cast());
            if mask.is_null() {
                DeleteObject(bitmap);
                return Err("Could not create device icon mask".into());
            }
            let icon = CreateIconIndirect(&ICONINFO {
                fIcon: 1,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: bitmap,
            });
            DeleteObject(mask);
            DeleteObject(bitmap);
            if icon.is_null() {
                Err("Could not create device icon".into())
            } else {
                Ok(Self(icon))
            }
        }
    }
}
impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DestroyIcon(self.0);
            }
        }
    }
}

fn system_uses_light_theme() -> bool {
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
    let mut light = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut light as *mut u32).cast(),
            &mut size,
        ) == ERROR_SUCCESS
            && light != 0
    }
}

fn high_contrast_enabled() -> bool {
    #[repr(C)]
    struct HighContrast {
        size: u32,
        flags: u32,
        scheme: *mut u16,
    }
    let mut contrast = HighContrast {
        size: std::mem::size_of::<HighContrast>() as u32,
        flags: 0,
        scheme: null_mut(),
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.size,
            (&mut contrast as *mut HighContrast).cast(),
            0,
        ) != 0
            && contrast.flags & 1 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::premultiplied;
    use windows_sys::Win32::{
        Graphics::Gdi::{CreateCompatibleDC, DeleteDC, GetDIBits},
        UI::WindowsAndMessaging::GetIconInfo,
    };
    const KINDS: [DeviceKind; 16] = [
        DeviceKind::Mouse,
        DeviceKind::Keyboard,
        DeviceKind::Numpad,
        DeviceKind::Headset,
        DeviceKind::Earbuds,
        DeviceKind::Gamepad,
        DeviceKind::Touchpad,
        DeviceKind::Trackball,
        DeviceKind::Tablet,
        DeviceKind::Pen,
        DeviceKind::Microphone,
        DeviceKind::Speaker,
        DeviceKind::Remote,
        DeviceKind::Camera,
        DeviceKind::Presenter,
        DeviceKind::Generic,
    ];

    #[test]
    fn sample_layer_averages_when_shrinking_and_interpolates_when_growing() {
        // 4x4 coverage: opaque left half, transparent right half.
        let mut data = vec![0u8; 4 * 4 * 2];
        for y in 0..4 {
            for x in 0..2 {
                let at = (y * 4 + x) * 2;
                data[at] = 255;
                data[at + 1] = 255;
            }
        }
        let layers: &[&[u8]] = &[&data];
        let sizes = [4usize];
        // Shrinking keeps the two halves at full coverage: an area average,
        // not a point sample that would also land on 255 and 0 but shift the
        // boundary. Every column pair collapses to one exact half.
        let small = sample_layer(layers, &sizes, DeviceKind::Mouse, 2);
        assert_eq!(small[0], (255, 255));
        assert_eq!(small[1], (0, 0));
        // Growing must interpolate: the seam pixel is partially covered
        // instead of duplicating a source row.
        let large = sample_layer(layers, &sizes, DeviceKind::Mouse, 8);
        assert!(
            large.iter().any(|(alpha, _)| (1..255).contains(alpha)),
            "upscaling must antialias the seam"
        );
    }

    #[test]
    fn non_native_symbol_sizes_stay_antialiased() {
        // 175% DPI asks for 28 * 168 / 96 = 49 pixels; nearest-neighbour
        // sampling used to duplicate rows and leave the silhouette jagged.
        for n in [49usize, 63, 70] {
            let image = symbol_pixels(n, DeviceKind::Mouse, Appearance::Light);
            assert!(
                image.iter().any(|pixel| (1..255).contains(&(pixel >> 24))),
                "missing antialiasing at {n}px"
            );
            // Upscaled rim and fill blend into each other, so only the solid
            // interior is asserted here; exact colours stay a native-size
            // guarantee. The glyph must still be present and opaque.
            assert!(
                image.iter().filter(|p| **p >> 24 == 255).count() > n * n / 8,
                "silhouette collapsed at {n}px"
            );
            // Corners stay clear: the glyph never fills its square.
            for edge in 0..n {
                for index in [edge, (n - 1) * n + edge, edge * n, edge * n + n - 1] {
                    assert_eq!(image[index], 0, "edge pixel {index} at {n}px");
                }
            }
        }
    }
    #[test]
    fn native_tray_pixels_equal_frozen_approved_reference_rasters() {
        let references = include_bytes!("../assets/generated/tray-v10-reference.bin");
        let mut offset = 0;
        for kind in [
            DeviceKind::Mouse,
            DeviceKind::Keyboard,
            DeviceKind::Headset,
            DeviceKind::Gamepad,
        ] {
            for n in [16, 20, 24] {
                for appearance in [Appearance::Light, Appearance::Dark] {
                    let icon =
                        TrayIcon::with_appearance(n, kind, BatteryVisual::Percent(80), appearance)
                            .unwrap();
                    let actual = read_icon(icon.get(), n);
                    for pixel in actual {
                        assert_eq!(
                            pixel,
                            u32::from_le_bytes(references[offset..offset + 4].try_into().unwrap()),
                            "{kind:?}/{n}"
                        );
                        offset += 4;
                    }
                }
            }
        }
        assert_eq!(offset, references.len());
        for kind in KINDS {
            assert_ne!(
                tray_pixels(16, kind, BatteryVisual::Unknown, Appearance::Dark),
                tray_pixels(16, kind, BatteryVisual::Percent(0), Appearance::Dark)
            );
        }
    }
    fn read_icon(icon: HICON, n: i32) -> Vec<u32> {
        unsafe {
            let mut icon_info: ICONINFO = std::mem::zeroed();
            assert_ne!(GetIconInfo(icon, &mut icon_info), 0);
            let dc = CreateCompatibleDC(null_mut());
            assert!(!dc.is_null());
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: n,
                    biHeight: -n,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut image = vec![0u32; (n * n) as usize];
            let lines = GetDIBits(
                dc,
                icon_info.hbmColor,
                0,
                n as u32,
                image.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );
            DeleteDC(dc);
            DeleteObject(icon_info.hbmColor);
            DeleteObject(icon_info.hbmMask);
            assert_eq!(lines, n);
            image
        }
    }
    fn assert_released(icon: HICON) {
        unsafe {
            let mut info: ICONINFO = std::mem::zeroed();
            let valid = GetIconInfo(icon, &mut info);
            if valid != 0 {
                DeleteObject(info.hbmColor);
                DeleteObject(info.hbmMask);
            }
            assert_eq!(valid, 0, "native icon leaked after Drop");
        }
    }
    #[test]
    fn v10_native_icons_have_rim_continuous_range_and_release() {
        let appearances = [
            Appearance::Light,
            Appearance::Dark,
            Appearance::HighContrast {
                foreground: 0x0033ccff,
                face: 0x00552211,
            },
        ];
        for appearance in appearances {
            let (rim, fill, bg) = appearance.colors();
            for n in [16, 20, 24, 32, 48, 256] {
                for kind in KINDS {
                    for value in [0, 1, 19, 45, 75, 80, 87, 90, 100] {
                        let icon = TrayIcon::with_appearance(
                            n,
                            kind,
                            BatteryVisual::Percent(value),
                            appearance,
                        )
                        .unwrap();
                        let image = read_icon(icon.get(), n);
                        let size = n as usize;
                        let x = size - if size <= 20 { 4 } else { 5 };
                        let bottom = size - 2;
                        for y in 1..=bottom {
                            assert_eq!(image[y * size + size - 1], premultiplied(rim, 255));
                            assert_eq!(image[y * size + x], premultiplied(rim, 255));
                        }
                        let height = meter_fill(size, value);
                        for y in 2..bottom {
                            assert_eq!(
                                image[y * size + x + 1],
                                premultiplied(if y >= bottom - height { fill } else { bg }, 255)
                            );
                        }
                        assert_eq!(image[0], 0);
                        let handle = icon.get();
                        drop(icon);
                        assert_released(handle);
                    }
                }
            }
        }
        assert!(TrayIcon::new(16, DeviceKind::Mouse, BatteryVisual::Percent(101)).is_err());
        assert!(TrayIcon::new(0, DeviceKind::Mouse, BatteryVisual::Unknown).is_err());
    }
    #[test]
    fn actual_window_symbols_use_filled_v10_layers_and_release() {
        for appearance in [
            Appearance::Light,
            Appearance::Dark,
            Appearance::HighContrast {
                foreground: 0x0033ccff,
                face: 0x00552211,
            },
        ] {
            let (rim, fill, _) = appearance.colors();
            for n in [28, 35, 42, 56] {
                let mut distinct = std::collections::HashSet::new();
                for kind in KINDS {
                    let icon = SymbolIcon::with_appearance(n, kind, appearance).unwrap();
                    let image = read_icon(icon.get(), n);
                    assert_eq!(image, symbol_pixels(n as usize, kind, appearance));
                    assert!(
                        image.contains(&premultiplied(rim, 255)),
                        "missing rim {kind:?}/{n}"
                    );
                    assert!(
                        image.contains(&premultiplied(fill, 255)),
                        "missing filled silhouette {kind:?}/{n}"
                    );
                    assert!(
                        image.iter().any(|pixel| (1..255).contains(&(pixel >> 24))),
                        "missing antialiasing {kind:?}/{n}"
                    );
                    // Every edge is transparent: these standalone symbols have no tray gauge.
                    let size = n as usize;
                    for edge in 0..size {
                        for index in [
                            edge,
                            (size - 1) * size + edge,
                            edge * size,
                            edge * size + size - 1,
                        ] {
                            assert_eq!(image[index], 0);
                        }
                    }
                    for pixel in &image {
                        let alpha = pixel >> 24;
                        assert!(
                            (pixel & 255) <= alpha
                                && ((pixel >> 8) & 255) <= alpha
                                && ((pixel >> 16) & 255) <= alpha
                        );
                    }
                    assert!(
                        distinct.insert(image),
                        "device types share a fallback silhouette: {kind:?}"
                    );
                    let handle = icon.get();
                    drop(icon);
                    assert_released(handle);
                }
            }
        }
    }
}
