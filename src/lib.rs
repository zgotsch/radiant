#![warn(missing_docs)]

//! # Radiant
//!
//! Load Radiance HDR (.hdr, .pic) images.
//!
//! This is a fork of [TechPriest's HdrLdr](https://crates.io/crates/hdrldr),
//! rewritten for slightly better performance. May or may not actually perform better.
//! I've restricted the API so that it only accepts readers that implement
//! `BufRead`.
//!
//! The original crate, which does not have this restriction, is in turn a slightly
//! rustified version of [C++ code by Igor
//! Kravtchenko](http://flipcode.com/archives/HDR_Image_Reader.shtml). If you need
//! more image formats besides HDR, take a look at [Image2
//! crate](https://crates.io/crates/image2).
//!
//! ## Example
//!
//! Add `radiant` to your dependencies of your `Cargo.toml`:
//! ```toml
//! [dependencies]
//! radiant = "0.2"
//! ```
//!
//! And then, in your rust file:
//! ```rust
//! use std::io::BufReader;
//! use std::fs::File;
//!
//! let f = File::open("assets/colorful_studio_2k.hdr").expect("Failed to open specified file");
//! let f = BufReader::new(f);
//! let image = radiant::load(f).expect("Failed to load image data");
//! ```
//!
//! For more complete example, see
//! [Simple HDR Viewer application](https://github.com/iwikal/radiant/blob/master/examples/view_hdr.rs)
//!
//! Huge thanks to [HDRI Haven](https://hdrihaven.com) for providing CC0 sample images for testing!

// Original source: http://flipcode.com/archives/HDR_Image_Reader.shtml
use std::io::{BufRead, Error as IoError, ErrorKind};

mod dim_parser;

/// The decoded R, G, and B value of a pixel. You typically get these from the data field on an
/// [`Image`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RGB {
    /// The red channel.
    pub r: f32,
    /// The green channel.
    pub g: f32,
    /// The blue channel.
    pub b: f32,
}

impl RGB {
    #[inline]
    fn apply_exposure(&mut self, expo: u8) {
        let expo = i32::from(expo) - 128;
        let d = 2_f32.powi(expo) / 255_f32;

        self.r *= d;
        self.g *= d;
        self.b *= d;
    }
}

#[derive(Debug, Clone)]
struct RGBE {
    r: u8,
    g: u8,
    b: u8,
    e: u8,
}

impl std::convert::From<RGBE> for RGB {
    #[inline]
    fn from(rgbe: RGBE) -> Self {
        let mut rgb = Self {
            r: rgbe.r as f32,
            g: rgbe.g as f32,
            b: rgbe.b as f32,
        };
        rgb.apply_exposure(rgbe.e);
        rgb
    }
}

impl std::convert::From<[u8; 4]> for RGBE {
    #[inline]
    fn from([r, g, b, e]: [u8; 4]) -> Self {
        Self { r, g, b, e }
    }
}

impl std::convert::From<RGBE> for [u8; 4] {
    #[inline]
    fn from(RGBE { r, g, b, e }: RGBE) -> Self {
        [r, g, b, e]
    }
}

impl RGBE {
    #[inline]
    fn is_rle_marker(&self) -> bool {
        self.r == 1 && self.g == 1 && self.b == 1
    }

    #[inline]
    fn is_new_decrunch_marker(&self) -> bool {
        self.r == 2 && self.g == 2 && self.b & 128 == 0
    }
}

