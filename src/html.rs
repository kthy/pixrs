//! Functions for generating HTML markup.

/*************************************************************************************************
 * Copyright © 2022 Kristian Thy (<k@pyxy.dk>)                                                   *
 * This file is part of PixRS: <https://github.com/pyxy-dk/pixrs>                                *
 *************************************************************************************************/

use crate::statics::{
    HTMX_SRI, HTMX_URL, LIGHTBOX_CSS_SRI, LIGHTBOX_CSS_URL, LIGHTBOX_JS_SRI, LIGHTBOX_JS_URL,
};
use crate::structs::PixrsOpt;
use log::{error, trace};
use maud::{html, Markup, DOCTYPE};
use time::{macros::format_description, OffsetDateTime};

/// Generate an emoji favicon as inline SVG.
fn favicon(emoji: &str, x: i8, y: i8) -> Markup {
    trace!("favicon({:?})", emoji);
    let data = vec![
        "data:image/svg+xml,",
        "%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2016%2016'%3E%3Ctext",
        &format!("%20x='{}'", x),
        &format!("%20y='{}'%3E", y),
        emoji,
        "%3C/text%3E%3C/svg%3E",
    ]
    .concat();
    html! { link rel="icon" href=(data) type="image/svg+xml"; }
}

/// Generate HTML duplicate file warning markup.
///
/// ```
/// let html = pixrs::html::html_index_dupe("foo", "bar").into_string();
/// pretty_assertions::assert_eq!(html, "<span class=\"dupe\">\
///                                          The file \
///                                          <code>bar</code> \
///                                          is a duplicate of \
///                                          <code>foo</code>\
///                                      </span>");
/// ```
pub fn html_index_dupe(the_real_deal: &str, the_impostor: &str) -> Markup {
    trace!("html_index_dupe({:?}, {:?})", the_real_deal, the_impostor);

    html! {
        span .dupe {
            "The file "
            code { (the_impostor) }
            " is a duplicate of "
            code { (the_real_deal) }
        }
    }
}

/// Generate HTML header markup.
fn html_index_head() -> Markup {
    trace!("html_index_head()");

    html!(
        meta charset="utf-8";
        title { "PixRS output" }
        meta name="viewport" content="width=device-width, initial-scale=1";
        (favicon("🗃️", -3, 14))
        link rel="stylesheet" href=(LIGHTBOX_CSS_URL) integrity=(LIGHTBOX_CSS_SRI) crossorigin="anonymous";
        style { (stylesheet()) }
        script src=(HTMX_URL) integrity=(HTMX_SRI) crossorigin="anonymous" {}
    )
}

/// Generate HTML header markup.
fn html_index_header(from_path: &str) -> Markup {
    trace!("html_index_header({:?})", from_path);

    let dt = OffsetDateTime::now_local().unwrap_or_else(|e| {
        error!("Unable to get local time (defaulting to UTC): {:?}", e);
        OffsetDateTime::now_utc()
    });
    let dt_fmt = format_description!(
        "[weekday], [day] [month repr:long] [year] at \
         [hour repr:24]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    let formatted_dt = dt.format(dt_fmt).expect("Unable to format DateTime!");
    html!(
        h1 { "PixRS" }
        p { "Output from " code { (from_path) } " generated " (formatted_dt) ":" }
    )
}

/// Generate HTML index page markup.
pub fn html_index<F>(opt: &PixrsOpt, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("html_index({:?}, _)", opt);

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                (html_index_head())
            }
            body {
                header {
                    (html_index_header(&opt.base_path))
                }
                main {
                    (html_index_main(opt, &on_err))
                }
            }
        }
    }
}

/// Generate HTML body markup.
fn html_index_main<F>(opt: &PixrsOpt, on_err: F) -> Markup
where
    F: Fn(i32),
{
    trace!("html_index_main({:?}, _)", opt);

    let glob_pattern = crate::create_glob_pattern(&opt.base_path, opt.recurse);

    html!(
        section #audio { (crate::audio::into_html(opt, &glob_pattern, &on_err)) }
        section #image { (crate::image::into_html(opt, &glob_pattern, &on_err)) }
        section #video { (crate::video::into_html(opt, &glob_pattern, &on_err)) }
        script src=(LIGHTBOX_JS_URL) integrity=(LIGHTBOX_JS_SRI) crossorigin="anonymous" {}
    )
}

/// Generate inline CSS.
fn stylesheet() -> String {
    trace!("stylesheet()");

    String::from(
        "
        audio {
            border-radius: 8px;
            padding: 8px 4px;
        }

        code {
            background-color: #ddd;
            border-radius: 6px;
            color: #222;
            padding: 2px 4px;
        }

        li { padding: 2px 0; }

        .clickable { cursor: pointer; }

        .dupe { color: red; }

        .ease-in { transition: all ease-in 3s; } /* FIXME */

        .video_thumbnail {
            float: left;
        }
    ",
    )
}
