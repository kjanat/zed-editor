use anyhow::Context as _;
use uuid::Uuid;
use x11rb::{
    connection::Connection as _, protocol::xproto::ConnectionExt as _, xcb_ffi::XCBConnection,
};

use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, Size, px};

#[derive(Debug)]
pub(crate) struct X11Display {
    x_screen_index: usize,
    bounds: Bounds<Pixels>,
    uuid: Uuid,
}

impl X11Display {
    pub(crate) fn new(
        xcb: &XCBConnection,
        scale_factor: f32,
        x_screen_index: usize,
    ) -> anyhow::Result<Self> {
        let screen = xcb
            .setup()
            .roots
            .get(x_screen_index)
            .with_context(|| format!("No screen found with index {x_screen_index}"))?;
        let root_geometry = xcb
            .get_geometry(screen.root)
            .context("Failed to request the root window geometry")?
            .reply()
            .context("Failed to get the root window geometry")?;
        Ok(Self {
            x_screen_index,
            bounds: screen_bounds(root_geometry.width, root_geometry.height, scale_factor),
            uuid: Uuid::from_bytes([0; 16]),
        })
    }
}

fn screen_bounds(width_in_pixels: u16, height_in_pixels: u16, scale_factor: f32) -> Bounds<Pixels> {
    Bounds {
        origin: Default::default(),
        size: Size {
            width: px(width_in_pixels as f32 / scale_factor),
            height: px(height_in_pixels as f32 / scale_factor),
        },
    }
}

impl PlatformDisplay for X11Display {
    // An X screen spans every monitor, so it has no single refresh rate.
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        None
    }

    fn id(&self) -> DisplayId {
        DisplayId::new(self.x_screen_index as u64)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}

#[cfg(test)]
mod tests {
    use super::screen_bounds;
    use gpui::{Bounds, point, px, size};

    #[test]
    fn test_screen_bounds_scale_the_root_size() {
        assert_eq!(
            screen_bounds(3840, 2160, 2.0),
            Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)))
        );
        assert_eq!(
            screen_bounds(1080, 1920, 1.0),
            Bounds::new(point(px(0.), px(0.)), size(px(1080.), px(1920.)))
        );
    }
}
