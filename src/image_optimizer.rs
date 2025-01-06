use crate::disk_cache::{DiskCache, DiskCacheError};
use image::{imageops::FilterType, DynamicImage};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OptimizeImageError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unknown error")]
    Unknown,
    #[error("Unsupported image format: {0}")]
    UnsupportedImageFormat(&'static str),
    #[error("Cache error: {0}")]
    Cache(#[from] DiskCacheError),
    #[error("Image decoding error: {0}")]
    ImageDecoding(#[from] image::ImageError),
    #[error("OxiPNG error: {0}")]
    OxiPNG(#[from] oxipng::PngError),
}

#[derive(serde::Serialize)]
pub struct ImageOptimizationSettings {
    pub key: String,
    pub is_grayscale: bool,
    pub max_width: Option<u32>,
}

fn load_and_transform(
    image: &[u8],
    settings: &ImageOptimizationSettings,
) -> Result<DynamicImage, OptimizeImageError> {
    let mut decoded = image::load_from_memory(image)?;

    if let Some(max_width) = settings.max_width {
        if decoded.width() > max_width {
            let scale = max_width as f32 / decoded.width() as f32;
            let new_width = max_width;
            let new_height = (decoded.height() as f32 * scale) as u32;

            decoded =
                image::imageops::resize(&decoded, new_width, new_height, FilterType::Lanczos3)
                    .into();
        }
    }

    if settings.is_grayscale {
        Ok(decoded.to_luma8().into())
    } else {
        Ok(decoded.to_rgba8().into())
    }
}

pub fn optimize_image(
    image: &[u8],
    settings: ImageOptimizationSettings,
    cache: DiskCache<ImageOptimizationSettings, Vec<u8>>,
) -> Result<Vec<u8>, OptimizeImageError> {
    if let Some(cached) = cache.get(&settings)? {
        return Ok(cached);
    }

    let mimetype = infer::get(image);

    let result = match mimetype.map(|m| m.mime_type()).unwrap_or_default() {
        "image/png" => {
            let mapped = load_and_transform(image, &settings)?;
            let color_type = if settings.is_grayscale {
                oxipng::ColorType::Grayscale {
                    transparent_shade: None,
                }
            } else {
                oxipng::ColorType::RGBA
            };

            let oxi_img = oxipng::RawImage::new(
                mapped.width(),
                mapped.height(),
                color_type,
                oxipng::BitDepth::Eight,
                mapped.into_bytes(),
            )?;
            Ok(oxi_img.create_optimized_png(&oxipng::Options::from_preset(2))?)
        }
        "image/jpeg" => {
            let mapped = load_and_transform(image, &settings)?;

            match std::panic::catch_unwind(|| -> std::io::Result<Vec<u8>> {
                let mut comp = mozjpeg::Compress::new(if settings.is_grayscale {
                    mozjpeg::ColorSpace::JCS_GRAYSCALE
                } else {
                    mozjpeg::ColorSpace::JCS_RGB
                });

                comp.set_size(mapped.width() as usize, mapped.height() as usize);
                comp.set_quality(80.0);
                let mut comp = comp.start_compress(Vec::new())?; // any io::Write will work

                let mapped = if settings.is_grayscale {
                    mapped.to_luma8().into_raw()
                } else {
                    mapped.to_rgb8().into_raw()
                };

                comp.write_scanlines(&mapped)?;

                let writer = comp.finish()?;
                Ok(writer)
            }) {
                Ok(Ok(writer)) => Ok(writer),
                Ok(Err(e)) => Err(OptimizeImageError::Io(e)),
                Err(_) => Err(OptimizeImageError::Unknown),
            }
        }
        _ => Err(OptimizeImageError::UnsupportedImageFormat(
            mimetype.map(|m| m.mime_type()).unwrap_or_default(),
        )),
    };

    if let Ok(ref result) = result {
        cache.set(&settings, result, 1000000000)?; // assume that key will change if the image changes
    }

    result
}
