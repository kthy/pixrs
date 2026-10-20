//! Media types and file classification.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

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
            Some(MediaType::Image)
        } else if VIDEO_EXTENSIONS.contains(&ext) {
            Some(MediaType::Video)
        } else if AUDIO_EXTENSIONS.contains(&ext) {
            Some(MediaType::Audio)
        } else {
            None
        }
    }

    /// The name used in the catalog and in templates.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Image => "image",
            MediaType::Video => "video",
            MediaType::Audio => "audio",
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MediaType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "image" => Ok(MediaType::Image),
            "video" => Ok(MediaType::Video),
            "audio" => Ok(MediaType::Audio),
            _ => Err(format!("unknown media type {:?}", s)),
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
