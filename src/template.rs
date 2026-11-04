//! Templates that say where `pixrs organize` puts each file, e.g.
//! `{type}/{year}/{month}/{date}_{time}.{ext}`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use std::fmt::{self, Write as _};

use crate::dates::Taken;
use crate::media::MediaType;

/// Characters Windows does not allow in file names, besides control characters.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// File names Windows reserves, case-insensitively, with or without an extension.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The file name every preset uses.
const PRESET_FILE: &str = "{date}_{time}.{ext}";

/// A template token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Date,
    Time,
    Type,
    Camera,
    Name,
    Ext,
}

impl Token {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "year" => Self::Year,
            "month" => Self::Month,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" => Self::Minute,
            "second" => Self::Second,
            "date" => Self::Date,
            "time" => Self::Time,
            "type" => Self::Type,
            "camera" => Self::Camera,
            "name" => Self::Name,
            "ext" => Self::Ext,
            _ => return None,
        })
    }

    const fn needs_date(self) -> bool {
        matches!(self, Self::Year | Self::Month | Self::Day | Self::Date) || self.needs_time()
    }

    const fn needs_time(self) -> bool {
        matches!(self, Self::Hour | Self::Minute | Self::Second | Self::Time)
    }
}

/// Text or a token in a template segment.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    Token(Token),
}

/// A parsed template: folder levels, then the file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    source: String,
    folders: Vec<Vec<Part>>,
    file: Vec<Part>,
}

/// What a template is filled in with for one file.
#[derive(Clone, Debug)]
pub struct Facts<'a> {
    pub taken: Option<Taken>,
    pub media_type: MediaType,
    pub camera: Option<&'a str>,
    /// The file name without extension.
    pub name: &'a str,
    /// The extension, without the dot.
    pub ext: &'a str,
}

/// Why a template or layout is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateError {
    /// A folder name in the template is empty, `.` or `..`.
    BadFolder { template: String },
    /// The file name lacks `{ext}`.
    NoExt { template: String },
    /// A `{` is not closed.
    Unclosed { template: String },
    /// A `}` was not opened.
    Unopened { template: String },
    /// A token name is not one of the tokens.
    UnknownToken { template: String, name: String },
    /// A character is not allowed in file names on Windows.
    Illegal { template: String, c: char },
    /// A layout lists a preset twice.
    Repeated { layout: String, preset: String },
    /// A layout preset does not exist.
    UnknownPreset { preset: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadFolder { template } => write!(
                f,
                "Template {template:?}: folder names cannot be empty, `.` or `..`"
            ),
            Self::NoExt { template } => write!(
                f,
                "Template {template:?}: the file name must contain {{ext}}, or files lose their \
                 extension"
            ),
            Self::Unclosed { template } => {
                write!(f, "Template {template:?}: `{{` without a closing `}}`")
            }
            Self::Unopened { template } => write!(f, "Template {template:?}: `}}` without `{{`"),
            Self::UnknownToken { template, name } => write!(
                f,
                "Template {template:?}: unknown token {{{name}}}. Tokens: {{year}} {{month}} \
                 {{day}} {{hour}} {{minute}} {{second}} {{date}} {{time}} {{type}} {{camera}} \
                 {{name}} {{ext}}"
            ),
            Self::Illegal { template, c } => write!(
                f,
                "Template {template:?}: {c:?} is not allowed in file names on Windows"
            ),
            Self::Repeated { layout, preset } => {
                write!(f, "Layout {layout:?} lists {preset} twice")
            }
            Self::UnknownPreset { preset } => write!(
                f,
                "Unknown layout {preset:?}: use flat, year, year-month or type, or several \
                 separated by commas"
            ),
        }
    }
}

impl std::error::Error for TemplateError {}

