//! Run the compiler against a bunch of inputs, tracking failures

use std::path::{Path, PathBuf};

use clap::Parser;
use fea_rs::{
    compile::{self, Opts},
    util::ttx::{self as test_utils, TestCase, TestResult},
    GlyphMap,
};

fn main() {
    let args = Args::parse();
    let to_run = std::fs::read_to_string(&args.input).expect("failed to read input");
    let filter = test_utils::Filter::new(args.test_filter.as_ref());
    let mut results = Vec::new();
    for path in to_run.lines().filter(|line| filter.filter(line)) {
        let path = Path::new(path);
        let request = norad::DataRequest::none().lib(true);
        let Ok(font) = norad::Font::load_requested_data(&path, request) else { continue };
        let glyph_order = compile::get_ufo_glyph_order(&font);
        let fea_path = path.join("features.fea");
        if glyph_order.is_none() || !fea_path.exists() {
            results.push(Err(TestCase {
                path: path.to_path_buf(),
                reason: TestResult::Other("skipped".to_string()),
            }));
            continue;
        }
        results.push(try_compile(path, glyph_order.as_ref().unwrap()));
    }

    let results = test_utils::finalize_results(results);
    eprintln!("{:?}", results.printer(args.verbose));
}

fn try_compile(ufo_path: &Path, map: &GlyphMap) -> Result<PathBuf, TestCase> {
    match std::panic::catch_unwind(|| try_compile_body(&ufo_path, map)) {
        Err(_) => Err(TestCase {
            path: ufo_path.to_path_buf(),
            reason: TestResult::Panic,
        }),
        Ok(Err(e)) => Err(e),
        Ok(_) => Ok(ufo_path.to_path_buf()),
    }
}

fn try_compile_body(ufo_path: &Path, glyph_map: &GlyphMap) -> Result<(), TestCase> {
    let fea_path = ufo_path.join("features.fea");
    match test_utils::try_parse_file(&fea_path, Some(glyph_map)) {
        Err((node, errs)) => Err(TestCase {
            path: ufo_path.to_owned(),
            reason: TestResult::ParseFail(test_utils::stringify_diagnostics(&node, &errs)),
        }),
        Ok(node) => match compile::compile(&node, glyph_map) {
            Ok(output) => {
                if let Ok(mut built) = output.build_raw(glyph_map, Opts::new()) {
                    let _bytes = built.build();
                }
                Ok(())
            }
            Err(errs) => Err(TestCase {
                path: ufo_path.to_owned(),
                reason: TestResult::CompileFail(test_utils::stringify_diagnostics(&node, &errs)),
            }),
        },
    }
}

/// Compare compilation output to expected results
#[derive(clap::Parser, Debug)]
#[command(author, version, long_about = None)]
struct Args {
    /// path to a file containing a list of ufo files to build
    input: PathBuf,
    /// Display more information about failures
    ///
    /// This includes errors encountered, as well as the generated diffs when
    /// comparison fails.
    #[arg(short, long)]
    verbose: bool,
    /// Optional comma separated list of words for filtering which fonts to build.
    ///
    /// e.g.: -t "font1,otherfont" matches font1-regular.ufo, otherfont.ufo, etc
    #[arg(short, long = "test")]
    test_filter: Option<String>,
}
