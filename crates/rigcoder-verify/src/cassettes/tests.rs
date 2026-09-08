//! Consumer fixture ownership and safety at the downstream repository root.

use super::*;
use std::collections::BTreeSet;
use std::path::Path;

fn files(root: &Path, output: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            files(&entry.path(), output)?;
        } else {
            output.insert(entry.path());
        }
    }
    Ok(())
}

#[test]
fn consumer_cassettes_match_the_registry_and_pass_safety_checks()
-> Result<(), Box<dyn std::error::Error>> {
    let expected: BTreeSet<_> = consumer_registry::cases()
        .into_iter()
        .filter_map(|case| {
            case.provider
                .cassette_provider()
                .map(|provider| cassette_path(provider, &format!("ecs_consumer/{}", case.id)))
        })
        .collect();
    // The consumer owns `fixtures/cassettes/<provider>/ecs_consumer`; the
    // product's own matrices (`observe_*`) live beside it and check
    // themselves (`gemini_observe::every_observe_cassette_is_scrubbed`).
    let mut all = BTreeSet::new();
    files(&cassette_root(), &mut all)?;
    let actual: BTreeSet<_> = all
        .into_iter()
        .filter(|path: &std::path::PathBuf| {
            path.parent()
                .and_then(|dir| dir.file_name())
                .is_some_and(|name| name == "ecs_consumer")
        })
        .collect();
    assert_eq!(actual, expected, "missing or orphaned consumer cassettes");
    for path in actual {
        let contents = std::fs::read_to_string(&path)?;
        let failures = cassette_safety_failures(&path, &contents);
        assert!(failures.is_empty(), "{}: {failures:?}", path.display());
    }
    Ok(())
}
