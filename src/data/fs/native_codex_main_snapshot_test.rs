//! Issue 230: observed-consistent native-main manifests, without execution authority.
use super::{encode_manifest, snapshot_native_main_tree, MainManifestEntry, MainManifestKind};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::PathBuf;

type TestResult = Result<(), Box<dyn Error>>;
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn fixture() -> Result<tempfile::TempDir, Box<dyn Error>> {
    Ok(tempfile::Builder::new()
        .prefix("awman-main-snapshot-")
        .tempdir_in(dirs::home_dir().ok_or("test requires HOME")?)?)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn empty_and_nested_trees_have_exact_sorted_manifests_and_root_identity() -> TestResult {
    let root = fixture()?;
    let empty = snapshot_native_main_tree(root.path())?;
    assert_eq!(empty.manifest_bytes(), br#"{"entries":[],"version":1}"#);
    assert_eq!(empty.digest(), &digest(empty.manifest_bytes()));
    fs::create_dir(root.path().join("dir"))?;
    fs::write(root.path().join("dir/z.txt"), b"")?;
    fs::write(root.path().join("dir/a.txt"), b"")?;
    let snapshot = snapshot_native_main_tree(root.path())?;
    let expected = br#"{"entries":[{"kind":"directory","path":"dir","sha256":null,"size":0},{"kind":"file","path":"dir/a.txt","sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":0},{"kind":"file","path":"dir/z.txt","sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","size":0}],"version":1}"#;
    assert_eq!(snapshot.manifest_bytes(), expected);
    assert_eq!(snapshot.digest(), &digest(expected));
    let metadata = fs::symlink_metadata(root.path())?;
    let identity = snapshot.root_identity();
    assert_eq!(identity.device, metadata.dev());
    assert_eq!(identity.inode, metadata.ino());
    assert_eq!(identity.owner_uid, metadata.uid());
    assert_eq!(identity.owner_gid, metadata.gid());
    assert_eq!(identity.mode, metadata.mode());
    Ok(())
}

#[test]
fn a_file_larger_than_eight_mib_has_an_exact_hash_in_a_small_manifest() -> TestResult {
    let root = fixture()?;
    let size = 8 * 1024 * 1024 + 1;
    let mut file = File::create(root.path().join("large"))?;
    file.set_len(size)?;
    file.seek(SeekFrom::End(-1))?;
    file.write_all(b"!")?;
    file.sync_all()?;
    let mut expected = Sha256::new();
    let zeros = [0; 8192];
    for _ in 0..1024 {
        expected.update(zeros);
    }
    expected.update(b"!");
    let snapshot = snapshot_native_main_tree(root.path())?;
    let document: serde_json::Value = serde_json::from_slice(snapshot.manifest_bytes())?;
    assert_eq!(
        document,
        serde_json::json!({"entries":[{
        "kind":"file","path":"large","sha256":hex(expected.finalize().as_ref()),"size":size
    }],"version":1})
    );
    assert!(snapshot.manifest_bytes().len() < 8 * 1024 * 1024);
    assert_eq!(snapshot.digest(), &digest(snapshot.manifest_bytes()));
    Ok(())
}

#[test]
fn unsafe_roots_links_special_files_and_invalid_names_are_rejected() -> TestResult {
    for case in [
        "component-link",
        "nested-link",
        "hardlink",
        "fifo",
        "backslash",
        "non-utf8",
        "missing",
        "file-root",
        "nul-root",
    ] {
        let root = fixture()?;
        let tree = root.path().join("tree");
        fs::create_dir(&tree)?;
        let input: PathBuf = match case {
            "component-link" => {
                fs::create_dir(tree.join("child"))?;
                symlink(&tree, root.path().join("link"))?;
                root.path().join("link/child")
            }
            "nested-link" => {
                symlink(root.path(), tree.join("link"))?;
                tree.clone()
            }
            "hardlink" => {
                fs::write(tree.join("a"), b"data")?;
                fs::hard_link(tree.join("a"), tree.join("b"))?;
                tree.clone()
            }
            "fifo" => {
                nix::unistd::mkfifo(&tree.join("fifo"), nix::sys::stat::Mode::S_IRUSR)?;
                tree.clone()
            }
            "backslash" => {
                fs::write(tree.join(r"bad\name"), b"")?;
                tree.clone()
            }
            "non-utf8" => {
                fs::write(tree.join(OsString::from_vec(vec![0xff])), b"")?;
                tree.clone()
            }
            "missing" => tree.join("absent"),
            "file-root" => {
                fs::write(tree.join("file"), b"")?;
                tree.join("file")
            }
            "nul-root" => tree.join("bad\0root"),
            _ => unreachable!("fixed unsafe path case"),
        };
        assert!(
            snapshot_native_main_tree(&input).is_err(),
            "accepted {case}"
        );
    }
    Ok(())
}

#[test]
fn only_the_encoded_manifest_document_is_limited_to_eight_mib() -> TestResult {
    let suffix = vec!["x".repeat(200); 11].join("/");
    let entries: Vec<_> = (0..4096)
        .map(|index| MainManifestEntry {
            kind: MainManifestKind::File,
            path: format!("{index:04}/{suffix}"),
            sha256: Some(EMPTY_SHA.into()),
            size: 0,
        })
        .collect();
    let below = encode_manifest(&entries[..2048])?;
    assert!(below.len() < 8 * 1024 * 1024);
    assert!(encode_manifest(&entries).is_err());
    Ok(())
}
