//! End to end checks for visaparse report destinations.

use std::path::Path;
use std::process::Command;

use cli_tools_common::test_support::TempTree;

/// Run visaparse with a named workbook and check that the matching CSV is written beside it.
#[expect(clippy::unwrap_used, reason = "this helper is only called by the CLI tests")]
fn assert_workbook_and_sibling_csv(output_name: &str) {
    let tree = TempTree::with_entries(&["reports/"]);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/visa_sample.xml");
    let output = Command::new(env!("CARGO_BIN_EXE_visaparse"))
        .arg(&fixture)
        .args(["-o", output_name])
        .current_dir(tree.path())
        .env_remove("CARGO_MANIFEST_DIR")
        .env("HOME", tree.path())
        .env("USERPROFILE", tree.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let csv = tree.read("reports/report.csv");
    assert!(csv.starts_with("Date,Sum,Name\n"));
    assert!(csv.contains("2024.02.05,25.50,K-MARKET\n"));
    assert!(
        std::fs::read(tree.join(output_name))
            .unwrap()
            .starts_with(b"PK\x03\x04")
    );
}

#[test]
fn named_workbook_writes_the_csv_beside_it() {
    assert_workbook_and_sibling_csv("reports/report.xlsx");
}

#[test]
fn uppercase_workbook_extension_preserves_the_requested_filename() {
    assert_workbook_and_sibling_csv("reports/report.XLSX");
}
