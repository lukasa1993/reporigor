use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use analysis_crap::{
    analyze, apply_coverage, apply_coverage_with_policy, crap_score, discover_coverage_report, load_coverage,
    load_coverage_as, normalize_path, parse_cobertura, parse_coverage, parse_coverage_py_json,
    parse_istanbul_json, parse_lcov, parse_llvm_json, CoverageApplication, CoverageError, CoverageFormat,
    CoverageReport, MAX_COBERTURA_RESOLUTION_CANDIDATES, MAX_COBERTURA_SOURCES, MAX_COBERTURA_XML_ATTRIBUTES,
    MAX_COBERTURA_XML_DEPTH, MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS, MAX_COVERAGE_DISCOVERY_BYTES,
    MAX_COVERAGE_REPORT_BYTES, MAX_LLVM_EXPANDED_LINES, MAX_LLVM_REGION_LINES,
};
use reporigor_core::{CoverageSpan, FunctionRecord, Language};

type LineExpectation = (u32, Option<u64>);
type FileExpectation<'a> = (&'a str, &'a [LineExpectation]);
type FunctionSpec<'a> = (&'a str, &'a str, u32, u32, u32);
type SpanFunctionSpec<'a> = (&'a str, &'a str, std::ops::RangeInclusive<u32>, u32, CoverageSpan);

fn function(file: &str, name: &str, start: u32, end: u32, complexity: u32) -> FunctionRecord {
    FunctionRecord::new(Language::Python, name, file, start, end, complexity)
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1.0e-9,
        "expected {expected}, got {actual}"
    );
}

fn assert_resource_limit(error: CoverageError, expected_resource: &str) {
    match error {
        CoverageError::ResourceLimit { resource, .. } => assert_eq!(resource, expected_resource),
        other => panic!("expected {expected_resource} resource limit, got {other}"),
    }
}

fn expect_coverage_error<T>(result: Result<T, CoverageError>) -> CoverageError {
    match result {
        Ok(_) => panic!("expected coverage operation to fail"),
        Err(error) => error,
    }
}

fn assert_parse_error<T>(result: Result<T, CoverageError>, format: &str, fragments: &[&str]) {
    match expect_coverage_error(result) {
        CoverageError::Parse {
            format: actual,
            message,
        } => {
            assert_eq!(actual, format);
            assert!(fragments.iter().any(|fragment| message.contains(fragment)));
        }
        other => panic!("expected {format} parse error, got {other}"),
    }
}

fn report_with_lines(format: CoverageFormat, lines: &[(&str, u32, u64)]) -> CoverageReport {
    let mut report = CoverageReport::new(format);
    for (file, line, hits) in lines {
        report.insert_line(file, *line, *hits);
    }
    report
}

