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
    let mut actual = BTreeSet::new();
    files(&cassette_root(), &mut actual)?;
    assert_eq!(actual, expected, "missing or orphaned consumer cassettes");
    for path in actual {
        let contents = std::fs::read_to_string(&path)?;
        let failures = cassette_safety_failures(&path, &contents);
        assert!(failures.is_empty(), "{}: {failures:?}", path.display());
    }
    Ok(())
}
