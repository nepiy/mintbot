/// Run with GITLEAKS_BIN=/absolute/path/to/gitleaks cargo test --test security_scan_test -- --ignored.
#[test]
#[ignore = "requires an explicitly supplied Gitleaks executable"]
fn scanner_does_not_skip_dotenv_or_benchmark_sources() {
    let binary = std::env::var("GITLEAKS_BIN").expect("set GITLEAKS_BIN");
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    std::fs::create_dir_all(fixtures.join("src")).unwrap();
    let key_one = format!("{:064x}", 1);
    let key_two = format!("{:064x}", 2);
    std::fs::write(
        fixtures.join(".env"),
        format!("scan_probe = \"{key_one}\"\n"),
    )
    .unwrap();
    std::fs::write(
        fixtures.join("src/benchmark.rs"),
        format!("scan_probe = \"{key_one}\"\nscan_probe = \"{key_two}\"\n"),
    )
    .unwrap();
    // A deterministic rule avoids entropy heuristics hiding allowlist mistakes.
    let config_path = directory.path().join("gitleaks.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}\n{}",
            include_str!("../.gitleaks.toml"),
            r#"[[rules]]
id = "allowlist-regression-probe"
description = "Synthetic fixture, never a real secret"
regex = '''scan_probe = "([0-9a-f]{64})"'''
secretGroup = 1
"#
        ),
    )
    .unwrap();
    let report = directory.path().join("report.json");
    let output = std::process::Command::new(binary)
        .current_dir(&fixtures)
        .args(["dir", ".", "--no-banner", "--redact", "--config"])
        .arg(&config_path)
        .args(["--report-format", "json", "--report-path"])
        .arg(&report)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let findings: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(report).unwrap()).unwrap();
    assert_eq!(
        findings.len(),
        3,
        "expected .env and both synthetic benchmark keys: {findings:?}"
    );
    assert!(findings.iter().any(|finding| finding["File"] == ".env"));
    assert!(
        findings
            .iter()
            .any(|finding| finding["File"] == "src/benchmark.rs" && finding["StartLine"] == 2)
    );
}
