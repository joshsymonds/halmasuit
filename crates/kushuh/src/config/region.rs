//! [`Region`] — a rectangle in monitor-percentage space.

use thiserror::Error;

/// A rectangle on a monitor, expressed as integer percentages of that
/// monitor's dimensions.
///
/// `x`/`y` are the top-left corner; `width`/`height` extend right/down.
/// All four are percentages in `0..=100`, and the rectangle must fit on
/// the monitor (`x + width <= 100`, `y + height <= 100`) with non-zero
/// area. Construct via [`Region::new`], which enforces those invariants;
/// the fields are private so a `Region` value is always valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    x: u8,
    y: u8,
    width: u8,
    height: u8,
}

/// Why a set of percentages does not describe a valid [`Region`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RegionError {
    /// `width` was zero — a region must have non-zero area.
    #[error("region width must be non-zero")]
    ZeroWidth,
    /// `height` was zero — a region must have non-zero area.
    #[error("region height must be non-zero")]
    ZeroHeight,
    /// `x + width` exceeded 100% — the region runs off the right edge.
    #[error("region runs off the right edge: x ({x}) + width ({width}) > 100")]
    HorizontalOverflow {
        /// The offending left edge.
        x: u8,
        /// The offending width.
        width: u8,
    },
    /// `y + height` exceeded 100% — the region runs off the bottom edge.
    #[error("region runs off the bottom edge: y ({y}) + height ({height}) > 100")]
    VerticalOverflow {
        /// The offending top edge.
        y: u8,
        /// The offending height.
        height: u8,
    },
}

impl Region {
    /// Construct a [`Region`] from monitor percentages, validating that
    /// it has non-zero area and fits within the monitor.
    ///
    /// # Errors
    ///
    /// Returns a [`RegionError`] if `width` or `height` is zero, or if the
    /// region extends past the monitor's right (`x + width > 100`) or
    /// bottom (`y + height > 100`) edge.
    pub fn new(x: u8, y: u8, width: u8, height: u8) -> Result<Self, RegionError> {
        if width == 0 {
            return Err(RegionError::ZeroWidth);
        }
        if height == 0 {
            return Err(RegionError::ZeroHeight);
        }
        // `u16::from` keeps the bounds check overflow-proof without an
        // `as` cast (workspace idiom); u8 + u8 maxes at 510 < u16::MAX.
        if u16::from(x) + u16::from(width) > 100 {
            return Err(RegionError::HorizontalOverflow { x, width });
        }
        if u16::from(y) + u16::from(height) > 100 {
            return Err(RegionError::VerticalOverflow { y, height });
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// The left edge, as a percentage of monitor width.
    #[must_use]
    pub const fn x(self) -> u8 {
        self.x
    }

    /// The top edge, as a percentage of monitor height.
    #[must_use]
    pub const fn y(self) -> u8 {
        self.y
    }

    /// The width, as a percentage of monitor width.
    #[must_use]
    pub const fn width(self) -> u8 {
        self.width
    }

    /// The height, as a percentage of monitor height.
    #[must_use]
    pub const fn height(self) -> u8 {
        self.height
    }
}

#[cfg(test)]
mod tests {
    use super::{Region, RegionError};

    #[test]
    fn full_monitor_is_valid() {
        let region = Region::new(0, 0, 100, 100).expect("full monitor is valid");
        assert_eq!(region.x(), 0);
        assert_eq!(region.y(), 0);
        assert_eq!(region.width(), 100);
        assert_eq!(region.height(), 100);
    }

    #[test]
    fn right_split_is_valid() {
        let region = Region::new(60, 0, 40, 100).expect("60..100 right split is valid");
        assert_eq!(region.x(), 60);
        assert_eq!(region.width(), 40);
    }

    #[test]
    fn left_split_is_valid() {
        Region::new(0, 0, 60, 100).expect("0..60 left split is valid");
    }

    #[test]
    fn bottom_strip_is_valid() {
        // The doc's `scratch` role: region 0 80 100 20.
        Region::new(0, 80, 100, 20).expect("bottom strip is valid");
    }

    #[test]
    fn max_corner_is_valid() {
        // A 1% square anchored at the far corner: edges land exactly on 100.
        Region::new(99, 99, 1, 1).expect("1% square at the far corner is valid");
    }

    #[test]
    fn zero_width_is_rejected() {
        assert_eq!(Region::new(0, 0, 0, 100), Err(RegionError::ZeroWidth));
    }

    #[test]
    fn zero_height_is_rejected() {
        assert_eq!(Region::new(0, 0, 100, 0), Err(RegionError::ZeroHeight));
    }

    #[test]
    fn horizontal_overflow_is_rejected() {
        // 61 + 40 = 101 > 100.
        assert_eq!(
            Region::new(61, 0, 40, 100),
            Err(RegionError::HorizontalOverflow { x: 61, width: 40 }),
        );
    }

    #[test]
    fn vertical_overflow_is_rejected() {
        // 81 + 20 = 101 > 100.
        assert_eq!(
            Region::new(0, 81, 100, 20),
            Err(RegionError::VerticalOverflow { y: 81, height: 20 }),
        );
    }

    #[test]
    fn zero_width_is_checked_before_overflow() {
        // A zero-width region that would also overflow reports ZeroWidth:
        // area is checked first so the message names the more basic fault.
        assert_eq!(Region::new(200, 0, 0, 100), Err(RegionError::ZeroWidth));
    }
}
