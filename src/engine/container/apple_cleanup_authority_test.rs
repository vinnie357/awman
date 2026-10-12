use super::*;
use crate::engine::container::gated_launch::{
    provider_call_deadline, GatedProviderAdapter, NamePresence, ProviderKind,
};
use std::error::Error;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TARGET: &str = "awman-altana-cleanup-authority";

struct ListCliFixture {
    _directory: tempfile::TempDir,
    executable: PathBuf,
}

impl ListCliFixture {
    fn new(stdout: &[u8]) -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("container");
        let output = directory.path().join("stdout");
        std::fs::write(&output, stdout)?;
        std::fs::write(
            &executable,
            b"#!/bin/sh\n\
if [ \"$#\" -ne 4 ] || [ \"$1\" != list ] || [ \"$2\" != --all ] || [ \"$3\" != --format ] || [ \"$4\" != json ]; then\n\
  exit 64\n\
fi\n\
/bin/cat \"${0%/*}/stdout\"\n",
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _directory: directory,
            executable,
        })
    }
}

fn inspect_name(stdout: &[u8]) -> Result<Result<AbsenceObservation, NamePresence>, Box<dyn Error>> {
    let fixture = ListCliFixture::new(stdout)?;
    let adapter =
        AppleGatedProviderAdapter::try_new_with_executable(Path::new(&fixture.executable))
            .map_err(|failure| {
                io::Error::other(format!("provider fixture unavailable: {failure:?}"))
            })?;
    Ok(adapter.inspect_name_absence(
        &ContainerName::new(TARGET),
        provider_call_deadline(Instant::now() + Duration::from_secs(2)),
    ))
}

fn assert_absent(stdout: &[u8]) -> Result<(), Box<dyn Error>> {
    let absence = inspect_name(stdout)?.map_err(|presence| {
        io::Error::other(format!("valid listing did not prove absence: {presence:?}"))
    })?;
    assert_eq!(absence.provider, ProviderKind::AppleContainers);
    assert_eq!(absence.exact_name.as_str(), TARGET);
    Ok(())
}

#[test]
fn strict_valid_empty_and_nonmatching_apple_listings_prove_absence() -> Result<(), Box<dyn Error>> {
    assert_absent(b"[]")?;
    for stdout in [
        br#"[{"configuration":{"id":"awman-other"}}]"#.as_slice(),
        br#"[{"Name":"/awman-other"}]"#.as_slice(),
        br#"[{"name":"awman-other"}]"#.as_slice(),
        br#"[{"Names":["/awman-other"]}]"#.as_slice(),
    ] {
        assert_absent(stdout)?;
    }
    assert_absent(
        b"{\"configuration\":{\"id\":\"awman-other\"}}\n{\"configuration\":{\"id\":\"awman-third\"}}\n",
    )?;
    Ok(())
}

#[test]
fn exact_supported_apple_name_is_present() -> Result<(), Box<dyn Error>> {
    for stdout in [
        br#"[{"configuration":{"id":"awman-altana-cleanup-authority"}}]"#.as_slice(),
        br#"[{"Name":"/awman-altana-cleanup-authority"}]"#.as_slice(),
        br#"[{"name":"awman-altana-cleanup-authority"}]"#.as_slice(),
        br#"[{"Names":["/awman-altana-cleanup-authority"]}]"#.as_slice(),
    ] {
        assert!(matches!(inspect_name(stdout)?, Err(NamePresence::Present)));
    }
    Ok(())
}

#[test]
fn parsed_unknown_null_conflicting_or_mixed_rows_are_ambiguous() -> Result<(), Box<dyn Error>> {
    for stdout in [
        br#"[{}]"#.as_slice(),
        br#"[{"configuration":{}}]"#.as_slice(),
        br#"[{"configuration":{"id":null}}]"#.as_slice(),
        br#"[{"configuration":{"id":""}}]"#.as_slice(),
        br#"[{"Name":null}]"#.as_slice(),
        br#"[{"Name":7}]"#.as_slice(),
        br#"[{"Name":"/"}]"#.as_slice(),
        br#"[{"name":null}]"#.as_slice(),
        br#"[{"name":{}}]"#.as_slice(),
        br#"[{"name":""}]"#.as_slice(),
        br#"[{"Names":[]}]"#.as_slice(),
        br#"[{"Names":null}]"#.as_slice(),
        br#"[{"Names":[7]}]"#.as_slice(),
        br#"[{"Names":["/"]}]"#.as_slice(),
        br#"[{"Names":["/awman-other","/awman-altana-cleanup-authority"]}]"#.as_slice(),
        br#"[{"id":"awman-other"}]"#.as_slice(),
        br#"[7]"#.as_slice(),
        br#"[{"configuration":{"id":"awman-other"},"Name":"/awman-altana-cleanup-authority"}]"#
            .as_slice(),
        br#"[{"Name":"/awman-other","name":"awman-altana-cleanup-authority"}]"#.as_slice(),
        br#"[{"configuration":{"id":"awman-other"}},{}]"#.as_slice(),
        b"{\"configuration\":{\"id\":\"awman-other\"}}\n{}\n".as_slice(),
    ] {
        assert!(matches!(
            inspect_name(stdout)?,
            Err(NamePresence::Ambiguous)
        ));
    }
    Ok(())
}

#[test]
fn malformed_or_whitespace_only_output_is_unavailable() -> Result<(), Box<dyn Error>> {
    for stdout in [
        b"[{".as_slice(),
        b"not-json\n".as_slice(),
        b" \n\t\r\n".as_slice(),
    ] {
        assert!(matches!(
            inspect_name(stdout)?,
            Err(NamePresence::Unavailable)
        ));
    }
    Ok(())
}

#[test]
fn duplicate_identity_keys_in_array_rows_are_ambiguous() -> Result<(), Box<dyn Error>> {
    for stdout in [
        br#"[{"Name":"/awman-altana-cleanup-authority","Name":"/awman-other"}]"#.as_slice(),
        br#"[{"Name":"/awman-other","Name":"/awman-altana-cleanup-authority"}]"#.as_slice(),
        br#"[{"configuration":{"id":"awman-altana-cleanup-authority","id":"awman-other"}}]"#
            .as_slice(),
        br#"[{"configuration":{"id":"awman-altana-cleanup-authority"},"configuration":{"id":"awman-other"}}]"#
            .as_slice(),
    ] {
        assert!(matches!(
            inspect_name(stdout)?,
            Err(NamePresence::Ambiguous)
        ));
    }
    Ok(())
}

#[test]
fn duplicate_identity_keys_in_ndjson_rows_are_ambiguous() -> Result<(), Box<dyn Error>> {
    let stdout = br#"{"Name":"/awman-altana-cleanup-authority","Name":"/awman-other"}
{"configuration":{"id":"awman-third"}}
"#;
    assert!(matches!(
        inspect_name(stdout)?,
        Err(NamePresence::Ambiguous)
    ));
    Ok(())
}
