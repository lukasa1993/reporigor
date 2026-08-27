use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::Path;

use analysis_crap::{
    analyze, apply_coverage, crap_score, load_coverage, load_coverage_as, normalize_path, parse_cobertura,
    parse_coverage_py_json, parse_istanbul_json, parse_lcov, parse_llvm_json, CoverageError, CoverageFormat,
    CoverageReport, MAX_COBERTURA_RESOLUTION_CANDIDATES, MAX_COBERTURA_SOURCES, MAX_COBERTURA_XML_ATTRIBUTES,
    MAX_COBERTURA_XML_DEPTH, MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS, MAX_COVERAGE_DISCOVERY_BYTES,
    MAX_COVERAGE_REPORT_BYTES, MAX_LLVM_EXPANDED_LINES, MAX_LLVM_REGION_LINES,
};
use reporigor_core::{FunctionRecord, Language};

fn function(file: &str, name: &str, start: u32, end: u32, complexity: u32) -> FunctionRecord {
    FunctionRecord {
        language: Language::Python,
        name: name.to_owned(),
        file: file.to_owned(),
        start_line: start,
        end_line: end,
        complexity,
        coverage: None,
        crap: None,
    }
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

#[test]
fn exact_crap_formula_is_shared_across_languages() {
    assert_close(crap_score(10, 0.0), 110.0);
    assert_close(crap_score(10, 50.0), 22.5);
    assert_close(crap_score(10, 100.0), 10.0);
}

#[test]
fn paths_are_normalized_lexically_and_cross_platform() {
    assert_eq!(normalize_path("./src//a/../b.py"), "src/b.py");
    assert_eq!(normalize_path(r"C:\Repo\.\SRC\..\main.py"), "c:/repo/main.py");
    assert_eq!(normalize_path("file:///tmp/project/a.py"), "/tmp/project/a.py");
    assert_eq!(normalize_path("/tmp/project/../../a.py"), "/a.py");
}

#[test]
fn lcov_merges_repeated_records_by_maximum_hits() -> Result<(), Box<dyn Error>> {
    let report = parse_lcov(
        "TN:\nSF:./src/main.py\nDA:1,0\nDA:2,2.9\nend_of_record\n\
         SF:src/main.py\nDA:1,4\nDA:0,99\nend_of_record\n",
    )?;
    assert_eq!(report.format(), CoverageFormat::Lcov);
    assert_eq!(report.file_count(), 1);
    assert_eq!(report.line_hits("src/main.py", 1), Some(4));
    assert_eq!(report.line_hits("src/main.py", 2), Some(2));
    assert_eq!(report.line_hits("src/main.py", 0), None);
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
    assert_eq!(report.format(), CoverageFormat::Cobertura);
    assert_eq!(report.line_hits("src/main.c", 4), Some(3));
    assert_eq!(report.line_hits("src/main.c", 5), Some(2));
    assert_eq!(report.line_hits("/workspace/src/main.c", 4), Some(3));
    Ok(())
}

#[test]
fn cobertura_rejects_duplicate_attributes() {
    let error = expect_coverage_error(parse_cobertura(
        r#"<coverage><class filename="src/main.c"><line number="4" number="5" hits="1"/></class></coverage>"#,
    ));
    match error {
        CoverageError::Parse { format, message } => {
            assert_eq!(format, "cobertura");
            assert!(message.contains("redefined") || message.contains("duplicate XML attribute"));
        }
        other => panic!("expected duplicate-attribute parse error, got {other}"),
    }
}

#[test]
fn cobertura_bounds_attributes_and_namespace_declarations_per_element() -> Result<(), Box<dyn Error>> {
    let mut xml = String::from("<coverage");
    for index in 0..=MAX_COBERTURA_XML_ATTRIBUTES {
        write!(&mut xml, " xmlns:n{index}=\"urn:{index}\"")?;
    }
    xml.push_str("></coverage>");

    let error = expect_coverage_error(parse_cobertura(&xml));
    assert_resource_limit(error, "Cobertura XML attributes per element");
    Ok(())
}

#[test]
fn cobertura_rejects_dtds_before_entity_processing() {
    let error = expect_coverage_error(parse_cobertura(
        r#"<!DOCTYPE coverage [<!ENTITY payload "boom">]><coverage><source>&payload;</source></coverage>"#,
    ));
    match error {
        CoverageError::Parse { format, message } => {
            assert_eq!(format, "cobertura");
            assert!(message.contains("DTD"));
        }
        other => panic!("expected DTD parse error, got {other}"),
    }
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
    assert_eq!(report.format(), CoverageFormat::CoveragePy);
    assert_eq!(report.line_hits("pkg/a.py", 2), Some(1));
    assert_eq!(report.line_hits("pkg/a.py", 3), Some(1));
    assert_eq!(report.line_hits("pkg/a.py", 4), Some(0));
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
    assert_eq!(report.format(), CoverageFormat::Istanbul);
    assert_eq!(report.line_hits("/repo/src/key.ts", 10), Some(5));
    assert_eq!(report.line_hits("/repo/src/key.ts", 14), Some(0));
    assert_eq!(report.line_hits("/old/key.ts", 10), None);
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
    assert_eq!(report.format(), CoverageFormat::Llvm);
    assert_eq!(report.line_hits("/repo/src/lib.swift", 2), Some(3));
    assert_eq!(report.line_hits("/repo/src/lib.swift", 4), Some(3));
    assert_eq!(report.line_hits("/repo/src/other.swift", 8), Some(0));
    assert_eq!(report.line_hits("/repo/src/other.swift", 9), Some(4));
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
    for index in 0..MAX_COBERTURA_SOURCES {
        write!(&mut xml, "<source>/workspace/{index}</source>")?;
    }
    xml.push_str("</sources><packages><package><classes><class filename=\"src/main.c\"><lines>");
    for line in 1..=line_count {
        write!(&mut xml, "<line number=\"{line}\" hits=\"1\"/>")?;
    }
    xml.push_str("</lines></class></classes></package></packages></coverage>");

    let error = expect_coverage_error(parse_cobertura(&xml));
    assert_resource_limit(error, "Cobertura source-resolution candidates");
    Ok(())
}

#[test]
fn loading_a_directory_discovers_and_detects_a_report() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("nested"))?;
    fs::write(
        directory.path().join("coverage.json"),
        r#"{"files":{"src/a.py":{"executed_lines":[1],"missing_lines":[2]}}}"#,
    )?;
    fs::write(
        directory.path().join("nested/lcov.info"),
        "SF:other.py\nDA:1,1\nend_of_record\n",
    )?;
    let report = load_coverage(directory.path())?;
    assert_eq!(report.format(), CoverageFormat::CoveragePy);
    assert_eq!(report.line_hits("src/a.py", 1), Some(1));
    Ok(())
}

