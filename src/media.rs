//! Media types and file classification.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use crate::decode::Decoder;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// What pixrs does with files of a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// An image, with how it is decoded; `None` for formats pixrs has no decoder for (RAW).
    Image(Option<Decoder>),
    Video,
    Audio,
}

/// Every format pixrs catalogs, by lowercase file extension.
const FORMATS: &[(&str, Format)] = &[
    ("arw", Format::Image(None)),
    ("avif", Format::Image(Some(Decoder::Ffmpeg))),
    ("bmp", Format::Image(Some(Decoder::InProcess(None)))),
    ("cr2", Format::Image(None)),
    ("cr3", Format::Image(None)),
    ("dng", Format::Image(None)),
    ("gif", Format::Image(Some(Decoder::GIF))),
    ("heic", Format::Image(Some(Decoder::Ffmpeg))),
    ("heif", Format::Image(Some(Decoder::Ffmpeg))),
    ("jpeg", Format::Image(Some(Decoder::JPEG))),
    ("jpg", Format::Image(Some(Decoder::JPEG))),
    ("nef", Format::Image(None)),
    ("orf", Format::Image(None)),
    ("png", Format::Image(Some(Decoder::PNG))),
    ("raf", Format::Image(None)),
    ("rw2", Format::Image(None)),
    ("tif", Format::Image(Some(Decoder::TIFF))),
    ("tiff", Format::Image(Some(Decoder::TIFF))),
    ("webp", Format::Image(Some(Decoder::WEBP))),
    ("3gp", Format::Video),
    ("avi", Format::Video),
    ("m2ts", Format::Video),
    ("m4v", Format::Video),
    ("mkv", Format::Video),
    ("mov", Format::Video),
    ("mp4", Format::Video),
    ("mpeg", Format::Video),
    ("mpg", Format::Video),
    ("mts", Format::Video),
    ("webm", Format::Video),
    ("wmv", Format::Video),
    ("aac", Format::Audio),
    ("aif", Format::Audio),
    ("aiff", Format::Audio),
    ("flac", Format::Audio),
    ("m4a", Format::Audio),
    ("mp3", Format::Audio),
    ("oga", Format::Audio),
    ("ogg", Format::Audio),
    ("opus", Format::Audio),
    ("wav", Format::Audio),
    ("wma", Format::Audio),
];

impl Format {
    /// The format of the file at `path`, by its extension in any letter case; `None` for files
    /// that are not media.
    pub fn of(path: &Path) -> Option<Self> {
        let ext = extension(path)?;
        FORMATS
            .iter()
            .find(|(known, _)| *known == ext)
            .map(|&(_, format)| format)
    }

    /// The kind of media files of this format hold.
    pub const fn media_type(self) -> MediaType {
        match self {
            Self::Image(_) => MediaType::Image,
            Self::Video => MediaType::Video,
            Self::Audio => MediaType::Audio,
        }
    }
}

/// The extension of `path` in lowercase, if it has one that is valid UTF-8.
pub fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// The kind of media a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
    Audio,
}

impl MediaType {
    /// Classify a file by its extension, case-insensitively. Returns `None` for non-media files.
    pub fn from_path(path: &Path) -> Option<Self> {
        Format::of(path).map(Format::media_type)
    }

    /// The name used in the catalog and in templates.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToSql for MediaType {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for MediaType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|e: String| FromSqlError::Other(e.into()))
    }
}

impl From<MediaType> for Value {
    fn from(media_type: MediaType) -> Self {
        Self::Text(String::from(media_type.as_str()))
    }
}

/// Width and height of an image or video in pixels, both above zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl Dimensions {
    /// The dimensions, if both are known and above zero.
    pub fn new(width: Option<u32>, height: Option<u32>) -> Option<Self> {
        let (width, height) = (width?, height?);
        (width > 0 && height > 0).then_some(Self { width, height })
    }

    /// The dimensions turned a quarter: width and height swapped.
    #[must_use]
    pub const fn transposed(self) -> Self {
        Self {
            width: self.height,
            height: self.width,
        }
    }

    /// Width times height.
    pub fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Long side over short side.
    pub fn aspect(self) -> f64 {
        f64::from(self.width.max(self.height)) / f64::from(self.width.min(self.height))
    }
}

impl FromStr for MediaType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "image" => Ok(Self::Image),
            "video" => Ok(Self::Video),
            "audio" => Ok(Self::Audio),
            _ => Err(format!("unknown media type {s:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn test_from_path() {
        assert_eq!(
            MediaType::from_path(Path::new("a/b.jpg")),
            Some(MediaType::Image)
        );
        assert_eq!(
            MediaType::from_path(Path::new("IMG_0001.HEIC")),
            Some(MediaType::Image)
        );
        assert_eq!(
            MediaType::from_path(Path::new("clip.MoV")),
            Some(MediaType::Video)
        );
        assert_eq!(
            MediaType::from_path(Path::new("song.flac")),
            Some(MediaType::Audio)
        );
        assert_eq!(MediaType::from_path(Path::new("notes.txt")), None);
        assert_eq!(MediaType::from_path(Path::new("jpg")), None);
    }

    #[test]
    fn test_every_format() {
        let images = [
            "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "jpeg", "jpg",
            "nef", "orf", "png", "raf", "rw2", "tif", "tiff", "webp",
        ];
        let videos = [
            "3gp", "avi", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "webm", "wmv",
        ];
        let audio = [
            "aac", "aif", "aiff", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wma",
        ];
        let in_process = ["bmp", "gif", "jpeg", "jpg", "png", "tif", "tiff", "webp"];
        let by_ffmpeg = ["avif", "heic", "heif"];
        let kinds = [
            (&images[..], MediaType::Image),
            (&videos[..], MediaType::Video),
            (&audio[..], MediaType::Audio),
        ];
        for (extensions, media_type) in kinds {
            for ext in extensions {
                let upper = format!("a/b.{}", ext.to_ascii_uppercase());
                assert_eq!(
                    MediaType::from_path(Path::new(&upper)),
                    Some(media_type),
                    "{ext}"
                );
                let decoder = crate::decode::image_decoder(&format!("b.{ext}"));
                let expected = if in_process.contains(ext) {
                    "in process"
                } else if by_ffmpeg.contains(ext) {
                    "ffmpeg"
                } else {
                    "none"
                };
                let got = match decoder {
                    Some(Decoder::InProcess(_)) => "in process",
                    Some(Decoder::Ffmpeg) => "ffmpeg",
                    None => "none",
                };
                assert_eq!(got, expected, "{ext}");
            }
        }
        assert_eq!(FORMATS.len(), images.len() + videos.len() + audio.len());
    }

    #[test]
    fn test_round_trip() {
        for t in [MediaType::Image, MediaType::Video, MediaType::Audio] {
            assert_eq!(t.as_str().parse::<MediaType>(), Ok(t));
        }
        assert!("other".parse::<MediaType>().is_err());
    }
}