fn span(start_line: u32, start_column: u32, end_line: u32, end_column: u32) -> CoverageSpan {
    CoverageSpan {
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn assert_line_hits(report: &CoverageReport, file: &str, expected: &[LineExpectation]) {
    for (line, hits) in expected {
        assert_eq!(report.line_hits(file, *line), *hits);
    }
}

fn assert_report_lines(report: &CoverageReport, format: CoverageFormat, expected: &[FileExpectation<'_>]) {
    assert_eq!(report.format(), format);
    for (file, lines) in expected {
        assert_line_hits(report, file, lines);
    }
}

fn single_file_report(format: CoverageFormat, file: &str, lines: &[(u32, u64)]) -> CoverageReport {
    let entries = lines
        .iter()
        .map(|(line, hits)| (file, *line, *hits))
        .collect::<Vec<_>>();
    report_with_lines(format, &entries)
}

fn function_with_span(
    file: &str,
    name: &str,
    lines: std::ops::RangeInclusive<u32>,
    complexity: u32,
    coverage_span: CoverageSpan,
) -> FunctionRecord {
    let mut record = function(file, name, *lines.start(), *lines.end(), complexity);
    record.coverage_span = coverage_span;
    record
}

fn functions_from_specs(specs: &[FunctionSpec<'_>]) -> Vec<FunctionRecord> {
    specs
        .iter()
        .map(|(file, name, start, end, complexity)| function(file, name, *start, *end, *complexity))
        .collect()
}

fn functions_with_spans(specs: Vec<SpanFunctionSpec<'_>>) -> Vec<FunctionRecord> {
    specs
        .into_iter()
        .map(|(file, name, lines, complexity, coverage_span)| {
            function_with_span(file, name, lines, complexity, coverage_span)
        })
        .collect()
}

fn excluded_function(
    name: &str,
    lines: std::ops::RangeInclusive<u32>,
    complexity: u32,
    excluded: (u32, u32),
) -> FunctionRecord {
    let mut record = function("src/main.py", name, *lines.start(), *lines.end(), complexity);
    record.coverage_excluded_ranges = vec![excluded];
    record
}

fn apply_fixture(
    mut functions: Vec<FunctionRecord>,
    report: &CoverageReport,
    expected: (usize, usize, usize, usize, usize),
) -> Vec<FunctionRecord> {
    let application = apply_coverage(Path::new("."), &mut functions, report);
    assert_application(application, expected);
    functions
}

fn temporary_path(name: &str) -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(name);
    Ok((directory, path))
}

fn write_temporary_file(name: &str, contents: &str) -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let (directory, path) = temporary_path(name)?;
    fs::write(&path, contents)?;
    Ok((directory, path))
}

fn two_line_report(format: CoverageFormat, lines: [(&str, u32, u64); 2]) -> CoverageReport {
    report_with_lines(format, &lines)
}

fn assert_cobertura_resource_limit(xml: &str, resource: &str) {
    assert_resource_limit(expect_coverage_error(parse_cobertura(xml)), resource);
}

#[derive(Clone, Copy)]
enum ParsedFixture {
    Cobertura,
    Istanbul,
    Llvm,
}

fn assert_parsed_fixture(report: &CoverageReport, fixture: ParsedFixture) {
    let (format, files) = fixture.identity();
    assert_report_lines(
        report,
        format,
        &[
            (files[0], fixture.first_lines()),
            (files[1], fixture.second_lines()),
        ],
    );
}

impl ParsedFixture {
    const fn identity(self) -> (CoverageFormat, [&'static str; 2]) {
        match self {
            Self::Cobertura => (CoverageFormat::Cobertura, ["src/main.c", "/workspace/src/main.c"]),
            Self::Istanbul => (CoverageFormat::Istanbul, ["/repo/src/key.ts", "/old/key.ts"]),
            Self::Llvm => (
                CoverageFormat::Llvm,
                ["/repo/src/lib.swift", "/repo/src/other.swift"],
            ),
        }
    }

    const fn first_lines(self) -> &'static [LineExpectation] {
        const LINES: [&[LineExpectation]; 3] = [
            &[(4, Some(3)), (5, Some(2))],
            &[(10, Some(5)), (14, Some(0))],
            &[(2, Some(3)), (4, Some(3))],
        ];
        LINES[self as usize]
    }

    fn second_lines(self) -> &'static [LineExpectation] {
        match self {
            Self::Cobertura => &[(4, Some(3))],
            Self::Istanbul => &[(10, None)],
            Self::Llvm => llvm_other_lines(),
        }
    }
}

fn main_py_report(file: &str, lines: &[(u32, u64)]) -> CoverageReport {
    single_file_report(CoverageFormat::Lcov, file, lines)
}

fn llvm_other_lines() -> &'static [LineExpectation] {
    &[(8, Some(0)), (9, Some(4))]
}

#[derive(Clone, Copy)]
enum NumberedXml {
    CoveredLine,
    Source,
}

fn append_numbered_xml(
    xml: &mut String,
    indices: std::ops::Range<usize>,
    element: NumberedXml,
) -> Result<(), std::fmt::Error> {
    for index in indices {
        let fragment = match element {
            NumberedXml::CoveredLine => format!("<line number=\"{index}\" hits=\"1\"/>"),
            NumberedXml::Source => format!("<source>/workspace/{index}</source>"),
        };
        xml.write_str(&fragment)?;
    }
    Ok(())
}