#[test]
fn sparse_oversized_coverage_file_is_rejected_from_metadata() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coverage.json");
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
    if Path::new("/dev/zero").exists() {
        assert!(matches!(
            load_coverage_as(Path::new("/dev/zero"), CoverageFormat::Lcov),
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
    let mut report = CoverageReport::new(CoverageFormat::Lcov);
    report.insert_line("/repo/src/main.py", 2, 1);
    report.insert_line("/repo/src/main.py", 3, 0);
    report.insert_line("/repo/src/main.py", 4, 7);
    report.insert_line("/repo/src/main.py", 20, 1);

    let mut functions = vec![
        function("src/main.py", "covered", 1, 5, 3),
        function("src/main.py", "no-lines", 6, 8, 2),
        function("src/missing.py", "missing", 1, 2, 1),
    ];
    let application = apply_coverage(root, &mut functions, &report);
    assert_eq!(application.total_functions, 3);
    assert_eq!(application.matched_functions, 1);
    assert_eq!(application.empty_ranges, 1);
    assert_eq!(application.unmatched_functions, 1);
    assert_eq!(application.missing_functions(), 2);

    assert_close(functions[0].coverage.unwrap_or_default(), 200.0 / 3.0);
    assert_close(functions[0].crap.unwrap_or_default(), crap_score(3, 200.0 / 3.0));
    assert_eq!(functions[1].coverage, None);
    assert_eq!(functions[2].crap, None);
}

#[test]
fn ambiguous_basename_fallback_does_not_cross_wire_files() {
    let mut report = CoverageReport::new(CoverageFormat::Lcov);
    report.insert_line("/one/src/index.ts", 1, 1);
    report.insert_line("/two/lib/index.ts", 1, 0);
    assert!(report.lines_for_file(Path::new("/unknown"), "index.ts").is_none());
}

#[test]
fn analysis_sorts_scored_functions_first_by_descending_risk() {
    let mut report = CoverageReport::new(CoverageFormat::Lcov);
    report.insert_line("a.py", 1, 1);
    report.insert_line("b.py", 1, 0);
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
