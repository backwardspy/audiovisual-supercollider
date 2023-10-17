#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::expect_used
)]
use std::{
    collections::HashSet,
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};

use clap::Parser;
use color_eyre::{eyre::eyre, Result};

const PADDING: u32 = 4;

#[derive(Parser)]
struct Args {
    image_path: PathBuf,
    audio_path: PathBuf,
}

fn main() -> Result<()> {
    color_eyre::config::HookBuilder::new()
        .display_env_section(false)
        .install()?;

    let args = Args::parse();

    let source_image = image::io::Reader::open(args.image_path)?
        .decode()?
        .into_rgb8();

    // this song and dance eventually collapses every colour in the source
    // image into an array of four unique colours. if that fails, this image
    // won't work so we just reject it.
    let palette: [[u8; 3]; 4] = source_image
        .pixels()
        .map(|px| px.0)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| eyre!("image must use exactly 4 colours"))?;

    let (width, height) = source_image.dimensions();
    if width % PADDING != 0 {
        return Err(eyre!(
            "image width must be a multiple of 4. try removing {} pixels from the vertical edge(s)",
            width % PADDING
        ));
    }

    let masks = create_masks(width, height, &palette, &source_image)?;

    fs::create_dir_all("output")?;

    for (i, mask) in masks.iter().enumerate() {
        save_mask(mask, i, width, height)?;
    }

    // reload the masks from disk so we get the bytes in the correct order for the output format.
    let masks = [
        read_bitmap_indices("output/mask-00.bmp")?,
        read_bitmap_indices("output/mask-01.bmp")?,
        read_bitmap_indices("output/mask-10.bmp")?,
        read_bitmap_indices("output/mask-11.bmp")?,
    ];

    // filling the buffer this way ensures that we have enough bytes to fill the entire image.
    let audio_bytes = {
        let mut buf = vec![0u8; masks[0].len()];
        let file_bytes = fs::read(args.audio_path)?;

        if file_bytes.len() > buf.len() {
            return Err(eyre!(
                "audio file is too large ({} bytes). ensure audio has no more than {} samples/bytes",
                file_bytes.len(),
                buf.len()
            ));
        }

        buf[..file_bytes.len()].copy_from_slice(&file_bytes);
        buf
    };
    let painted_audio_bytes = paint_audio_bytes(&audio_bytes, &masks)?;

    // now for the real hack; use `image` to create the bitmap in the format we
    // need, then use `tinybmp` to find the right spot in the file to overwrite
    // the palette indices.
    // let palette = [[32, 32, 32], [220, 110, 0], [0, 220, 110], [110, 0, 220]];
    let path = Path::new("output/final.bmp");
    save_painted_bitmap(&painted_audio_bytes, palette, path, width, height)?;

    println!("done! check {}", path.display());
    Ok(())
}

fn save_painted_bitmap(
    padded_audio_bytes: &[u8],
    palette: [[u8; 3]; 4],
    path: &Path,
    width: u32,
    height: u32,
) -> Result<(), color_eyre::eyre::Error> {
    // we're working at 8bpp so we need 256 colours in the palette.
    // we have four colours, so we just loop them in the palette.
    // the painting process ensures that these looped palette indices end up correct.
    let size = (width * height) as usize;
    let palette: Vec<[u8; 3]> = (0..256).map(|i| palette[i % 4]).collect();

    // save & reload an empty bitmap to give us a valid header to work with.
    save_empty_bitmap(path, size, width, height, &palette)?;
    let mut bitmap_bytes = fs::read(path)?;
    let bitmap = tinybmp::RawBmp::from_slice(&bitmap_bytes).map_err(|e| eyre!("{e:?}"))?;

    let index_offset = bitmap.header().image_data_start;

    // sanity checks - make sure we've not messed anything up to this point.
    assert_eq!(
        padded_audio_bytes.len(),
        bitmap.header().image_data_len as usize
    );
    assert_eq!(bitmap_bytes.len() - index_offset, padded_audio_bytes.len());

    // overwrite the palette indices and save over the file.
    bitmap_bytes[index_offset..].copy_from_slice(padded_audio_bytes);
    fs::write(path, bitmap_bytes)?;

    Ok(())
}

fn read_bitmap_indices(path: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    let bitmap = tinybmp::RawBmp::from_slice(&bytes).map_err(|e| eyre!("{e:?}"))?;
    Ok(bytes[bitmap.header().image_data_start..].to_vec())
}

fn save_empty_bitmap(
    path: &Path,
    size: usize,
    width: u32,
    height: u32,
    palette: &[[u8; 3]],
) -> Result<(), color_eyre::eyre::Error> {
    let mut writer = BufWriter::new(File::create(path)?);
    let mut encoder = image::codecs::bmp::BmpEncoder::new(&mut writer);
    encoder.encode_with_palette(
        &vec![0u8; size],
        width,
        height,
        image::ColorType::L8,
        Some(palette),
    )?;
    Ok(())
}

fn paint_audio_bytes(audio_bytes: &[u8], masks: &[Vec<u8>; 4]) -> Result<Vec<u8>> {
    audio_bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let mask_idx = masks
                .iter()
                .position(|mask| mask[i] > 0)
                .ok_or_else(|| eyre!("no mask covers byte at index {i}"))?;

            #[allow(clippy::cast_possible_truncation)]
            Ok((b & 0xFC) | mask_idx as u8)
        })
        .collect()
}

fn create_masks(
    width: u32,
    height: u32,
    colours: &[[u8; 3]],
    source_image: &image::RgbImage,
) -> Result<[Vec<u8>; 4]> {
    let size = (width * height) as usize;
    let mut masks = [
        vec![0u8; size],
        vec![0u8; size],
        vec![0u8; size],
        vec![0u8; size],
    ];
    for y in 0..height {
        for x in 0..width {
            let pixel_idx = (y * width + x) as usize;
            let palette_idx = colours
                .iter()
                .position(|c| *c == source_image.get_pixel(x, y).0)
                .ok_or_else(|| eyre!("off-palette colour in source image at ({x}, {y})"))?;
            masks[palette_idx][pixel_idx] = 1;
        }
    }
    Ok(masks)
}

fn save_mask(mask: &[u8], i: usize, width: u32, height: u32) -> Result<()> {
    let mut writer = BufWriter::new(File::create(format!("output/mask-{i:02b}.bmp"))?);
    let mut encoder = image::codecs::bmp::BmpEncoder::new(&mut writer);
    encoder.encode_with_palette(
        mask,
        width,
        height,
        image::ColorType::L8,
        Some(&[[0, 0, 0], [255, 255, 255]]),
    )?;
    Ok(())
}