impl Template {
    /// Parse a template like `{type}/{year}/{date}_{time}.{ext}`.
    pub fn parse(source: &str) -> Result<Self, TemplateError> {
        let mut segments: Vec<Vec<Part>> = vec![];
        for segment in source.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(TemplateError::BadFolder {
                    template: String::from(source),
                });
            }
            segments.push(parse_segment(source, segment)?);
        }
        let file = segments.pop().expect("split yields at least one segment");
        if !file.contains(&Part::Token(Token::Ext)) {
            return Err(TemplateError::NoExt {
                template: String::from(source),
            });
        }
        Ok(Self {
            source: String::from(source),
            folders: segments,
            file,
        })
    }

    /// The template for layout presets, e.g. `type,year-month`: their folders in that order,
    /// then `{date}_{time}.{ext}`.
    pub fn from_layout(layout: &str) -> Result<Self, TemplateError> {
        let mut folders: Vec<&str> = vec![];
        let mut seen: Vec<&str> = vec![];
        for preset in layout.split(',').map(str::trim) {
            if seen.contains(&preset) {
                return Err(TemplateError::Repeated {
                    layout: String::from(layout),
                    preset: String::from(preset),
                });
            }
            seen.push(preset);
            match preset {
                "flat" => {}
                "year" => folders.push("{year}"),
                "year-month" => folders.extend(["{year}", "{month}"]),
                "type" => folders.push("{type}"),
                _ => {
                    return Err(TemplateError::UnknownPreset {
                        preset: String::from(preset),
                    })
                }
            }
        }
        folders.push(PRESET_FILE);
        Self::parse(&folders.join("/"))
    }

    /// The template as given.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Where a file goes: folder names and the file name, valid on every OS.
    ///
    /// Folder levels that need a date (or time) the file lacks become one `undated` folder. A
    /// file name that needs a value the file lacks becomes its original name, except that a
    /// WhatsApp file's counter stands in for a missing `{time}` (see `whatsapp_counter`).
    pub fn render(&self, facts: &Facts, undated: &str) -> Rendered {
        let mut folders: Vec<String> = vec![];
        let mut in_undated = false;
        for segment in &self.folders {
            if has(segment, facts) {
                folders.push(sanitize(&fill(segment, facts, None)));
                in_undated = false;
            } else if !in_undated {
                folders.push(sanitize(undated));
                in_undated = true;
            }
        }
        let file = if has(&self.file, facts) {
            fill(&self.file, facts, None)
        } else if let Some(counter) = self.counter_for_time(facts) {
            fill(&self.file, facts, Some(counter))
        } else {
            format!("{}.{}", facts.name, facts.ext)
        };
        Rendered {
            folders,
            file: sanitize(&file),
        }
    }

    /// The WhatsApp counter to put in place of `{time}` in the file name, if the file has a
    /// date but no time, its name has a counter, and the time is only needed as `{time}`: a
    /// counter cannot fill `{hour}`, `{minute}` or `{second}`.
    fn counter_for_time<'a>(&self, facts: &Facts<'a>) -> Option<&'a str> {
        let date_only = facts.taken.is_some_and(|d| d.time.is_none());
        let only_time_token = !self.file.iter().any(|part| {
            matches!(
                part,
                Part::Token(Token::Hour | Token::Minute | Token::Second)
            )
        });
        (date_only && only_time_token)
            .then(|| whatsapp_counter(facts.name))
            .flatten()
    }
}

/// Where a template puts a file: folder names, outermost first, and the file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub folders: Vec<String>,
    pub file: String,
}

/// The counter at the end of a WhatsApp file name: `WA0001` in `IMG-20190501-WA0001` (also
/// `VID-`, `AUD-`, `PTT-` and the like), and in `2019-05-01_WA0001`, the name pixrs gives such
/// a file. WhatsApp numbers the files of a day in the order they arrived, so the counter sorts
/// them within the day as a time would, and the names sort among the dated files around them.
fn whatsapp_counter(name: &str) -> Option<&str> {
    let (_, counter) = name.rsplit_once(['-', '_'])?;
    let digits = counter.strip_prefix("WA")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(counter)
}

