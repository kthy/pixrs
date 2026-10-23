//! Templates that say where `pixrs organize` puts each file, e.g.
//! `{type}/{year}/{month}/{date}_{time}.{ext}`.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

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
            "year" => Token::Year,
            "month" => Token::Month,
            "day" => Token::Day,
            "hour" => Token::Hour,
            "minute" => Token::Minute,
            "second" => Token::Second,
            "date" => Token::Date,
            "time" => Token::Time,
            "type" => Token::Type,
            "camera" => Token::Camera,
            "name" => Token::Name,
            "ext" => Token::Ext,
            _ => return None,
        })
    }

    fn needs_date(self) -> bool {
        matches!(self, Token::Year | Token::Month | Token::Day | Token::Date) || self.needs_time()
    }

    fn needs_time(self) -> bool {
        matches!(
            self,
            Token::Hour | Token::Minute | Token::Second | Token::Time
        )
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

impl Template {
    /// Parse a template like `{type}/{year}/{date}_{time}.{ext}`.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut segments: Vec<Vec<Part>> = vec![];
        for segment in source.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(format!(
                    "Template {:?}: folder names cannot be empty, `.` or `..`",
                    source
                ));
            }
            segments.push(parse_segment(source, segment)?);
        }
        let file = segments.pop().expect("split yields at least one segment");
        if !file.contains(&Part::Token(Token::Ext)) {
            return Err(format!(
                "Template {:?}: the file name must contain {{ext}}, or files lose their \
                 extension",
                source
            ));
        }
        Ok(Template {
            source: String::from(source),
            folders: segments,
            file,
        })
    }

    /// The template for layout presets, e.g. `type,year-month`: their folders in that order,
    /// then `{date}_{time}.{ext}`.
    pub fn from_layout(layout: &str) -> Result<Self, String> {
        let mut folders: Vec<&str> = vec![];
        let mut seen: Vec<&str> = vec![];
        for preset in layout.split(',').map(str::trim) {
            if seen.contains(&preset) {
                return Err(format!("Layout {:?} lists {} twice", layout, preset));
            }
            seen.push(preset);
            match preset {
                "flat" => {}
                "year" => folders.push("{year}"),
                "year-month" => folders.extend(["{year}", "{month}"]),
                "type" => folders.push("{type}"),
                _ => {
                    return Err(format!(
                        "Unknown layout {:?}: use flat, year, year-month or type, or several \
                         separated by commas",
                        preset
                    ))
                }
            }
        }
        folders.push(PRESET_FILE);
        Template::parse(&folders.join("/"))
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
    pub fn render(&self, facts: &Facts, undated: &str) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        let mut in_undated = false;
        for segment in &self.folders {
            if has(segment, facts) {
                out.push(sanitize(&fill(segment, facts, None)));
                in_undated = false;
            } else if !in_undated {
                out.push(sanitize(undated));
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
        out.push(sanitize(&file));
        out
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
fn parse_segment(source: &str, segment: &str) -> Result<Vec<Part>, String> {
    let mut parts = vec![];
    let mut rest = segment;
    while !rest.is_empty() {
        match rest.find(['{', '}']) {
            Some(0) if rest.starts_with('{') => {
                let end = rest.find('}').ok_or_else(|| {
                    format!("Template {:?}: `{{` without a closing `}}`", source)
                })?;
                let name = &rest[1..end];
                let token = Token::parse(name).ok_or_else(|| {
                    format!(
                        "Template {:?}: unknown token {{{}}}. Tokens: {{year}} {{month}} {{day}} \
                         {{hour}} {{minute}} {{second}} {{date}} {{time}} {{type}} {{camera}} \
                         {{name}} {{ext}}",
                        source, name
                    )
                })?;
                parts.push(Part::Token(token));
                rest = &rest[end + 1..];
            }
            Some(0) => return Err(format!("Template {:?}: `}}` without `{{`", source)),
            found => {
                let end = found.unwrap_or(rest.len());
                let text = &rest[..end];
                if let Some(c) = text
                    .chars()
                    .find(|&c| ILLEGAL.contains(&c) || c.is_control())
                {
                    return Err(format!(
                        "Template {:?}: {:?} is not allowed in file names on Windows",
                        source, c
                    ));
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
            Part::Token(token) => out.push_str(&match token {
                Token::Year => format!("{:04}", date.year),
                Token::Month => format!("{:02}", date.month),
                Token::Day => format!("{:02}", date.day),
                Token::Hour => format!("{:02}", hour),
                Token::Minute => format!("{:02}", minute),
                Token::Second => format!("{:02}", second),
                Token::Date => format!("{:04}-{:02}-{:02}", date.year, date.month, date.day),
                Token::Time => match time {
                    Some(time) => String::from(time),
                    None => format!("{:02}{:02}{:02}", hour, minute, second),
                },
                Token::Type => String::from(facts.media_type.as_str()),
                Token::Camera => String::from(facts.camera.unwrap_or("unknown")),
                Token::Name => String::from(facts.name),
                Token::Ext => String::from(facts.ext),
            }),
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
            t.render(&facts(Some(MAY_FIRST)), "undated"),
            ["video", "2023", "05", "2023-05-01_143205.mp4"]
        );
        // Undated: the date folders collapse into one, the file keeps its name.
        assert_eq!(
            t.render(&facts(None), "undated"),
            ["video", "undated", "DSC0047.mp4"]
        );
        // A date without a time: date folders, original name.
        let date_only = Taken {
            time: None,
            ..MAY_FIRST
        };
        assert_eq!(
            t.render(&facts(Some(date_only)), "undated"),
            ["video", "2023", "05", "DSC0047.mp4"]
        );
        let t = Template::parse("{camera}/{date} {name}.{ext}").unwrap();
        assert_eq!(
            t.render(&facts(Some(date_only)), "undated"),
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
            assert_eq!(
                t.render(&named(name), "undated"),
                ["2023", file],
                "{}",
                name
            );
        }
        // Another template puts the counter where it puts the time.
        let t = Template::parse("{year}{month}{day}-{time}.{ext}").unwrap();
        assert_eq!(
            t.render(&named("IMG-20230501-WA0001"), "undated"),
            ["20230501-WA0001.jpg"]
        );
        // A counter cannot be an hour, minute or second.
        let t = Template::parse("{date} {hour}h{minute}.{ext}").unwrap();
        assert_eq!(
            t.render(&named("IMG-20230501-WA0001"), "undated"),
            ["IMG-20230501-WA0001.jpg"]
        );
        // With a time, the time is used; undated files keep their names.
        let t = Template::from_layout("year").unwrap();
        let timed = Facts {
            name: "IMG-20230501-WA0001",
            ..facts(Some(MAY_FIRST))
        };
        assert_eq!(
            t.render(&timed, "undated"),
            ["2023", "2023-05-01_143205.mp4"]
        );
        let undated = Facts {
            name: "IMG-20230501-WA0001",
            ..facts(None)
        };
        assert_eq!(
            t.render(&undated, "undated"),
            ["undated", "IMG-20230501-WA0001.mp4"]
        );
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
