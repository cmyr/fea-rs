//! utilities for compiling and comparing ttx

use std::{
    collections::HashMap,
    convert::TryInto,
    env::temp_dir,
    ffi::OsStr,
    fmt::{Debug, Write},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use crate::{Compilation, Diagnostic, GlyphIdent, GlyphMap, GlyphName, ParseTree};

use ansi_term::Color;
use fonttools::{font::Font, tables};
use rayon::prelude::*;

static IGNORED_TESTS: &[&str] = &[
    // ## tests with invalid syntax ## //
    "GSUB_5_formats.fea",
    "AlternateChained.fea",
    "GSUB_6.fea",
    // https://github.com/adobe-type-tools/afdko/issues/1415
    "bug509.fea",
    //
    // ## tests that should be revisited ## //
    //
    // includes syntax that is (i think) useless, and should at least be a warning
    "GSUB_8.fea",
];

/// A way to customize output when our test fails
#[derive(Default)]
pub struct Results {
    pub failures: Vec<Failure>,
    successes: Vec<PathBuf>,
}

pub struct ResultsPrinter<'a> {
    verbose: bool,
    results: &'a Results,
}

pub struct Failure {
    pub path: PathBuf,
    pub reason: Reason,
}

#[derive(PartialEq)]
pub enum Reason {
    Panic,
    ParseFail(String),
    CompileFail(String),
    UnexpectedSuccess,
    TtxFail {
        code: Option<i32>,
        std_err: String,
    },
    CompareFail {
        expected: String,
        result: String,
        diff_percent: f64,
    },
}

pub struct ReasonPrinter<'a> {
    verbose: bool,
    reason: &'a Reason,
}

pub fn assert_has_ttx_executable() {
    assert!(
        Command::new("ttx")
            .arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "\nmissing `ttx` executable. Install it with `pip install fonttools`."
    )
}

/// Run the fonttools tests.
///
/// This compiles the test files, generates ttx, and compares that with what
/// is generated by fonttools.
///
/// `filter` is an optional comma-separated list of strings. If present, only
/// tests which contain one of the strings in the list will be run.
pub fn run_all_tests(
    fonttools_data_dir: impl AsRef<Path>,
    filter: Option<&String>,
) -> Result<(), Results> {
    let glyph_map = make_glyph_map();
    let reverse_map = glyph_map.reverse_map();
    let reverse_map = reverse_map
        .into_iter()
        .map(|(id, glyph)| {
            (
                format!("glyph{:05}", id.to_raw()),
                match glyph {
                    GlyphIdent::Cid(num) => format!("cid{:05}", num),
                    GlyphIdent::Name(name) => name.to_string(),
                },
            )
        })
        .collect::<HashMap<_, _>>();

    let result = iter_compile_tests(fonttools_data_dir.as_ref(), filter)
        .par_bridge()
        .map(|path| run_test(path, &glyph_map, &reverse_map))
        .collect::<Vec<_>>();

    finalize_results(result)
}

pub fn finalize_results(result: Vec<Result<PathBuf, Failure>>) -> Result<(), Results> {
    let mut result = result
        .into_iter()
        .fold(Results::default(), |mut results, current| {
            match current {
                Err(e) => results.failures.push(e),
                Ok(path) => results.successes.push(path),
            }
            results
        });
    result.failures.sort_unstable_by(|a, b| {
        (a.reason.sort_order(), &a.path).cmp(&(b.reason.sort_order(), &b.path))
    });
    if result.failures.is_empty() {
        Ok(())
    } else {
        Err(result)
    }
}

