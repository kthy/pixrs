use pixrs::{structs::PixrsOpt, write_html};
use std::path::Path;

#[test]
fn e2e() {
    let opt = PixrsOpt {
        base_path: String::from("tests/files"),
        clobber: true,
        create_output_dir: true,
        license: false,
        no_server: true,
        open: false,
        out_path: Path::new("./target/e2e").to_path_buf(),
        quiet: true,
        recurse: true,
    };
    let idx = format!("{}/index.html", opt.out_path.display());
    let on_err = |_| panic!("at the callback");
    let _ = write_html(&opt, &idx, on_err);
}