/// Parse one `/`-free part of a template.
fn parse_segment(source: &str, segment: &str) -> Result<Vec<Part>, TemplateError> {
    let template = || String::from(source);
    let mut parts = vec![];
    let mut rest = segment;
    while !rest.is_empty() {
        match rest.find(['{', '}']) {
            Some(0) if rest.starts_with('{') => {
                let end = rest.find('}').ok_or_else(|| TemplateError::Unclosed {
                    template: template(),
                })?;
                let name = &rest[1..end];
                let token = Token::parse(name).ok_or_else(|| TemplateError::UnknownToken {
                    template: template(),
                    name: String::from(name),
                })?;
                parts.push(Part::Token(token));
                rest = &rest[end + 1..];
            }
            Some(0) => {
                return Err(TemplateError::Unopened {
                    template: template(),
                })
            }
            found => {
                let end = found.unwrap_or(rest.len());
                let text = &rest[..end];
                if let Some(c) = text
                    .chars()
                    .find(|&c| ILLEGAL.contains(&c) || c.is_control())
                {
                    return Err(TemplateError::Illegal {
                        template: template(),
                        c,
                    });
                }
                parts.push(Part::Text(String::from(text)));
                rest = &rest[end..];
            }
        }
    }
    Ok(parts)
}

/// Whether the file has every value the segment needs.
fn has(segment: &[Part], facts: &Facts) -> bool {
    segment.iter().all(|part| match part {
        Part::Token(t) if t.needs_time() => facts.taken.is_some_and(|d| d.time.is_some()),
        Part::Token(t) if t.needs_date() => facts.taken.is_some(),
        _ => true,
    })
}

/// The segment filled in; `has` must be true, or `time` must stand in for the missing time.
fn fill(segment: &[Part], facts: &Facts, time: Option<&str>) -> String {
    let date = facts.taken.unwrap_or(Taken {
        year: 0,
        month: 0,
        day: 0,
        time: None,
    });
    let (hour, minute, second) = date.time.unwrap_or_default();
    let mut out = String::new();
    for part in segment {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Token(token) => {
                let _ = match token {
                    Token::Year => write!(out, "{:04}", date.year),
                    Token::Month => write!(out, "{:02}", date.month),
                    Token::Day => write!(out, "{:02}", date.day),
                    Token::Hour => write!(out, "{hour:02}"),
                    Token::Minute => write!(out, "{minute:02}"),
                    Token::Second => write!(out, "{second:02}"),
                    Token::Date => {
                        write!(out, "{:04}-{:02}-{:02}", date.year, date.month, date.day)
                    }
                    Token::Time => match time {
                        Some(time) => out.write_str(time),
                        None => write!(out, "{hour:02}{minute:02}{second:02}"),
                    },
                    Token::Type => out.write_str(facts.media_type.as_str()),
                    Token::Camera => out.write_str(facts.camera.unwrap_or("unknown")),
                    Token::Name => out.write_str(facts.name),
                    Token::Ext => out.write_str(facts.ext),
                };
            }
        }
    }
    out
}