/// The various types of errors that can occur while loading an [`Image`].
#[derive(thiserror::Error, Debug)]
pub enum LoadError {
    /// A lower level io error was raised.
    #[error("io error: {0}")]
    Io(#[source] IoError),
    /// The image file ended unexpectedly.
    #[error("file ended unexpectedly")]
    Eof(#[source] IoError),
    /// The file did not follow valid Radiance HDR format.
    #[error("invalid file format")]
    FileFormat,
    /// The image file contained invalid run-length encoding.
    #[error("invalid run-length encoding")]
    Rle,
}

impl From<IoError> for LoadError {
    fn from(error: IoError) -> Self {
        match error.kind() {
            ErrorKind::UnexpectedEof => Self::Eof(error),
            _ => Self::Io(error),
        }
    }
}

/// An alias for the type of results this crate returns.
pub type LoadResult<T = ()> = Result<T, LoadError>;

trait ReadExt {
    fn read_byte(&mut self) -> std::io::Result<u8>;
    fn read_rgbe(&mut self) -> std::io::Result<RGBE>;
}

impl<R: BufRead> ReadExt for R {
    #[inline]
    fn read_byte(&mut self) -> std::io::Result<u8> {
        let mut buf = [0u8];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    #[inline]
    fn read_rgbe(&mut self) -> std::io::Result<RGBE> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(buf.into())
    }
}

fn old_decrunch<R: BufRead>(mut reader: R, mut scanline: &mut [RGB]) -> LoadResult {
    let mut l_shift = 0;

    while scanline.len() > 1 {
        let rgbe = reader.read_rgbe()?;
        if rgbe.is_rle_marker() {
            let count = usize::checked_shl(1, l_shift)
                .and_then(|shift_factor| usize::from(rgbe.e).checked_mul(shift_factor))
                .ok_or(LoadError::Rle)?;

            let from = scanline[0];

            scanline
                .get_mut(1..=count)
                .ok_or(LoadError::Rle)?
                .iter_mut()
                .for_each(|to| *to = from);

            scanline = &mut scanline[count..];
            l_shift += 8;
        } else {
            scanline[1] = rgbe.into();
            scanline = &mut scanline[1..];
            l_shift = 0;
        }
    }

    Ok(())
}

fn decrunch<R: BufRead>(mut reader: R, scanline: &mut [RGB]) -> LoadResult {
    const MIN_LEN: usize = 8;
    const MAX_LEN: usize = 0x7fff;

    let rgbe = reader.read_rgbe()?;

    if !(MIN_LEN..=MAX_LEN).contains(&scanline.len()) || !rgbe.is_new_decrunch_marker() {
        scanline[0] = rgbe.into();
        return old_decrunch(reader, scanline);
    }

    let mut decrunch_channel = |mutate_pixel: fn(&mut RGB, u8)| {
        let mut scanline = &mut scanline[..];
        while !scanline.is_empty() {
            let code = reader.read_byte()? as usize;
            if code > 128 {
                // run
                let count = code & 127;
                let pixels = scanline.get_mut(..count).ok_or(LoadError::Rle)?;

                let val = reader.read_byte()?;
                for pixel in pixels {
                    mutate_pixel(pixel, val);
                }
                scanline = &mut scanline[count..];
            } else {
                // non-run
                let mut bytes_left = code;
                while bytes_left > 0 {
                    let buf = reader.fill_buf()?;

                    if buf.is_empty() {
                        #[cold]
                        fn fail() -> LoadResult<()> {
                            Err(LoadError::Eof(IoError::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "failed to fill whole buffer",
                            )))
                        }

                        return fail();
                    }

                    let count = buf.len().min(bytes_left);
                    let pixels = scanline.get_mut(..count).ok_or(LoadError::Rle)?;

                    for (pixel, &val) in pixels.iter_mut().zip(&buf[..count]) {
                        mutate_pixel(pixel, val);
                    }
                    scanline = &mut scanline[count..];
                    reader.consume(count);
                    bytes_left -= count;
                }
            }
        }

        Ok(())
    };

    decrunch_channel(|pixel, val| pixel.r = val as f32)?;
    decrunch_channel(|pixel, val| pixel.g = val as f32)?;
    decrunch_channel(|pixel, val| pixel.b = val as f32)?;
    decrunch_channel(RGB::apply_exposure)?;

    Ok(())
}

/// A decoded Radiance HDR image.
#[derive(Debug)]
pub struct Image {
    /// The width of the image, in pixels.
    pub width: usize,
    /// The height of the image, in pixels.
    pub height: usize,
    /// The decoded image data.
    pub data: Vec<RGB>,
}

impl Image {
    /// Calculate an offset into the data buffer, given an x and y coordinate.
    pub fn pixel_offset(&self, x: usize, y: usize) -> usize {
        self.width * y + x
    }

    /// Get a pixel at a specific x and y coordinate. Will panic if out of bounds.
    pub fn pixel(&self, x: usize, y: usize) -> &RGB {
        let offset = self.pixel_offset(x, y);
        &self.data[offset]
    }
}

const MAGIC: &[u8; 10] = b"#?RADIANCE";

/// Load a Radiance HDR image from a reader that implements [`BufRead`].
pub fn load<R: BufRead>(mut reader: R) -> LoadResult<Image> {
    let mut buf = [0u8; MAGIC.len()];
    reader.read_exact(&mut buf)?;

    if &buf != MAGIC {
        return Err(LoadError::FileFormat);
    }

    // Grab image dimensions
    let (width, height, mut reader) = dim_parser::parse_header(reader)?;

    let length = width.checked_mul(height).ok_or(LoadError::FileFormat)?;

    // Allocate result buffer
    let mut data = vec![
        RGB {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        };
        length
    ];

    if length > 0 {
        // Decrunch image data
        for row in 0..height {
            let start = row * width;
            let end = start + width;
            decrunch(&mut reader, &mut data[start..end])?;
        }
    }

    Ok(Image {
        width,
        height,
        data,
    })
}