fn assert_coverages(functions: &[FunctionRecord], expected: &[Option<f64>]) {
    assert_eq!(
        functions
            .iter()
            .map(|function| function.coverage)
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_application(application: CoverageApplication, expected: (usize, usize, usize, usize, usize)) {
    assert_eq!(
        (
            application.total_functions,
            application.matched_functions,
            application.unmatched_functions,
            application.empty_ranges,
            application.ambiguous_functions,
        ),
        expected
    );
}

#[test]
fn exact_crap_formula_is_shared_across_languages() {
    for (coverage, expected) in [(0.0, 110.0), (50.0, 22.5), (100.0, 10.0)] {
        assert_close(crap_score(10, coverage), expected);
    }
}

#[test]
fn paths_are_normalized_lexically_and_cross_platform() {
    const CASES: &str = concat!(
        "./src//a/../b.py\tsrc/b.py\n",
        "C:\\Repo\\.\\SRC\\..\\main.py\tc:/repo/main.py\n",
        "file:///tmp/project/a.py\t/tmp/project/a.py\n",
        "/tmp/project/../../a.py\t/a.py\n",
        "\t.\n",
        "../src/../main.py\t../main.py\n",
        "//SERVER/Share/../main.py\t//server/main.py\n",
        "C:\\\tc:/",
    );
    for case in CASES.lines() {
        let (input, expected) = case
            .split_once('\t')
            .unwrap_or_else(|| panic!("invalid path fixture: {case}"));
        assert_eq!(normalize_path(input), expected);
    }
}

#[test]
fn coverage_format_labels_and_explicit_dispatch_cover_every_variant() -> Result<(), Box<dyn Error>> {
    let cases = [
        (CoverageFormat::Lcov, "lcov", "SF:a.rs\nDA:1,1\n"),
        (
            CoverageFormat::Cobertura,
            "cobertura",
            r#"<coverage><class filename="a.rs"><line number="1" hits="1"/></class></coverage>"#,
        ),
        (
            CoverageFormat::CoveragePy,
            "coverage.py-json",
            r#"{"files":{"a.py":{"executed_lines":[1]}}}"#,
        ),
        (
            CoverageFormat::Istanbul,
            "istanbul-json",
            r#"{"a.js":{"statementMap":{"0":{"start":{"line":1}}},"s":{"0":1}}}"#,
        ),
        (
            CoverageFormat::Llvm,
            "llvm-export-json",
            r#"{"data":[{"files":[{"filename":"a.rs","segments":[[1,1,1,true]]}]}]}"#,
        ),
    ];
    for (format, label, input) in cases {
        assert_eq!(format.as_str(), label);
        assert_eq!(format.to_string(), label);
        assert!(!parse_coverage(format, input)?.is_empty());
    }
    assert_eq!(CoverageFormat::Merged.as_str(), "merged");
    assert!(matches!(
        parse_coverage(CoverageFormat::Merged, "{}"),
        Err(CoverageError::Unsupported(_))
    ));
    Ok(())
}

#[test]
fn lcov_merges_repeated_records_by_maximum_hits() -> Result<(), Box<dyn Error>> {
    let report = parse_lcov(
        "TN:\nSF:./src/main.py\nDA:1,0\nDA:2,2.9\nend_of_record\n\
         SF:src/main.py\nDA:1,4\nDA:0,99\nend_of_record\n",
    )?;
    assert_eq!(report.file_count(), 1);
    assert_report_lines(
        &report,
        CoverageFormat::Lcov,
        &[("src/main.py", &[(1, Some(4)), (2, Some(2)), (0, None)])],
    );
    Ok(())
}

#[test]
fn cobertura_supports_sources_floats_and_duplicate_classes() -> Result<(), Box<dyn Error>> {
    let report = parse_cobertura(
        r#"<?xml version="1.0" ?>
        <coverage>
          <sources><source>/workspace</source></sources>
          <packages><package><classes>
            <class filename="src/main.c"><lines>
              <line number="4" hits="0"/>
              <line number="5" hits="2.7"/>
            </lines></class>
            <class filename="./src/main.c"><lines>
              <line number="4" hits="3"/>
            </lines></class>
          </classes></package></packages>
        </coverage>"#,
    )?;
    assert_parsed_fixture(&report, ParsedFixture::Cobertura);
    Ok(())
}

#[test]
fn cobertura_rejects_duplicate_attributes() {
    assert_parse_error(
        parse_cobertura(
            r#"<coverage><class filename="src/main.c"><line number="4" number="5" hits="1"/></class></coverage>"#,
        ),
        "cobertura",
        &["redefined", "duplicate XML attribute"],
    );
}

#[test]
fn cobertura_bounds_attributes_and_namespace_declarations_per_element() -> Result<(), Box<dyn Error>> {
    let mut namespaces = String::new();
    for index in 0..MAX_COBERTURA_XML_ATTRIBUTES.saturating_add(1) {
        write!(&mut namespaces, " xmlns:n{index}=\"urn:{index}\"")?;
    }
    let xml = format!("<coverage{namespaces}></coverage>");
    assert_cobertura_resource_limit(&xml, "Cobertura XML attributes per element");
    Ok(())
}

#[test]
fn cobertura_rejects_dtds_before_entity_processing() {
    assert_parse_error(
        parse_cobertura(
            r#"<!DOCTYPE coverage [<!ENTITY payload "boom">]><coverage><source>&payload;</source></coverage>"#,
        ),
        "cobertura",
        &["DTD"],
    );
}

#[test]
fn cobertura_preflights_comments_cdata_and_sparse_class_events() -> Result<(), Box<dyn Error>> {
    let report = parse_cobertura(
        r#"<?xml version="1.0"?>
        <coverage><!-- bounded comment -->
          <sources><source><![CDATA[/workspace]]></source><source/></sources>
          <line number="99" hits="8"/>
          <class filename="/absolute.rs"><line number="1"/></class>
          <class><line number="2" hits="4"/></class>
        </coverage>"#,
    )?;
    assert_eq!(report.line_hits("/absolute.rs", 1), Some(0));
    assert_eq!(report.line_hits("/workspace/absolute.rs", 1), None);

    for malformed in ["<coverage><!--", "<coverage><![CDATA[", "<?coverage"] {
        assert!(matches!(
            parse_cobertura(malformed),
            Err(CoverageError::Parse { .. })
        ));
    }
    Ok(())
}

#[test]
fn cobertura_bounds_aggregate_namespaces_before_stream_parsing() -> Result<(), Box<dyn Error>> {
    let mut xml = String::from("<coverage>");
    let mut remaining = MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS + 1;
    let mut element = 0_usize;
    while remaining > 0 {
        write!(&mut xml, "<n{element}")?;
        let declarations = remaining.min(MAX_COBERTURA_XML_ATTRIBUTES);
        for local in 0..declarations {
            write!(&mut xml, " xmlns:x{element}_{local}=\"urn:x\"")?;
        }
        xml.push('>');
        remaining -= declarations;
        element += 1;
    }

    let error = expect_coverage_error(parse_cobertura(&xml));
    assert_resource_limit(error, "Cobertura XML namespace declarations");
    Ok(())
}

#[test]
fn cobertura_bounds_element_depth_before_stream_parsing() {
    let mut xml = String::from("<coverage>");
    for _ in 0..MAX_COBERTURA_XML_DEPTH {
        xml.push_str("<nested>");
    }
    let error = expect_coverage_error(parse_cobertura(&xml));
    assert_resource_limit(error, "Cobertura XML element depth");
}

#[test]
fn coverage_py_json_maps_executed_and_missing_lines() -> Result<(), Box<dyn Error>> {
    let report = parse_coverage_py_json(
        r#"{
          "meta": {"version": "7"},
          "files": {
            "pkg/a.py": {
              "executed_lines": [2, 3],
              "missing_lines": [3, 4]
            }
          }
        }"#,
    )?;
    assert!(matches!(report.format(), CoverageFormat::CoveragePy));
    assert_line_hits(&report, "pkg/a.py", &[(2, Some(1)), (3, Some(1)), (4, Some(0))]);
    Ok(())
}

#[test]
fn istanbul_json_uses_statement_start_lines_and_path() -> Result<(), Box<dyn Error>> {
    let report = parse_istanbul_json(
        r#"{
          "ignored": {"anything": true},
          "/old/key.ts": {
            "path": "/repo/src/key.ts",
            "statementMap": {
              "0": {"start": {"line": 10, "column": 0}, "end": {"line": 12, "column": 1}},
              "1": {"start": {"line": 14, "column": 0}, "end": {"line": 14, "column": 1}}
            },
            "s": {"0": 5, "1": 0}
          }
        }"#,
    )?;
    assert_parsed_fixture(&report, ParsedFixture::Istanbul);
    Ok(())
}

