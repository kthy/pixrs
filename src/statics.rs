//! Static definitions for PixRS.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use glob::MatchOptions;

/// Glob options.
pub static GLOPTS: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Protocol for the local server.
pub static PROTOCOL: &str = "http://";
/// Base URL for the local server.
pub static SERVER_URL: &str = "http://localhost:2635";

/// HTMX SRI hash.
pub static HTMX_SRI: &str = "sha384-tvG/2mnCFmGQzYC1Oh3qxQ7CkQ9kMzYjWZSNtrRZygHPDDqottzEJsqS4oUVodhW";
/// HTMX unpkg URL.
pub static HTMX_URL: &str = "https://unpkg.com/htmx.org@1.6.1";