fn iter_compile_tests<'a>(
    path: &'a Path,
    filter: Option<&'a String>,
) -> impl Iterator<Item = PathBuf> + 'a {
    let filter_items = filter
        .map(|s| s.split(',').map(|s| s.trim()).collect::<Vec<_>>())
        .unwrap_or_default();

    iter_fea_files(path).filter(move |p| {
        if p.extension() == Some(OsStr::new("fea")) && p.with_extension("ttx").exists() {
            let path_str = p.file_name().unwrap().to_str().unwrap();
            if IGNORED_TESTS.contains(&path_str) {
                return false;
            }
            if !filter_items.is_empty() && !filter_items.iter().any(|item| path_str.contains(item))
            {
                return false;
            }
            return true;
        }
        {
            false
        }
    })
}

pub fn iter_fea_files(path: impl AsRef<Path>) -> impl Iterator<Item = PathBuf> + 'static {
    let mut dir = path.as_ref().read_dir().unwrap();
    std::iter::from_fn(move || loop {
        let entry = dir.next()?.unwrap();
        let path = entry.path();
        if path.extension() == Some(OsStr::new("fea")) {
            return Some(path);
        }
    })
}

pub fn try_parse_file(
    path: &Path,
    glyphs: Option<&GlyphMap>,
) -> Result<ParseTree, (ParseTree, Vec<Diagnostic>)> {
    let ctx = crate::parse_root_file(path, glyphs, None).unwrap();
    let (tree, errs) = ctx.generate_parse_tree();
    if errs.iter().any(Diagnostic::is_error) {
        Err((tree, errs))
    } else {
        Ok(tree)
    }
}

/// takes a path to a sample ttx file
fn run_test(
    path: PathBuf,
    glyph_map: &GlyphMap,
    reverse_map: &HashMap<String, String>,
) -> Result<PathBuf, Failure> {
    match std::panic::catch_unwind(|| match try_parse_file(&path, Some(glyph_map)) {
        Err((node, errs)) => Err(Failure {
            path: path.clone(),
            reason: Reason::ParseFail(stringify_diagnostics(&node, &errs)),
        }),
        Ok(node) => match crate::compile(&node, glyph_map) {
            Err(errs) => Err(Failure {
                path: path.clone(),
                reason: Reason::CompileFail(stringify_diagnostics(&node, &errs)),
            }),
            Ok(result) => {
                let font = make_font(result, glyph_map);
                compare_ttx(font, &path, reverse_map)
            }
        },
    }) {
        Err(_) => {
            return Err(Failure {
                path,
                reason: Reason::Panic,
            })
        }
        Ok(Err(e)) => return Err(e),
        Ok(Ok(_)) => (),
    };
    Ok(path)
}

fn make_font(compilation: Compilation, glyphs: &GlyphMap) -> Font {
    let mut font = Font::new(fonttools::font::SfntVersion::TrueType);
    let maxp = tables::maxp::maxp::new05(glyphs.len().try_into().unwrap());
    font.tables.insert(maxp);
    compilation.apply(&mut font).unwrap();
    font
}

pub fn stringify_diagnostics(root: &ParseTree, diagnostics: &[Diagnostic]) -> String {
    let mut out = String::new();
    for d in diagnostics {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&root.format_diagnostic(d));
    }
    out
}