#[test]
fn llvm_export_json_maps_code_regions_and_segments() -> Result<(), Box<dyn Error>> {
    let report = parse_llvm_json(
        r#"{
          "type": "llvm.coverage.json.export",
          "data": [{
            "functions": [{
              "filenames": ["/repo/src/lib.swift"],
              "regions": [
                [2, 1, 4, 2, 3, 0, 0, 0],
                [3, 1, 3, 9, 0, 0, 0, 1]
              ]
            }],
            "files": [{
              "filename": "/repo/src/other.swift",
              "segments": [[8, 1, 0, true, true, false], [9, 1, 4, true, true, false]]
            }]
          }]
        }"#,
    )?;
    assert_parsed_fixture(&report, ParsedFixture::Llvm);
    Ok(())
}

#[test]
fn llvm_function_regions_assign_same_line_functions_and_unemitted_code() -> Result<(), Box<dyn Error>> {
    let report = parse_llvm_json(
        r#"{
          "type": "llvm.coverage.json.export",
          "data": [{
            "functions": [
              {"filenames": ["src/lib.rs"], "regions": [[3, 1, 3, 10, 1, 0, 0, 0]]},
              {"filenames": ["src/lib.rs"], "regions": [[2, 20, 2, 25, 0, 0, 0, 0]]},
              {"filenames": ["src/lib.rs"], "regions": [[6, 5, 6, 15, 2, 0, 0, 0]]},
              {"filenames": ["src/lib.rs"], "regions": [[6, 25, 6, 35, 0, 0, 0, 0]]}
            ],
            "files": [{
              "filename": "src/lib.rs",
              "segments": [[2, 1, 1, true, true, false], [3, 1, 1, true, true, false],
                           [6, 5, 2, true, true, false], [6, 25, 0, true, true, false]]
            }]
          }]
        }"#,
    )?;

    let mut outer = function_with_span("src/lib.rs", "outer", 1..=4, 2, span(1, 1, 4, 2));
    outer.coverage_excluded_ranges = vec![(2, 2)];
    outer.coverage_excluded_spans = vec![span(2, 20, 2, 25)];
    let mut regional = functions_with_spans(vec![
        ("src/lib.rs", "left", 6..=6, 2, span(6, 1, 6, 20)),
        ("src/lib.rs", "right", 6..=6, 2, span(6, 21, 6, 40)),
        ("src/lib.rs", "generic", 8..=9, 2, span(8, 1, 9, 2)),
    ]);
    let absent_from_explicit_scope = function("scripts/tool.sh", "run", 1, 3, 2);
    let mut functions = vec![outer];
    functions.append(&mut regional);
    functions.push(absent_from_explicit_scope);
    let application = apply_coverage_with_policy(Path::new("."), &mut functions, &report, true);

    assert_eq!(application.matched_functions, 5);
    assert_eq!(application.missing_functions(), 0);
    assert_coverages(
        &functions,
        &[Some(100.0), Some(100.0), Some(0.0), Some(0.0), Some(0.0)],
    );

    let mut strict = vec![function("scripts/tool.sh", "run", 1, 3, 2)];
    let strict_application = apply_coverage(Path::new("."), &mut strict, &report);
    assert_eq!(strict_application.unmatched_functions, 1);
    assert_eq!(strict[0].coverage, None);
    Ok(())
}

