//! Static definitions for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

/// Clap about text.
pub static ABOUT: &str = "A CLI tool that presents a bunch of media files in a reasonable manner.";

/// Clap after-help text.
pub static AFTER_HELP: &str = "LEGAL NOTICE:
    This Software is released under the GNU Affero General Public License,
    version 3 or any later version (SPDX: \"AGPL-3.0-or-later\").

    TODO \"contains software from\"

    You may obtain a copy of the Software's corresponding source code from
    <https://github.com/kthy/pixrs>. To read the full license notices, run
    the Software with the `--license` flag.
";

/// Full license text, displayed by `--license`.
pub static LICENSE: &str = include_str!("../LICENSE.md");

/// Glob options.
pub static GLOPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Protocol for the local server.
pub static PROTOCOL: &str = "http://";
/// Base URL for the local server.
pub static SERVER_URL: &str = "http://localhost:2635";

/// HTMX SRI hash.
pub static HTMX_SRI: &str =
    "sha384-ujb1lZYygJmzgSwoxRggbCHcjc0rB2XoQrxeTUQyRjrOnlCoYta87iKBWq3EsdM2";

/// HTMX unpkg URL.
pub static HTMX_URL: &str = "https://unpkg.com/htmx.org@1.9.12/dist/htmx.min.js";

/// Lightbox2 CSS SRI hash.
pub static LIGHTBOX_CSS_SRI: &str =
    "sha384-MzHT0pgTPI8fjMvEz54yA7HmSVjLLsxl+ytGS13+EOr/L5uLaBakmchzKgDYsSQz";

/// Lightbox2 CSS unpkg URL.
pub static LIGHTBOX_CSS_URL: &str = "https://unpkg.com/lightbox2@2.11.4/dist/css/lightbox.min.css";

/// Lightbox2 JS SRI hash.
pub static LIGHTBOX_JS_SRI: &str =
    "sha384-uTmpRFR0EdFNfmq7QDdVBqyOmvNYqjnAtF/pfcyZSXCCBkirh2VWddSoaQHW8dN1";

/// Lightbox2 JS unpkg URL.
pub static LIGHTBOX_JS_URL: &str =
    "https://unpkg.com/lightbox2@2.11.4/dist/js/lightbox-plus-jquery.min.js";
