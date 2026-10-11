//! Renders SVG icon drafts at tray and app sizes on light and dark panels.
//! cargo run -p anything-gui --example icon_preview -- out.png crates/anything-gui/assets/anything.svg

use resvg::{tiny_skia, usvg};

fn render(svg: &[u8], size: u32) -> tiny_skia::Pixmap {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).expect("valid svg");
    let mut pixmap = tiny_skia::Pixmap::new(size, size).unwrap();
    let scale = size as f32 / tree.size().width();
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, files) = args.split_first().expect("out.png and svg files");
    // Per draft: 256 px, then 48, 32, 22 on a dark and a light panel.
    let row_height = 280;
    let mut sheet = tiny_skia::Pixmap::new(720, row_height * files.len() as u32).unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(128, 128, 128, 255));
    for (row, file) in files.iter().enumerate() {
        let svg = std::fs::read(file).unwrap();
        let y = row as i32 * row_height as i32 + 12;
        let paint = tiny_skia::PixmapPaint::default();
        sheet.draw_pixmap(12, y, render(&svg, 256).as_ref(), &paint, tiny_skia::Transform::identity(), None);
        for (panel, color) in [(0, (32, 32, 36)), (1, (236, 236, 240))] {
            let px = 300 + panel * 210;
            let mut bg = tiny_skia::Pixmap::new(200, 256).unwrap();
            bg.fill(tiny_skia::Color::from_rgba8(color.0, color.1, color.2, 255));
            sheet.draw_pixmap(px, y, bg.as_ref(), &paint, tiny_skia::Transform::identity(), None);
            let mut offset = 10;
            for size in [48u32, 32, 22] {
                sheet.draw_pixmap(px + 20, y + offset, render(&svg, size).as_ref(), &paint, tiny_skia::Transform::identity(), None);
                offset += size as i32 + 30;
            }
        }
    }
    sheet.save_png(out).unwrap();
}