#[test]
fn llvm_function_region_order_is_irrelevant() -> Result<(), Box<dyn Error>> {
    let first = parse_llvm_json(
        r#"{"data":[{"functions":[
          {"filenames":["src/lib.rs"],"regions":[[2,1,2,8,1,0,0,0],[3,1,3,8,0,0,0,0]]},
          {"filenames":["src/lib.rs"],"regions":[[6,1,6,8,4,0,0,0,0]]}
        ]}]}"#,
    )?;
    let second = parse_llvm_json(
        r#"{"data":[{"functions":[
          {"filenames":["src/lib.rs"],"regions":[[6,1,6,8,4,0,0,0,0]]},
          {"filenames":["src/lib.rs"],"regions":[[3,1,3,8,0,0,0,0],[2,1,2,8,1,0,0,0]]}
        ]}]}"#,
    )?;
    assert_eq!(first, second);

    let [left, right] = [
        ("left", 1..=4, span(1, 1, 4, 1)),
        ("right", 5..=7, span(5, 1, 7, 1)),
    ]
    .map(|(name, lines, coverage_span)| function_with_span("src/lib.rs", name, lines, 2, coverage_span));
    let mut forward = vec![left.clone(), right.clone()];
    let mut reverse = vec![right, left];
    apply_coverage(Path::new("."), &mut forward, &first);
    apply_coverage(Path::new("."), &mut reverse, &second);
    let scores = |functions: &[FunctionRecord]| {
        functions
            .iter()
            .map(|function| (function.name.clone(), function.coverage))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(scores(&forward), scores(&reverse));
    Ok(())
}