/// A folder or file name valid on Windows, macOS and Linux: illegal and control characters
/// become `_`, trailing dots and spaces go, and reserved names like `CON` get a `_`.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    if out.is_empty() {
        return String::from("_");
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.trim_end()) {
        out.insert(stem.len(), '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    /// The folders and the file name `t` gives a file, with `undated` for missing dates.
    fn render(t: &Template, facts: &Facts) -> Vec<String> {
        let Rendered { mut folders, file } = t.render(facts, "undated");
        folders.push(file);
        folders
    }

    fn facts(taken: Option<Taken>) -> Facts<'static> {
        Facts {
            taken,
            media_type: MediaType::Video,
            camera: Some("Canon EOS 80D"),
            name: "DSC0047",
            ext: "mp4",
        }
    }

    const MAY_FIRST: Taken = Taken {
        year: 2023,
        month: 5,
        day: 1,
        time: Some((14, 32, 5)),
    };

    #[test]
    fn test_render() {
        let t = Template::parse("{type}/{year}/{month}/{date}_{time}.{ext}").unwrap();
        assert_eq!(
            render(&t, &facts(Some(MAY_FIRST))),
            ["video", "2023", "05", "2023-05-01_143205.mp4"]
        );
        // Undated: the date folders collapse into one, the file keeps its name.
        assert_eq!(
            render(&t, &facts(None)),
            ["video", "undated", "DSC0047.mp4"]
        );
        // A date without a time: date folders, original name.
        let date_only = Taken {
            time: None,
            ..MAY_FIRST
        };
        assert_eq!(
            render(&t, &facts(Some(date_only))),
            ["video", "2023", "05", "DSC0047.mp4"]
        );
        let t = Template::parse("{camera}/{date} {name}.{ext}").unwrap();
        assert_eq!(
            render(&t, &facts(Some(date_only))),
            ["Canon EOS 80D", "2023-05-01 DSC0047.mp4"]
        );
    }

    #[test]
    fn test_whatsapp_counter_stands_in_for_the_time() {
        let date_only = Taken {
            time: None,
            ..MAY_FIRST
        };
        let named = |name| Facts {
            name,
            ext: "jpg",
            ..facts(Some(date_only))
        };
        let t = Template::from_layout("year").unwrap();
        for (name, file) in [
            ("IMG-20230501-WA0001", "2023-05-01_WA0001.jpg"),
            ("VID-20230501-WA0012", "2023-05-01_WA0012.jpg"),
            // A name pixrs gave, renamed again the same way: stable.
            ("2023-05-01_WA0001", "2023-05-01_WA0001.jpg"),
            // Not a WhatsApp counter: the original name, as for any file without a time.
            ("IMG-20230501-WA", "IMG-20230501-WA.jpg"),
            ("IMG-20230501-WAx1", "IMG-20230501-WAx1.jpg"),
            ("holiday-WAVES", "holiday-WAVES.jpg"),
        ] {
            assert_eq!(render(&t, &named(name)), ["2023", file], "{}", name);
        }
        // Another template puts the counter where it puts the time.
        let t = Template::parse("{year}{month}{day}-{time}.{ext}").unwrap();
        assert_eq!(
            render(&t, &named("IMG-20230501-WA0001")),
            ["20230501-WA0001.jpg"]
        );
        // A counter cannot be an hour, minute or second.
        let t = Template::parse("{date} {hour}h{minute}.{ext}").unwrap();
        assert_eq!(
            render(&t, &named("IMG-20230501-WA0001")),
            ["IMG-20230501-WA0001.jpg"]
        );
        // With a time, the time is used; undated files keep their names.
        let t = Template::from_layout("year").unwrap();
        let timed = Facts {
            name: "IMG-20230501-WA0001",
            ..facts(Some(MAY_FIRST))
        };
        assert_eq!(render(&t, &timed), ["2023", "2023-05-01_143205.mp4"]);
        let undated = Facts {
            name: "IMG-20230501-WA0001",
            ..facts(None)
        };
        assert_eq!(render(&t, &undated), ["undated", "IMG-20230501-WA0001.mp4"]);
    }

    #[test]
    fn test_layouts() {
        let source = |l| Template::from_layout(l).unwrap().source().to_string();
        assert_eq!(source("flat"), "{date}_{time}.{ext}");
        assert_eq!(source("year"), "{year}/{date}_{time}.{ext}");
        assert_eq!(
            source("type,year-month"),
            "{type}/{year}/{month}/{date}_{time}.{ext}"
        );
        assert!(Template::from_layout("type,type").is_err());
        assert!(Template::from_layout("weekly").is_err());
    }

    #[test]
    fn test_parse_errors() {
        for bad in [
            "{year}/{date}.jpg",
            "{year}//{name}.{ext}",
            "../{name}.{ext}",
            "{nope}.{ext}",
            "{year.{ext}",
            "a}b/{name}.{ext}",
            "a:b/{name}.{ext}",
            "a\\b/{name}.{ext}",
        ] {
            assert!(Template::parse(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn test_sanitize() {
        assert_eq!(sanitize("AC/DC: live?"), "AC_DC_ live_");
        assert_eq!(sanitize("trailing. "), "trailing");
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("nul.txt"), "nul_.txt");
        assert_eq!(sanitize("console.jpg"), "console.jpg");
        assert_eq!(sanitize("..."), "_");
        assert_eq!(sanitize("Ærø 2023"), "Ærø 2023");
    }
}