fn compare_ttx(
    mut font: Font,
    fea_path: &Path,
    reverse_map: &HashMap<String, String>,
) -> Result<(), Failure> {
    let ttx_path = fea_path.with_extension("ttx");
    let expected_diff_path = fea_path.with_extension("expected_diff");
    assert!(ttx_path.exists());
    let temp_path = temp_dir()
        .join(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string(),
        )
        .with_extension("ttf");
    font.save(&temp_path).unwrap();

    const TO_WRITE: &[&str] = &[
        "head", "name", "BASE", "GDEF", "GSUB", "GPOS", "OS/2", "STAT", "hhea", "vhea",
    ];

    let mut cmd = Command::new("ttx");
    for table in TO_WRITE {
        cmd.arg("-t").arg(table);
    }
    let status = cmd
        .arg(&temp_path)
        .output()
        .unwrap_or_else(|_| panic!("failed to execute for path {}", fea_path.display()));
    if !status.status.success() {
        let std_err = String::from_utf8_lossy(&status.stderr).into_owned();
        return Err(Failure {
            path: fea_path.into(),
            reason: Reason::TtxFail {
                code: status.status.code(),
                std_err,
            },
        });
    }

    let ttx_out_path = temp_path.with_extension("ttx");
    assert!(ttx_out_path.exists());

    let expected = std::fs::read_to_string(ttx_path).unwrap();
    let expected = rewrite_ttx(&expected, reverse_map);
    let result = std::fs::read_to_string(ttx_out_path).unwrap();
    let result = rewrite_ttx(&result, reverse_map);

    if expected_diff_path.exists() {
        let expected_diff = std::fs::read_to_string(&expected_diff_path).unwrap();
        let simple_diff = plain_text_diff(&expected, &result);
        if expected_diff == simple_diff {
            return Ok(());
        }
    }

    let diff_percent = compute_diff_percentage(&expected, &result);

    if expected != result {
        Err(Failure {
            path: fea_path.into(),
            reason: Reason::CompareFail {
                expected,
                result,
                diff_percent,
            },
        })
    } else {
        Ok(())
    }
}

pub fn compare_to_expected_output(
    output: &str,
    src_path: &Path,
    cmp_ext: &str,
) -> Result<(), Failure> {
    let cmp_path = src_path.with_extension(cmp_ext);
    let expected = if cmp_path.exists() {
        std::fs::read_to_string(&cmp_path).expect("failed to read cmp_path")
    } else {
        String::new()
    };

    if expected != output {
        let diff_percent = compute_diff_percentage(&expected, output);
        return Err(Failure {
            path: src_path.to_owned(),
            reason: Reason::CompareFail {
                expected,
                result: output.to_string(),
                diff_percent,
            },
        });
    }
    Ok(())
}
// hacky way to make our ttx output match fonttools'
fn rewrite_ttx(input: &str, reverse_map: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());

    for line in input.lines() {
        if line.starts_with("<ttFont") {
            out.push_str("<ttFont>\n");
            continue;
        }
        let mut scan = line;
        loop {
            let next = scan.find("glyph").unwrap_or(scan.len());
            out.push_str(&scan[..next]);
            scan = &scan[next..];
            if scan.is_empty() {
                break;
            }
            if scan.len() >= 10 {
                if let Some(replacement) = reverse_map.get(&scan[..10]) {
                    out.push_str(replacement);
                    scan = &scan[10..];
                    continue;
                }
            }
            out.push_str(&scan[..5]);
            scan = &scan[5..];
        }
        out.push('\n');
    }
    out
}

fn write_lines(f: &mut impl Write, lines: &[&str], line_num: usize, prefix: char) {
    writeln!(f, "L{}", line_num).unwrap();
    for line in lines {
        writeln!(f, "{}  {}", prefix, line).unwrap();
    }
}

static DIFF_PREAMBLE: &str = "\
# generated automatically by fea-rs
# this file represents an acceptable difference between the output of
# fonttools and the output of fea-rs for a given input.
";

pub fn compute_diff_percentage(left: &str, right: &str) -> f64 {
    let lines = diff::lines(left, right);
    let same = lines
        .iter()
        .filter(|l| matches!(l, diff::Result::Both { .. }))
        .count();
    let total = lines.len() as f64;
    (same as f64) / total
}

