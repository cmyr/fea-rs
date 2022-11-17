//! utilities for compiling and comparing ttx

use std::{
    collections::HashMap,
    convert::TryInto,
    env::temp_dir,
    ffi::OsStr,
    fmt::{Debug, Display, Write},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use crate::{Compilation, Diagnostic, GlyphIdent, GlyphMap, GlyphName, ParseTree};

use ansi_term::Color;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use font_types::Tag;
use write_fonts::{tables::maxp::Maxp, FontBuilder};

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

/// An environment variable that can be set to specify where to write generated files.
///
/// This can be set during debugging if you want to inspect the generated files.
static TEMP_DIR_ENV: &str = "TTX_TEMP_DIR";

/// The combined results of this set of tests
#[derive(Default, Serialize, Deserialize)]
pub struct Report {
    pub results: Vec<TestCase>,
}

#[derive(Default)]
struct ReportSummary {
    passed: u32,
    panic: u32,
    parse: u32,
    compile: u32,
    compare: u32,
    other: u32,
    sum_compare_perc: f64,
}

pub struct ResultsPrinter<'a> {
    verbose: bool,
    results: &'a Report,
}

pub struct ReportComparePrinter<'a> {
    old: &'a Report,
    new: &'a Report,
}

#[derive(Serialize, Deserialize)]
pub struct TestCase {
    pub path: PathBuf,
    pub reason: TestResult,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum TestResult {
    Success,
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
    reason: &'a TestResult,
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
pub fn run_all_tests(fonttools_data_dir: impl AsRef<Path>, filter: Option<&String>) -> Report {
    let glyph_map = make_glyph_map();
    let reverse_map = glyph_map.reverse_map();
    let reverse_map = reverse_map
        .into_iter()
        .map(|(id, glyph)| {
            (
                format!("glyph{:05}", id.to_u16()),
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

pub fn finalize_results(result: Vec<Result<PathBuf, TestCase>>) -> Report {
    let mut result = result
        .into_iter()
        .fold(Report::default(), |mut results, current| {
            match current {
                Err(e) => results.results.push(e),
                Ok(path) => results.results.push(TestCase {
                    path,
                    reason: TestResult::Success,
                }),
            }
            results
        });
    result.results.sort_unstable_by(|a, b| {
        (a.reason.sort_order(), &a.path).cmp(&(b.reason.sort_order(), &b.path))
    });
    result
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
) -> Result<PathBuf, TestCase> {
    match std::panic::catch_unwind(|| match try_parse_file(&path, Some(glyph_map)) {
        Err((node, errs)) => Err(TestCase {
            path: path.clone(),
            reason: TestResult::ParseFail(stringify_diagnostics(&node, &errs)),
        }),
        Ok(node) => match crate::compile(&node, glyph_map) {
            Err(errs) => Err(TestCase {
                path: path.clone(),
                reason: TestResult::CompileFail(stringify_diagnostics(&node, &errs)),
            }),
            Ok(result) => {
                let font_data = build_font(result, glyph_map);
                compare_ttx(&font_data, &path, reverse_map)
            }
        },
    }) {
        Err(_) => {
            return Err(TestCase {
                path,
                reason: TestResult::Panic,
            })
        }
        Ok(Err(e)) => return Err(e),
        Ok(Ok(_)) => (),
    };
    Ok(path)
}

fn build_font(compilation: Compilation, glyphs: &GlyphMap) -> Vec<u8> {
    let mut font = FontBuilder::default();
    let maxp = Maxp::new(glyphs.len().try_into().unwrap());
    font.add_table(Tag::new(b"maxp"), write_fonts::dump_table(&maxp).unwrap());
    compilation.apply(&mut font).unwrap();
    font.build()
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

fn get_temp_dir() -> PathBuf {
    match std::env::var(TEMP_DIR_ENV) {
        Ok(dir) => {
            let dir = PathBuf::from(dir);
            if !dir.exists() {
                std::fs::create_dir_all(&dir).unwrap();
            }
            dir
        }
        Err(_) => temp_dir(),
    }
}

fn get_temp_file_name(in_file: &Path) -> PathBuf {
    let stem = in_file.file_stem().unwrap().to_str().unwrap();
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    Path::new(&format!("{stem}_{millis}")).with_extension("ttf")
}

fn compare_ttx(
    font_data: &[u8],
    fea_path: &Path,
    reverse_map: &HashMap<String, String>,
) -> Result<(), TestCase> {
    let ttx_path = fea_path.with_extension("ttx");
    let expected_diff_path = fea_path.with_extension("expected_diff");
    assert!(ttx_path.exists());
    let temp_path = get_temp_dir().join(get_temp_file_name(fea_path));
    std::fs::write(&temp_path, &font_data).unwrap();

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
        return Err(TestCase {
            path: fea_path.into(),
            reason: TestResult::TtxFail {
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
        Err(TestCase {
            path: fea_path.into(),
            reason: TestResult::CompareFail {
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
) -> Result<(), TestCase> {
    let cmp_path = src_path.with_extension(cmp_ext);
    let expected = if cmp_path.exists() {
        std::fs::read_to_string(&cmp_path).expect("failed to read cmp_path")
    } else {
        String::new()
    };

    if expected != output {
        let diff_percent = compute_diff_percentage(&expected, output);
        return Err(TestCase {
            path: src_path.to_owned(),
            reason: TestResult::CompareFail {
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

fn compute_diff_percentage(left: &str, right: &str) -> f64 {
    let lines = diff::lines(left, right);
    let same = lines
        .iter()
        .filter(|l| matches!(l, diff::Result::Both { .. }))
        .count();
    let total = lines.len() as f64;
    let perc = (same as f64) / total;

    const PRECISION_SMUDGE: f64 = 10000.0;
    (perc * PRECISION_SMUDGE).trunc() / PRECISION_SMUDGE
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

impl Report {
    pub fn has_failures(&self) -> bool {
        self.results.iter().any(|r| !r.reason.is_success())
    }

    pub fn into_error(self) -> Result<(), Self> {
        if self.has_failures() {
            Err(self)
        } else {
            Ok(())
        }
    }

    pub fn printer(&self, verbose: bool) -> ResultsPrinter {
        ResultsPrinter {
            verbose,
            results: self,
        }
    }

    pub fn compare_printer<'a, 'b: 'a>(&'b self, old: &'a Report) -> ReportComparePrinter<'a> {
        ReportComparePrinter { old, new: self }
    }

    /// returns the number of chars in the widest path
    fn widest_path(&self) -> usize {
        self.results
            .iter()
            .map(|item| &item.path)
            .map(|p| p.file_name().unwrap().to_str().unwrap().chars().count())
            .max()
            .unwrap_or(0)
    }

    fn summary(&self) -> ReportSummary {
        let mut summary = ReportSummary::default();
        for item in &self.results {
            match &item.reason {
                TestResult::Success => summary.passed += 1,
                TestResult::Panic => summary.panic += 1,
                TestResult::ParseFail(_) => summary.parse += 1,
                TestResult::CompileFail(_) => summary.compile += 1,
                TestResult::UnexpectedSuccess | TestResult::TtxFail { .. } => summary.other += 1,
                TestResult::CompareFail { diff_percent, .. } => {
                    summary.compare += 1;
                    summary.sum_compare_perc += diff_percent;
                }
            }
        }
        summary
    }
}

impl TestResult {
    fn sort_order(&self) -> u8 {
        match self {
            Self::Success => 1,
            Self::Panic => 2,
            Self::ParseFail(_) => 3,
            Self::CompileFail(_) => 4,
            Self::UnexpectedSuccess => 6,
            Self::TtxFail { .. } => 10,
            Self::CompareFail { .. } => 50,
        }
    }

    fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    pub fn printer(&self, verbose: bool) -> ReasonPrinter {
        ReasonPrinter {
            reason: self,
            verbose,
        }
    }
}

impl std::fmt::Debug for ResultsPrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_impl(f, &self.results, None, self.verbose)
    }
}

impl std::fmt::Debug for ReportComparePrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        debug_impl(f, self.new, Some(self.old), false)
    }
}

struct OldResults<'a> {
    map: Option<HashMap<&'a Path, TestResult>>,
}

impl<'a> OldResults<'a> {
    fn new(report: Option<&'a Report>) -> Self {
        Self {
            map: report.map(|report| {
                report
                    .results
                    .iter()
                    .map(|test| (test.path.as_path(), test.reason.clone()))
                    .collect()
            }),
        }
    }

    fn get(&self, result: &TestCase) -> ComparePrinter {
        match self.map.as_ref() {
            None => ComparePrinter::NotComparing,
            Some(map) => match map.get(result.path.as_path()) {
                None => ComparePrinter::Missing,
                Some(prev) => match (prev, &result.reason) {
                    (
                        TestResult::CompareFail {
                            diff_percent: old, ..
                        },
                        TestResult::CompareFail {
                            diff_percent: new, ..
                        },
                    ) => {
                        if (old - new).abs() > f64::EPSILON {
                            ComparePrinter::PercChange((new - old) * 100.)
                        } else {
                            ComparePrinter::Same
                        }
                    }
                    (x, y) if x == y => ComparePrinter::Same,
                    (old, _) => ComparePrinter::Different(old.clone()),
                },
            },
        }
    }
}

enum ComparePrinter {
    // print nothing, we aren't comparing
    NotComparing,
    // this item didn't previously exist
    Missing,
    // no diff
    Same,
    /// we are both compare failures, with a percentage change
    PercChange(f64),
    /// we are some other difference
    Different(TestResult),
}

impl std::fmt::Display for ComparePrinter {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ComparePrinter::NotComparing => Ok(()),
            ComparePrinter::Missing => write!(f, "(new)"),
            ComparePrinter::Same => write!(f, "--"),
            ComparePrinter::PercChange(val) if val.is_sign_positive() => {
                write!(f, "{}", Color::Green.paint(format!("+{val:.2}")))
            }
            ComparePrinter::PercChange(val) => {
                write!(f, "{}", Color::Red.paint(format!("-{val:.2}")))
            }
            ComparePrinter::Different(reason) => write!(f, "{reason:?}"),
        }
    }
}

fn debug_impl(
    f: &mut std::fmt::Formatter,
    report: &Report,
    old: Option<&Report>,
    verbose: bool,
) -> std::fmt::Result {
    writeln!(f, "failed test cases")?;
    let path_pad = report.widest_path();
    let old_results = OldResults::new(old);

    for result in &report.results {
        let old = old_results.get(result);
        let file_name = result.path.file_name().unwrap().to_str().unwrap();
        writeln!(
            f,
            "{file_name:path_pad$}  {:<30}  {old}",
            result.reason.printer(verbose).to_string(),
        )?;
    }
    let summary = report.summary();
    let prefix = old.is_some().then_some("new: ").unwrap_or("");
    writeln!(f, "{prefix}{summary}")?;
    if let Some(old_summary) = old.map(Report::summary) {
        writeln!(f, "old: {old_summary}")?;
    }

    Ok(())
}

impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.printer(false).fmt(f)
    }
}

impl Display for ReasonPrinter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            TestResult::Success => write!(f, "{}", Color::Green.paint("success")),
            TestResult::Panic => write!(f, "{}", Color::Red.paint("panic")),
            TestResult::ParseFail(diagnostics) => {
                write!(f, "{}", Color::Purple.paint("parse failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            TestResult::CompileFail(diagnostics) => {
                write!(f, "{}", Color::Yellow.paint("compile failure"))?;
                if self.verbose {
                    write!(f, "\n{}", diagnostics)?;
                }
                Ok(())
            }
            TestResult::UnexpectedSuccess => {
                write!(f, "{}", Color::Yellow.paint("unexpected success"))
            }
            TestResult::TtxFail { code, std_err } => {
                write!(f, "ttx failure ({:?}) stderr:\n{}", code, std_err)
            }
            TestResult::CompareFail {
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

impl Debug for TestResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.printer(false).fmt(f)
    }
}

impl ReportSummary {
    fn total_items(&self) -> u32 {
        self.passed + self.panic + self.parse + self.compile + self.compare + self.other
    }

    fn average_diff_percent(&self) -> f64 {
        (self.sum_compare_perc + (self.passed as f64)) / self.total_items() as f64 * 100.
    }
}

impl Display for ReportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.total_items();
        let perc = self.average_diff_percent();
        let ReportSummary {
            passed,
            panic,
            parse,
            compile,
            ..
        } = self;
        write!(f, "passed {passed}/{total} tests: ({panic} panics {parse} unparsed {compile} compile) {perc:.2}% avg diff")
    }
}