#[test]
fn llvm_regions_crossing_function_or_nested_boundaries_are_unassigned() -> Result<(), Box<dyn Error>> {
    let report = parse_llvm_json(
        r#"{"data":[{"functions":[
          {"filenames":["src/outer.rs"],"regions":[[2,1,5,1,1,0,0,0]]},
          {"filenames":["src/nested.rs"],"regions":[[2,1,3,10,1,0,0,0]]}
        ]}]}"#,
    )?;
    let mut crossing = function("src/outer.rs", "crossing", 1, 4, 2);
    crossing.coverage_span = span(1, 1, 4, 2);
    let mut nested = function("src/nested.rs", "nested", 1, 4, 2);
    nested.coverage_span = crossing.coverage_span;
    nested.coverage_excluded_spans = vec![span(3, 1, 3, 5)];
    let mut functions = vec![crossing, nested];
    let application = apply_coverage(Path::new("."), &mut functions, &report);
    assert_eq!(application.matched_functions, 0);
    assert_eq!(application.empty_ranges, 2);
    assert!(functions.iter().all(|function| function.coverage.is_none()));
    Ok(())
}

#[test]
fn llvm_u32_max_region_is_rejected_before_line_expansion() {
    let error = expect_coverage_error(parse_llvm_json(
        r#"{
          "data": [{
            "functions": [{
              "filenames": ["src/lib.rs"],
              "regions": [[1, 1, 4294967295, 1, 1, 0, 0, 0]]
            }]
          }]
        }"#,
    ));
    assert_resource_limit(error, "LLVM executable lines per region");
}

#[test]
fn llvm_aggregate_expansion_is_preflighted_before_any_region_is_expanded() {
    let region_count = MAX_LLVM_EXPANDED_LINES / MAX_LLVM_REGION_LINES + 1;
    let regions: Vec<_> = (0..region_count)
        .map(|_| serde_json::json!([1, 1, MAX_LLVM_REGION_LINES, 1, 1, 0, 0, 0]))
        .collect();
    let report = serde_json::json!({
        "data": [{
            "functions": [{
                "filenames": ["src/lib.rs"],
                "regions": regions
            }]
        }]
    });

    let error = expect_coverage_error(parse_llvm_json(&report.to_string()));
    assert_resource_limit(error, "LLVM expanded executable lines");
}

#[test]
fn cobertura_source_class_cross_product_is_rejected_before_alias_generation() -> Result<(), Box<dyn Error>> {
    let line_count = MAX_COBERTURA_RESOLUTION_CANDIDATES / (MAX_COBERTURA_SOURCES + 1) + 1;
    let mut xml = String::from("<coverage><sources>");
    append_numbered_xml(&mut xml, 0..MAX_COBERTURA_SOURCES, NumberedXml::Source)?;
    xml.push_str("</sources><packages><package><classes><class filename=\"src/main.c\"><lines>");
    append_numbered_xml(
        &mut xml,
        1..line_count.saturating_add(1),
        NumberedXml::CoveredLine,
    )?;
    xml.push_str("</lines></class></classes></package></packages></coverage>");
    assert_cobertura_resource_limit(&xml, "Cobertura source-resolution candidates");
    Ok(())
}

