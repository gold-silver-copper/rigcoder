//! Gemini cassette matrices for the observation witness (Rig #2476) on the
//! real product session. Seven matrices (A–G), one file each; the tables
//! are in each file's doc comment. See `support` for the harness and the
//! recording order.

mod support;

mod drivers;
mod failures;
mod gates;
mod host;
mod interruptions;
mod lineage;
mod turns;

/// Every cassette these matrices hold is in scrubbed form and carries no
/// secret, token or local path — including the derived ones.
#[test]
fn every_observe_cassette_is_scrubbed() {
    let root = support::cassette_root().join("gemini");
    let mut seen = 0;
    for matrix in std::fs::read_dir(&root).unwrap().flatten() {
        if !matrix.file_name().to_string_lossy().starts_with("observe_") {
            continue;
        }
        for file in std::fs::read_dir(matrix.path()).unwrap().flatten() {
            let path = file.path();
            let contents = std::fs::read_to_string(&path).unwrap();
            let failures = rig_cassette::cassette_safety_failures(&path, &contents);
            assert!(failures.is_empty(), "{}: {failures:?}", path.display());
            assert!(
                !contents.contains("/Users/") && !contents.contains("/home/"),
                "{}",
                path.display()
            );
            seen += 1;
        }
    }
    eprintln!("[hygiene] {seen} cassettes checked");
    assert!(seen >= 40, "{seen}");
    // The evidence packets too: every file, the cassette engine's secret
    // and provider-token checks, and no local path.
    let mut packets = 0;
    let mut stack = vec![support::fixture_root().join("evidence")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let contents = std::fs::read_to_string(&path).unwrap();
            let failures = rig_cassette::artifact_safety_failures(&path, &contents);
            assert!(failures.is_empty(), "{}: {failures:?}", path.display());
            assert!(
                !contents.contains("/Users/") && !contents.contains("/home/"),
                "{}",
                path.display()
            );
            packets += 1;
        }
    }
    eprintln!("[hygiene] {packets} evidence files checked");
    // While packets are being written the later cells' are not there yet.
    if !support::writing_evidence() {
        assert!(packets >= 400, "{packets}");
    }
}