// a simple diff we write to disk
pub fn plain_text_diff(left: &str, right: &str) -> String {
    let lines = diff::lines(left, right);
    let mut result = DIFF_PREAMBLE.to_string();
    let mut temp: Vec<&str> = Vec::new();
    let mut left_or_right = None;
    let mut section_start = 0;

    for (i, line) in lines.iter().enumerate() {
        match line {
            diff::Result::Left(line) => {
                if left_or_right == Some('R') {
                    write_lines(&mut result, &temp, section_start, '<');
                    temp.clear();
                } else if left_or_right != Some('L') {
                    section_start = i;
                }
                temp.push(line);
                left_or_right = Some('L');
            }
            diff::Result::Right(line) => {
                if left_or_right == Some('L') {
                    write_lines(&mut result, &temp, section_start, '>');
                    temp.clear();
                } else if left_or_right != Some('R') {
                    section_start = i;
                }
                temp.push(line);
                left_or_right = Some('R');
            }
            diff::Result::Both { .. } => {
                match left_or_right.take() {
                    Some('R') => write_lines(&mut result, &temp, section_start, '<'),
                    Some('L') => write_lines(&mut result, &temp, section_start, '>'),
                    _ => (),
                }
                temp.clear();
            }
        }
    }
    match left_or_right.take() {
        Some('R') => write_lines(&mut result, &temp, section_start, '<'),
        Some('L') => write_lines(&mut result, &temp, section_start, '>'),
        _ => (),
    }
    result
}

pub fn make_glyph_map() -> GlyphMap {
    #[rustfmt::skip]
static TEST_FONT_GLYPHS: &[&str] = &[
    ".notdef", "space", "slash", "fraction", "semicolon", "period", "comma",
    "ampersand", "quotedblleft", "quotedblright", "quoteleft", "quoteright",
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight",
    "nine", "zero.oldstyle", "one.oldstyle", "two.oldstyle",
    "three.oldstyle", "four.oldstyle", "five.oldstyle", "six.oldstyle",
    "seven.oldstyle", "eight.oldstyle", "nine.oldstyle", "onequarter",
    "onehalf", "threequarters", "onesuperior", "twosuperior",
    "threesuperior", "ordfeminine", "ordmasculine", "A", "B", "C", "D", "E",
    "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z", "a", "b", "c", "d", "e", "f", "g",
    "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u",
    "v", "w", "x", "y", "z", "A.sc", "B.sc", "C.sc", "D.sc", "E.sc", "F.sc",
    "G.sc", "H.sc", "I.sc", "J.sc", "K.sc", "L.sc", "M.sc", "N.sc", "O.sc",
    "P.sc", "Q.sc", "R.sc", "S.sc", "T.sc", "U.sc", "V.sc", "W.sc", "X.sc",
    "Y.sc", "Z.sc", "A.alt1", "A.alt2", "A.alt3", "B.alt1", "B.alt2",
    "B.alt3", "C.alt1", "C.alt2", "C.alt3", "a.alt1", "a.alt2", "a.alt3",
    "a.end", "b.alt", "c.mid", "d.alt", "d.mid", "e.begin", "e.mid",
    "e.end", "m.begin", "n.end", "s.end", "z.end", "Eng", "Eng.alt1",
    "Eng.alt2", "Eng.alt3", "A.swash", "B.swash", "C.swash", "D.swash",
    "E.swash", "F.swash", "G.swash", "H.swash", "I.swash", "J.swash",
    "K.swash", "L.swash", "M.swash", "N.swash", "O.swash", "P.swash",
    "Q.swash", "R.swash", "S.swash", "T.swash", "U.swash", "V.swash",
    "W.swash", "X.swash", "Y.swash", "Z.swash", "f_l", "c_h", "c_k", "c_s",
    "c_t", "f_f", "f_f_i", "f_f_l", "f_i", "o_f_f_i", "s_t", "f_i.begin",
    "a_n_d", "T_h", "T_h.swash", "germandbls", "ydieresis", "yacute",
    "breve", "grave", "acute", "dieresis", "macron", "circumflex",
    "cedilla", "umlaut", "ogonek", "caron", "damma", "hamza", "sukun",
    "kasratan", "lam_meem_jeem", "noon.final", "noon.initial", "by",
    "feature", "lookup", "sub", "table", "uni0327", "uni0328", "e.fina",
];
    TEST_FONT_GLYPHS
        .iter()
        .map(|name| GlyphIdent::Name(GlyphName::new(*name)))
        .chain((800_u16..=1001).into_iter().map(GlyphIdent::Cid))
        .collect()
}