#[test]
fn loading_a_directory_discovers_and_detects_a_report() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    write_discoverable_coverage_fixture(directory.path())?;
    let report = load_coverage(directory.path())?;
    assert_eq!(report.format(), CoverageFormat::CoveragePy);
    assert_eq!(report.line_hits("src/a.py", 1), Some(1));
    Ok(())
}

fn write_discoverable_coverage_fixture(directory: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(directory.join("nested"))?;
    fs::write(
        directory.join("coverage.json"),
        r#"{"files":{"src/a.py":{"executed_lines":[1],"missing_lines":[2]}}}"#,
    )?;
    fs::write(
        directory.join("nested/lcov.info"),
        "SF:other.py\nDA:1,1\nend_of_record\n",
    )?;
    Ok(())
}

#[test]
fn discovery_classifies_direct_missing_and_empty_paths() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let direct = directory.path().join("custom.data");
    fs::write(&direct, "SF:src/a.rs\nDA:1,1\nend_of_record\n")?;

    assert_eq!(discover_coverage_report(&direct)?, direct);
    assert_eq!(load_coverage(&direct)?.line_hits("src/a.rs", 1), Some(1));
    assert!(matches!(
        discover_coverage_report(&directory.path().join("missing")),
        Err(CoverageError::Missing(_))
    ));

    let empty = tempfile::tempdir()?;
    assert!(matches!(
        discover_coverage_report(empty.path()),
        Err(CoverageError::NotFound(_))
    ));
    Ok(())
}

#[test]
fn loading_rejects_reports_without_executable_lines() -> Result<(), Box<dyn Error>> {
    let (_directory, path) = write_temporary_file("empty.info", "TN:\nend_of_record\n")?;
    assert!(matches!(load_coverage(&path), Err(CoverageError::Empty(_))));
    Ok(())
}

#[test]
fn sparse_oversized_coverage_file_is_rejected_from_metadata() -> Result<(), Box<dyn Error>> {
    let (_directory, path) = temporary_path("coverage.json")?;
    let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
    file.set_len(MAX_COVERAGE_REPORT_BYTES + 1)?;

    let error = expect_coverage_error(load_coverage_as(&path, CoverageFormat::CoveragePy));
    assert_resource_limit(error, "coverage report bytes");
    Ok(())
}

#[test]
fn discovery_rejects_oversized_candidate_aggregate_from_metadata() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let candidate_size = MAX_COVERAGE_DISCOVERY_BYTES / 3 + 1;
    for index in 0..3 {
        let nested = directory.path().join(index.to_string());
        fs::create_dir(&nested)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(nested.join("coverage.json"))?;
        file.set_len(candidate_size)?;
    }

    let error = expect_coverage_error(load_coverage(directory.path()));
    assert_resource_limit(error, "coverage discovery candidate bytes");
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_and_device_coverage_inputs_are_rejected() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir()?;
    let report = directory.path().join("actual.json");
    fs::write(&report, r#"{"files":{"src/a.py":{"executed_lines":[1]}}}"#)?;
    let link = directory.path().join("coverage.json");
    symlink(&report, &link)?;

    assert!(matches!(
        load_coverage_as(&link, CoverageFormat::CoveragePy),
        Err(CoverageError::UnsafePath { .. })
    ));
    assert!(matches!(
        load_coverage(&link),
        Err(CoverageError::UnsafePath { .. })
    ));
    if Path::new("/dev/zero").exists() {
        assert!(matches!(
            load_coverage_as(Path::new("/dev/zero"), CoverageFormat::Lcov),
            Err(CoverageError::UnsafePath { .. })
        ));
        assert!(matches!(
            load_coverage(Path::new("/dev/zero")),
            Err(CoverageError::UnsafePath { .. })
        ));
    }
    Ok(())
}

