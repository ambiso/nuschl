// SPDX-License-Identifier: EUPL-1.2
//! Native capture for the isolated macOS video worker.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::run;

#[cfg(any(target_os = "macos", test))]
mod layout {
    pub const MAX_WIDTH: usize = 3840;
    pub const MAX_HEIGHT: usize = 2160;

    /// The part of a packed BGRA plane that contains image data.
    #[derive(Debug, Eq, PartialEq)]
    pub struct Plane {
        pub row_bytes: usize,
        pub span: usize,
    }

    impl Plane {
        pub fn new(width: usize, height: usize, stride: usize, bytes: usize) -> Option<Self> {
            if !(16..=MAX_WIDTH).contains(&width) || !(16..=MAX_HEIGHT).contains(&height) {
                return None;
            }
            let row_bytes = width * 4;
            if !(row_bytes..=MAX_WIDTH * 4 + 4096).contains(&stride) {
                return None;
            }
            let span = stride * (height - 1) + row_bytes;
            (span <= bytes).then_some(Self { row_bytes, span })
        }
    }

    pub fn fit_screen(width: f64, height: f64, scale: f64) -> Option<(usize, usize)> {
        if scale <= 0.0 {
            return None;
        }
        let (width, height) = (width * scale, height * scale);
        if !width.is_finite() || !height.is_finite() || width.min(height) < 16.0 {
            return None;
        }
        let shrink = (3840.0 / width).min(2160.0 / height).min(1.0);
        let (width, height) = (width * shrink, height * shrink);
        if width.min(height) < 16.0 {
            return None;
        }
        Some((width as usize & !1, height as usize & !1))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn requires_complete_bgra_rows() {
            assert_eq!(
                Plane::new(32, 16, 256, 3968),
                Some(Plane {
                    row_bytes: 128,
                    span: 3968,
                }),
            );
            assert_eq!(Plane::new(32, 16, 256, 3967), None);
            assert_eq!(Plane::new(32, 16, 127, usize::MAX), None);
            assert_eq!(Plane::new(32, 16, usize::MAX, usize::MAX), None);
            assert_eq!(Plane::new(4096, 2160, 16384, usize::MAX), None);
        }

        #[test]
        fn fits_raster_pixels_without_upscaling() {
            assert_eq!(fit_screen(1720.0, 720.0, 2.0), Some((3440, 1440)));
            assert_eq!(fit_screen(2560.0, 1440.0, 2.0), Some((3840, 2160)));
            assert_eq!(fit_screen(641.0, 481.0, 1.0), Some((640, 480)));
            assert_eq!(fit_screen(f64::NAN, 481.0, 1.0), None);
        }
    }
}
