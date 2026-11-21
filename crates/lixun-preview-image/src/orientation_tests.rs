use super::*;
use image::{ImageEncoder, Rgb, RgbImage, codecs::jpeg::JpegEncoder};

fn write_jpeg(path: &Path, width: u32, height: u32, orientation: Option<u8>) {
    // Asymmetric quadrants make rotations and reflections distinguishable.
    let image = RgbImage::from_fn(width, height, |x, y| {
        let level = match (x < width / 2, y < height / 2) {
            (true, true) => 32,
            (false, true) => 96,
            (true, false) => 160,
            (false, false) => 224,
        };
        Rgb([level; 3])
    });
    let mut encoder = JpegEncoder::new_with_quality(std::fs::File::create(path).unwrap(), 95);
    if let Some(tag) = orientation {
        // Little-endian TIFF header, one IFD0 SHORT entry (Orientation, 0x0112).
        let exif = [
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 1, 3, 0, 1, 0, 0, 0, tag, 0, 0, 0, 0, 0, 0,
            0,
        ];
        encoder.set_exif_metadata(exif.to_vec()).unwrap();
    }
    encoder.encode_image(&image).unwrap();
}

#[test]
fn preview_respects_camera_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.jpg");
    let corners = [
        [0, 1, 2, 3],
        [1, 0, 3, 2],
        [3, 2, 1, 0],
        [2, 3, 0, 1],
        [0, 2, 1, 3],
        [2, 0, 3, 1],
        [3, 1, 2, 0],
        [1, 3, 0, 2],
    ];
    for tag in [6, 8, 2, 3, 4, 5, 7, 1, 0] {
        write_jpeg(&path, 48, 32, (tag != 0).then_some(tag));
        let (texture, intrinsic) = decode_scaled(&path).unwrap();
        let expected = if tag >= 5 { (32, 48) } else { (48, 32) };
        assert_eq!(intrinsic, expected, "EXIF {tag} footer");
        assert_eq!(
            (texture.width(), texture.height()),
            expected,
            "EXIF {tag} texture"
        );
        let pixels = image::load_from_memory(&texture.save_to_png_bytes())
            .unwrap()
            .to_luma8();
        let (w, h) = pixels.dimensions();
        let order = [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ]
        .map(|(x, y)| pixels.get_pixel(x, y)[0] / 64);
        assert_eq!(
            order,
            corners[usize::from(tag.max(1) - 1)],
            "EXIF {tag} pixels"
        );
    }

    write_jpeg(&path, 3200, 800, Some(6));
    let (texture, intrinsic) = decode_scaled(&path).unwrap();
    assert_eq!(intrinsic, (800, 3200));
    assert_eq!((texture.width(), texture.height()), (640, 2560));
}
