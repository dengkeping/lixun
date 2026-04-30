use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use tempfile::NamedTempFile;

pub struct TempFile(NamedTempFile);

impl TempFile {
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

pub fn prepare_batch(paths: &[PathBuf]) -> Result<(Vec<PathBuf>, Vec<TempFile>)> {
    let mut out_paths = Vec::with_capacity(paths.len());
    let mut temp_files = Vec::new();
    let mut skipped = 0usize;
    
    for path in paths {
        let ext = path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        
        match ext.as_str() {
            #[cfg(feature = "heic")]
            "heic" | "heif" => {
                match decode_heic_to_temp(path) {
                    Ok(temp) => {
                        out_paths.push(temp.path().to_owned());
                        temp_files.push(temp);
                    }
                    Err(e) => {
                        tracing::warn!("HEIC decode failed, skipping {}: {:#}", path.display(), e);
                        skipped += 1;
                    }
                }
            }
            
            #[cfg(feature = "jxl")]
            "jxl" => {
                match decode_jxl_to_temp(path) {
                    Ok(temp) => {
                        out_paths.push(temp.path().to_owned());
                        temp_files.push(temp);
                    }
                    Err(e) => {
                        tracing::warn!("JXL decode failed, skipping {}: {:#}", path.display(), e);
                        skipped += 1;
                    }
                }
            }
            
            #[cfg(feature = "raw")]
            "cr2" | "cr3" | "nef" | "nrw" | "arw" | "srf" | "sr2" | 
            "dng" | "raf" | "orf" | "rw2" | "pef" => {
                match decode_raw_to_temp(path) {
                    Ok(temp) => {
                        out_paths.push(temp.path().to_owned());
                        temp_files.push(temp);
                    }
                    Err(e) => {
                        tracing::warn!("RAW decode failed, skipping {}: {:#}", path.display(), e);
                        skipped += 1;
                    }
                }
            }
            
            _ => {
                out_paths.push(path.clone());
            }
        }
    }
    
    if skipped > 0 {
        tracing::info!("Image decode: skipped {} unsupported/corrupt files", skipped);
    }
    
    Ok((out_paths, temp_files))
}

#[cfg(feature = "heic")]
fn decode_heic_to_temp(heic_path: &Path) -> Result<TempFile> {
    use libheif_rs::integration::image::register_all_decoding_hooks;
    use image::ImageReader;
    
    register_all_decoding_hooks();
    
    let img = ImageReader::open(heic_path)
        .context("Failed to open HEIC")?
        .decode()
        .context("Failed to decode HEIC")?;
    
    let mut temp = tempfile::Builder::new()
        .prefix("lixun-heic-")
        .suffix(".jpg")
        .tempfile()
        .context("Failed to create temp file")?;
    
    img.write_to(&mut temp, image::ImageFormat::Jpeg)
        .context("Failed to write JPEG")?;
    
    Ok(TempFile(temp))
}

#[cfg(feature = "jxl")]
fn decode_jxl_to_temp(jxl_path: &Path) -> Result<TempFile> {
    use image::DynamicImage;
    use jxl_oxide::integration::JxlDecoder;
    
    let file = std::fs::File::open(jxl_path)
        .context("Failed to open JXL")?;
    
    let decoder = JxlDecoder::new(file)
        .context("Failed to create JXL decoder")?;
    
    let img = DynamicImage::from_decoder(decoder)
        .context("Failed to decode JXL")?;
    
    let mut temp = tempfile::Builder::new()
        .prefix("lixun-jxl-")
        .suffix(".jpg")
        .tempfile()
        .context("Failed to create temp file")?;
    
    img.write_to(&mut temp, image::ImageFormat::Jpeg)
        .context("Failed to write JPEG")?;
    
    Ok(TempFile(temp))
}

#[cfg(feature = "raw")]
fn decode_raw_to_temp(raw_path: &Path) -> Result<TempFile> {
    use image::{RgbImage, DynamicImage};
    
    let _raw_image = rawler::decode_file(raw_path)
        .context("Failed to decode RAW")?;
    
    let srgb = imagepipe::simple_decode_8bit(raw_path, 0, 0)
        .map_err(|e| anyhow::anyhow!("Failed to process RAW: {}", e))?;
    
    let rgb = RgbImage::from_raw(
        srgb.width as u32,
        srgb.height as u32,
        srgb.data
    ).context("Invalid RAW dimensions")?;
    
    let img = DynamicImage::ImageRgb8(rgb);
    
    let mut temp = tempfile::Builder::new()
        .prefix("lixun-raw-")
        .suffix(".jpg")
        .tempfile()
        .context("Failed to create temp file")?;
    
    img.write_to(&mut temp, image::ImageFormat::Jpeg)
        .context("Failed to write JPEG")?;
    
    Ok(TempFile(temp))
}

pub fn decode_to_dynamic_image(path: &Path) -> Result<image::DynamicImage> {
    let ext = path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    
    match ext.as_str() {
        #[cfg(feature = "heic")]
        "heic" | "heif" => decode_heic_direct(path),
        
        #[cfg(feature = "jxl")]
        "jxl" => decode_jxl_direct(path),
        
        #[cfg(feature = "raw")]
        "cr2" | "cr3" | "nef" | "nrw" | "arw" | "srf" | "sr2" | 
        "dng" | "raf" | "orf" | "rw2" | "pef" => decode_raw_direct(path),
        
        _ => image::open(path).context("Failed to open image"),
    }
}

#[cfg(feature = "heic")]
fn decode_heic_direct(heic_path: &Path) -> Result<image::DynamicImage> {
    use libheif_rs::integration::image::register_all_decoding_hooks;
    use image::ImageReader;
    
    register_all_decoding_hooks();
    
    ImageReader::open(heic_path)
        .context("Failed to open HEIC")?
        .decode()
        .context("Failed to decode HEIC")
}

#[cfg(feature = "jxl")]
fn decode_jxl_direct(jxl_path: &Path) -> Result<image::DynamicImage> {
    use image::DynamicImage;
    use jxl_oxide::integration::JxlDecoder;
    
    let file = std::fs::File::open(jxl_path)
        .context("Failed to open JXL")?;
    
    let decoder = JxlDecoder::new(file)
        .context("Failed to create JXL decoder")?;
    
    DynamicImage::from_decoder(decoder)
        .context("Failed to decode JXL")
}

#[cfg(feature = "raw")]
fn decode_raw_direct(raw_path: &Path) -> Result<image::DynamicImage> {
    use image::{RgbImage, DynamicImage};
    
    let _raw_image = rawler::decode_file(raw_path)
        .context("Failed to decode RAW")?;
    
    let srgb = imagepipe::simple_decode_8bit(raw_path, 0, 0)
        .map_err(|e| anyhow::anyhow!("Failed to process RAW: {}", e))?;
    
    let rgb = RgbImage::from_raw(
        srgb.width as u32,
        srgb.height as u32,
        srgb.data
    ).context("Invalid RAW dimensions")?;
    
    Ok(DynamicImage::ImageRgb8(rgb))
}