impl Results {
    fn len(&self) -> usize {
        self.failures.len() + self.successes.len()
    }

    pub fn printer(&self, verbose: bool) -> ResultsPrinter {
        ResultsPrinter {
            verbose,
            results: self,
        }
    }
}

impl Reason {
    fn sort_order(&self) -> u8 {
        match self {
            Self::Panic => 1,
            Self::ParseFail(_) => 2,
            Self::CompileFail(_) => 3,
            Self::UnexpectedSuccess => 6,
            Self::TtxFail { .. } => 10,
            Self::CompareFail { .. } => 50,
        }
    }

    pub fn printer(&self, verbose: bool) -> ReasonPrinter {
        ReasonPrinter {
            reason: self,
            verbose,
        }
    }
}

impl std::fmt::Debug for ResultsPrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "failed test cases")?;
        let widest = self
            .results
            .failures
            .iter()
            .map(|item| &item.path)
            .chain(self.results.successes.iter())
            .map(|p| p.file_name().unwrap().to_str().unwrap().len())
            .max()
            .unwrap_or(0)
            + 2;
        for success in &self.results.successes {
            writeln!(
                f,
                "{:width$} {}",
                success.file_name().unwrap().to_str().unwrap(),
                Color::Green.paint("success"),
                width = widest
            )?;
        }
        for failure in &self.results.failures {
            let file_name = failure.path.file_name().unwrap().to_str().unwrap();
            writeln!(
                f,
                "{:width$} {:?}",
                file_name,
                failure.reason.printer(self.verbose),
                width = widest
            )?;
        }
        let (panic, parse, compile, compare, perc) = self.results.failures.iter().fold(
            (0, 0, 0, 0, 0.0),
            |(panic, parse, compile, compare, perc), fail| match fail.reason {
                Reason::Panic => (panic + 1, parse, compile, compare, perc),
                Reason::ParseFail(_) => (panic, parse + 1, compile, compare, perc),
                Reason::CompileFail(_) => (panic, parse, compile + 1, compare, perc),
                Reason::CompareFail { diff_percent, .. } => {
                    (panic, parse, compile, compare + 1, perc + diff_percent)
                }
                _ => (panic, parse, compile, compare, perc),
            },
        );
        let perc = perc + self.results.successes.len() as f64;
        let perc = perc / self.results.len() as f64;

        writeln!(
            f,
            "failed {}/{} test cases",
            self.results.failures.len(),
            self.results.len()
        )?;

        writeln!(
            f,
            "{} panic, {} parse, {} compile, {} compare ({:.0}%)",
            panic,
            parse,
            compile,
            compare,
            perc * 100.0,
        )?;
        Ok(())
    }
}

impl std::fmt::Debug for Results {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.printer(false).fmt(f)
    }
}

impl Debug for ReasonPrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            Reason::Panic => write!(f, "{}", Color::Red.paint("panic")),
            Reason::ParseFail(diagnostics) => {
                write!(f, "{}", Color::Purple.paint("parse failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            Reason::CompileFail(diagnostics) => {
                write!(f, "{}", Color::Yellow.paint("compile failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            Reason::UnexpectedSuccess => write!(f, "{}", Color::Yellow.paint("unexpected success")),
            Reason::TtxFail { code, std_err } => {
                write!(f, "ttx failure ({:?}) stderr:\n{}", code, std_err)
            }
            Reason::CompareFail {
                expected,
                result,
                diff_percent,
            } => {
                if self.verbose {
                    writeln!(f, "compare failure")?;
                    super::write_line_diff(f, result, expected)
                } else {
                    write!(
                        f,
                        "{} ({:.0}%)",
                        Color::Blue.paint("compare failure"),
                        diff_percent * 100.0
                    )
                }
            }
        }
    }
}

impl Debug for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.printer(false).fmt(f)
    }
}