#[test]
fn reports_merge_without_allowing_later_shards_to_erase_hits() {
    let mut first = CoverageReport::new(CoverageFormat::Lcov);
    first.insert_line("src/a.py", 1, 8);
    first.insert_line("src/a.py", 2, 0);
    let mut second = CoverageReport::new(CoverageFormat::CoveragePy);
    second.insert_line("./src/a.py", 1, 0);
    second.insert_line("src/a.py", 2, 2);
    first.merge(&second);
    assert_eq!(first.format(), CoverageFormat::Merged);
    assert_eq!(first.line_hits("src/a.py", 1), Some(8));
    assert_eq!(first.line_hits("src/a.py", 2), Some(2));
}

#[test]
fn coverage_is_applied_only_to_executable_lines_in_function_range() {
    let root = Path::new("/repo");
    let report = main_py_report("/repo/src/main.py", &[(2, 1), (3, 0), (4, 7), (20, 1)]);

    let mut functions = functions_from_specs(&[
        ("src/main.py", "covered", 1, 5, 3),
        ("src/main.py", "no-lines", 6, 8, 2),
        ("src/missing.py", "missing", 1, 2, 1),
    ]);
    let application = apply_coverage(root, &mut functions, &report);
    assert_application(application, (3, 1, 1, 1, 0));
    assert_eq!(application.missing_functions(), 2);

    assert_close(functions[0].coverage.unwrap_or_default(), 200.0 / 3.0);
    assert_close(functions[0].crap.unwrap_or_default(), crap_score(3, 200.0 / 3.0));
    assert_eq!(functions[1].coverage, None);
    assert_eq!(functions[2].crap, None);
}

#[test]
fn outer_coverage_excludes_nested_executable_ranges() {
    let lines = [(2, 1), (4, 0), (5, 0), (8, 1)];
    let report = main_py_report("src/main.py", &lines);

    let outer = excluded_function("outer", 1..=9, 2, (3, 6));
    let nested = function("src/main.py", "nested", 3, 6, 2);
    let functions = apply_fixture(vec![outer, nested], &report, (2, 2, 0, 0, 0));
    assert_coverages(&functions, &[Some(100.0), Some(0.0)]);
}

#[test]
fn shared_nested_boundary_line_is_explicitly_coverage_ambiguous() {
    let report = single_file_report(CoverageFormat::Lcov, "src/main.py", &[(2, 0), (3, 1)]);

    let outer = excluded_function("outer", 1..=3, 5, (2, 2));
    let functions = apply_fixture(vec![outer], &report, (1, 0, 0, 0, 1));
    assert_coverages(&functions, &[None]);
    assert_eq!(functions[0].crap, None);
}

#[test]
fn sibling_functions_on_one_executable_line_are_both_coverage_ambiguous() {
    let report = single_file_report(CoverageFormat::Lcov, "src/overloads.cpp", &[(1, 1)]);
    let overloads = [("convert(int)", 2), ("convert(double)", 3)];
    let functions = overloads
        .into_iter()
        .map(|(name, complexity)| function("src/overloads.cpp", name, 1, 1, complexity))
        .collect();

    let functions = apply_fixture(functions, &report, (2, 0, 0, 0, 2));
    assert!(functions.iter().all(|function| function.coverage.is_none()));
    assert!(functions.iter().all(|function| function.crap.is_none()));
}

#[test]
fn ambiguous_basename_fallback_does_not_cross_wire_files() {
    let report = report_with_lines(
        CoverageFormat::Lcov,
        &[("/one/src/index.ts", 1, 1), ("/two/lib/index.ts", 1, 0)],
    );
    assert!(report.lines_for_file(Path::new("/unknown"), "index.ts").is_none());
}

#[test]
fn analysis_sorts_scored_functions_first_by_descending_risk() {
    let report = two_line_report(CoverageFormat::Lcov, [("a.py", 1, 1), ("b.py", 1, 0)]);
    let result = analyze(
        Path::new("."),
        vec![
            function("missing.py", "missing", 1, 1, 30),
            function("a.py", "safe", 1, 1, 2),
            function("b.py", "risk", 1, 1, 5),
        ],
        Some(&report),
    );
    let names: Vec<_> = result
        .functions
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    assert_eq!(names, vec!["risk", "safe", "missing"]);
    assert_eq!(result.missing_coverage(), 1);
    assert_eq!(result.over_threshold(6.0), 1);
    assert_close(result.max_score().unwrap_or_default(), 30.0);
}
