//! The application icon, drawn from `assets/anything.svg` at whatever sizes
//! the tray and the window manager ask for.

use resvg::{tiny_skia, usvg};

pub const SVG: &[u8] = include_bytes!("../assets/anything.svg");

/// Sizes offered to the tray and the window manager.
pub const SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

/// Straight (not premultiplied) RGBA pixels, row by row.
pub fn rgba(size: u32) -> Vec<[u8; 4]> {
    let tree = usvg::Tree::from_data(SVG, &usvg::Options::default()).expect("bundled icon is valid");
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("icon size is not zero");
    let scale = size as f32 / tree.size().width();
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap
        .pixels()
        .iter()
        .map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// ARGB in network byte order, as StatusNotifierItem wants it.
pub fn argb_bytes(size: u32) -> Vec<u8> {
    rgba(size).into_iter().flat_map(|[r, g, b, a]| [a, r, g, b]).collect()
}

/// `_NET_WM_ICON` data: width, height, then one ARGB value per pixel, for
/// every size.
pub fn net_wm_icon() -> Vec<u32> {
    let mut data = Vec::new();
    for size in SIZES {
        data.extend([size, size]);
        data.extend(
            rgba(size)
                .into_iter()
                .map(|[r, g, b, a]| u32::from_be_bytes([a, r, g, b])),
        );
    }
    data
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders() {
        let pixels = super::rgba(32);
        assert_eq!(pixels.len(), 32 * 32);
        // Corners are transparent, the lens centre is opaque and light.
        assert_eq!(pixels[0][3], 0);
        let centre = pixels[13 * 32 + 13];
        assert_eq!(centre[3], 255);
        assert!(centre[0] > 200);
        let header = super::net_wm_icon();
        assert_eq!(&header[..2], &[16, 16]);
    }
}
