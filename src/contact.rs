use crate::{export::Frame, process::Cancellation};
use anyhow::{Context, Result};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder, imageops};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

const COLUMNS: u32 = 5;
const ROWS: usize = 4;
const THUMB_WIDTH: u32 = 320;
const THUMB_HEIGHT: u32 = 180;
const CAPTION: u32 = 32;
const HEADER: u32 = 56;
const MARGIN: u32 = 16;
const BACKGROUND: Rgb<u8> = Rgb([22, 24, 28]);
const FOREGROUND: Rgb<u8> = Rgb([232, 234, 238]);

pub fn create(
    root: &Path,
    title: &str,
    frames: &[Frame],
    cancel: &Cancellation,
) -> Result<Vec<String>> {
    let per_page = COLUMNS as usize * ROWS;
    let pages = frames.len().div_ceil(per_page);
    let mut filenames = Vec::with_capacity(pages);
    for (page_index, page) in frames.chunks(per_page).enumerate() {
        cancel.check()?;
        let columns = COLUMNS.min(page.len() as u32);
        let rows = page.len().div_ceil(COLUMNS as usize) as u32;
        let width = columns * THUMB_WIDTH + MARGIN * 2;
        let height = HEADER + rows * (THUMB_HEIGHT + CAPTION) + MARGIN;
        let mut sheet = RgbImage::from_pixel(width, height, BACKGROUND);
        let heading = format!("{} | {}/{}", title, page_index + 1, pages);
        draw_text(&mut sheet, MARGIN, 12, &heading, 2);
        for (index, frame) in page.iter().enumerate() {
            cancel.check()?;
            let image = image::open(root.join(&frame.file))
                .with_context(|| format!("Cannot read {} for the contact sheet", frame.file))?;
            let thumb = image.thumbnail(THUMB_WIDTH - 8, THUMB_HEIGHT - 8).to_rgb8();
            let x = MARGIN + (index as u32 % COLUMNS) * THUMB_WIDTH;
            let y = HEADER + (index as u32 / COLUMNS) * (THUMB_HEIGHT + CAPTION);
            imageops::replace(
                &mut sheet,
                &thumb,
                i64::from(x + (THUMB_WIDTH - thumb.width()) / 2),
                i64::from(y + (THUMB_HEIGHT - thumb.height()) / 2),
            );
            draw_text(
                &mut sheet,
                x + 8,
                y + THUMB_HEIGHT + 6,
                &format!("{:04}  {}", frame.index, frame.timestamp),
                2,
            );
        }
        let filename = format!("contact-sheet-{:03}.jpg", page_index + 1);
        let mut output = BufWriter::new(File::create(root.join(&filename))?);
        JpegEncoder::new_with_quality(&mut output, 90).encode_image(&sheet)?;
        output.flush()?;
        filenames.push(filename);
    }
    Ok(filenames)
}

fn draw_text(image: &mut RgbImage, start_x: u32, y: u32, text: &str, scale: u32) {
    for (index, character) in text.chars().enumerate() {
        let x = start_x + index as u32 * 8 * scale;
        if x + 8 * scale > image.width() - MARGIN {
            break;
        }
        let Some(glyph) = BASIC_FONTS.get(character).or_else(|| BASIC_FONTS.get('?')) else {
            continue;
        };
        for (row, bits) in glyph.into_iter().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let py = y + row as u32 * scale + dy;
                        if py < image.height() {
                            image.put_pixel(x + column * scale + dx, py, FOREGROUND);
                        }
                    }
                }
            }
        }
    }
}
