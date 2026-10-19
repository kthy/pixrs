//! Static definitions for PixRS.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

/// Clap about text.
pub static ABOUT: &str = "View, deduplicate and organize your media files.";

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
