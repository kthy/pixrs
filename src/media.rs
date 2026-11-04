//! Media types and file classification.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// File extensions treated as images, lowercase.
const IMAGE_EXTENSIONS: &[&str] = &[
    "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "jpeg", "jpg", "nef", "orf",
    "png", "raf", "rw2", "tif", "tiff", "webp",
];

/// File extensions treated as video, lowercase.
const VIDEO_EXTENSIONS: &[&str] = &[
    "3gp", "avi", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "webm", "wmv",
];

/// File extensions treated as audio, lowercase.
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wma",
];

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
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let ext = ext.as_str();
        if IMAGE_EXTENSIONS.contains(&ext) {
            Some(Self::Image)
        } else if VIDEO_EXTENSIONS.contains(&ext) {
            Some(Self::Video)
        } else if AUDIO_EXTENSIONS.contains(&ext) {
            Some(Self::Audio)
        } else {
            None
        }
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
    fn test_round_trip() {
        for t in [MediaType::Image, MediaType::Video, MediaType::Audio] {
            assert_eq!(t.as_str().parse::<MediaType>(), Ok(t));
        }
        assert!("other".parse::<MediaType>().is_err());
    }
}
