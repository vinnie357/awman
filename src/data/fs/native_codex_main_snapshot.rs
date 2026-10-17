//! Observed-consistent manifests only; this helper grants no execution authority.
use crate::data::{startup_gate::NativeFileIdentity, DataError};
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

type Result<T> = std::result::Result<T, DataError>;
type Stamp = (NativeFileIdentity, [u64; 6]);
const MAX_MANIFEST: usize = 8 * 1024 * 1024;
const OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

pub(crate) struct NativeMainTreeSnapshot {
    manifest: Vec<u8>,
    digest: [u8; 32],
    root: NativeFileIdentity,
}

impl NativeMainTreeSnapshot {
    pub(crate) fn manifest_bytes(&self) -> &[u8] {
        &self.manifest
    }
    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub(crate) fn root_identity(&self) -> &NativeFileIdentity {
        &self.root
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum MainManifestKind {
    Directory,
    File,
}

#[derive(Serialize)]
struct MainManifestEntry {
    kind: MainManifestKind,
    path: String,
    sha256: Option<String>,
    size: u64,
}

fn invalid(reason: impl std::fmt::Display) -> DataError {
    DataError::Other(format!("native main snapshot: {reason}"))
}

struct ManifestWriter(Vec<u8>);

impl Write for ManifestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_MANIFEST - self.0.len() {
            return Err(std::io::Error::other("manifest exceeds eight MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_manifest(entries: &[MainManifestEntry]) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Manifest<'a> {
        entries: &'a [MainManifestEntry],
        version: u32,
    }
    let mut writer = ManifestWriter(Vec::new());
    serde_json::to_writer(
        &mut writer,
        &Manifest {
            entries,
            version: 1,
        },
    )
    .map_err(invalid)?;
    Ok(writer.0)
}

#[allow(
    clippy::unnecessary_cast,
    reason = "Stat field widths and signedness differ across Unix targets."
)]
fn stamp(info: &Stat) -> Stamp {
    (
        NativeFileIdentity {
            device: info.st_dev as u64,
            inode: info.st_ino,
            owner_uid: info.st_uid,
            owner_gid: info.st_gid,
            mode: info.st_mode as u32,
        },
        [
            info.st_nlink as u64,
            info.st_size as u64,
            info.st_mtime as u64,
            info.st_mtime_nsec as u64,
            info.st_ctime as u64,
            info.st_ctime_nsec as u64,
        ],
    )
}

fn named(parent: &File, name: &std::ffi::OsStr) -> Result<Stat> {
    fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(invalid)
}

fn open_child(
    parent: &File,
    name: &std::ffi::OsStr,
    directory_required: bool,
) -> Result<(File, Stat)> {
    let before = named(parent, name)?;
    let flags = match FileType::from_raw_mode(before.st_mode) {
        FileType::Directory => OPEN_FLAGS | OFlags::DIRECTORY,
        FileType::RegularFile if !directory_required && before.st_nlink == 1 => OPEN_FLAGS,
        _ => return Err(invalid("symlink, special file, or hard-linked file")),
    };
    let file = File::from(fs::openat(parent, name, flags, Mode::empty()).map_err(invalid)?);
    let observed = stamp(&fs::fstat(&file).map_err(invalid)?);
    let expected = stamp(&before);
    if observed.0 != expected.0 || (!directory_required && observed != expected) {
        return Err(invalid("entry changed while opening"));
    }
    Ok((file, before))
}

struct RootPin {
    file: File,
    name: OsString,
    before: Stamp,
}

fn pin_root(path: &Path) -> Result<Vec<RootPin>> {
    if path.as_os_str().is_empty() {
        return Err(invalid("empty root path"));
    }
    let seed = if path.is_absolute() { "/" } else { "." };
    let file =
        File::from(fs::open(seed, OPEN_FLAGS | OFlags::DIRECTORY, Mode::empty()).map_err(invalid)?);
    let before = stamp(&fs::fstat(&file).map_err(invalid)?);
    let mut pins = vec![RootPin {
        file,
        name: seed.into(),
        before,
    }];
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => return Err(invalid("root contains a parent or unsupported component")),
        };
        let parent = pins.last().ok_or_else(|| invalid("missing root pin"))?;
        let (file, info) = open_child(&parent.file, name, true)?;
        pins.push(RootPin {
            file,
            name: name.into(),
            before: stamp(&info),
        });
    }
    Ok(pins)
}

fn names(file: &File, name_bytes: &mut usize) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(file).map_err(invalid)? {
        let entry = entry.map_err(invalid)?;
        let name = entry.file_name().to_str().map_err(invalid)?;
        if name == "." || name == ".." {
            continue;
        }
        if name.is_empty() || name.contains(['\\', '/', '\0']) {
            return Err(invalid("invalid manifest entry name"));
        }
        if name.len() >= MAX_MANIFEST - *name_bytes {
            return Err(invalid("entry names exceed manifest budget"));
        }
        *name_bytes += name.len() + 1;
        names.push(name.to_owned());
    }
    names.sort();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("duplicate directory entry"));
    }
    Ok(names)
}

struct Frame {
    file: File,
    before: Stamp,
    path: String,
    names: Vec<String>,
    next: usize,
}

fn walk(root: File) -> Result<Vec<MainManifestEntry>> {
    let mut name_bytes = 0;
    let before = stamp(&fs::fstat(&root).map_err(invalid)?);
    let root_names = names(&root, &mut name_bytes)?;
    let mut stack = vec![Frame {
        file: root,
        before,
        path: String::new(),
        names: root_names,
        next: 0,
    }];
    let mut entries = Vec::new();
    let mut encoded_bytes = encode_manifest(&[])?.len();
    while let Some(frame) = stack.last_mut() {
        if frame.next == frame.names.len() {
            let mut recheck_bytes = 0;
            if names(&frame.file, &mut recheck_bytes)? != frame.names
                || stamp(&fs::fstat(&frame.file).map_err(invalid)?) != frame.before
            {
                return Err(invalid("directory changed during traversal"));
            }
            let finished = stack
                .pop()
                .ok_or_else(|| invalid("missing directory frame"))?;
            if let Some(parent) = stack.last() {
                let name = Path::new(&finished.path)
                    .file_name()
                    .ok_or_else(|| invalid("missing entry name"))?;
                if stamp(&named(&parent.file, name)?) != finished.before {
                    return Err(invalid("directory binding changed during traversal"));
                }
            }
            continue;
        }
        let name = &frame.names[frame.next];
        frame.next += 1;
        if name.len() >= MAX_MANIFEST - frame.path.len() {
            return Err(invalid("entry path exceeds manifest budget"));
        }
        let path = if frame.path.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", frame.path)
        };
        let (mut file, info) = open_child(&frame.file, name.as_ref(), false)?;
        let directory = FileType::from_raw_mode(info.st_mode) == FileType::Directory;
        let mut size = 0u64;
        let mut hasher = Sha256::new();
        if !directory {
            let expected_size = u64::try_from(info.st_size).map_err(invalid)?;
            let mut reader = (&mut file).take(expected_size.saturating_add(1));
            let mut block = [0; 8192];
            loop {
                let count = reader.read(&mut block).map_err(invalid)?;
                if count == 0 {
                    break;
                }
                size = size
                    .checked_add(count as u64)
                    .ok_or_else(|| invalid("file length overflow"))?;
                hasher.update(&block[..count]);
            }
            if size != expected_size
                || stamp(&fs::fstat(&file).map_err(invalid)?) != stamp(&info)
                || stamp(&named(&frame.file, name.as_ref())?) != stamp(&info)
            {
                return Err(invalid("file changed while hashing"));
            }
        }
        let entry = MainManifestEntry {
            kind: if directory {
                MainManifestKind::Directory
            } else {
                MainManifestKind::File
            },
            path: path.clone(),
            sha256: (!directory).then(|| {
                hasher
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect()
            }),
            size,
        };
        let mut writer = ManifestWriter(Vec::new());
        serde_json::to_writer(&mut writer, &entry).map_err(invalid)?;
        let cost = writer.0.len() + usize::from(!entries.is_empty());
        if cost > MAX_MANIFEST - encoded_bytes {
            return Err(invalid("manifest exceeds eight MiB"));
        }
        encoded_bytes += cost;
        entries.push(entry);
        if directory {
            let child_names = names(&file, &mut name_bytes)?;
            stack.push(Frame {
                file,
                before: stamp(&info),
                path,
                names: child_names,
                next: 0,
            });
        }
    }
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(entries)
}

pub(crate) fn snapshot_native_main_tree(path: &Path) -> Result<NativeMainTreeSnapshot> {
    let pins = pin_root(path)?;
    let root = pins.last().ok_or_else(|| invalid("missing root pin"))?;
    let info = fs::fstat(&root.file).map_err(invalid)?;
    let entries = walk(root.file.try_clone().map_err(invalid)?)?;
    for (index, pin) in pins.iter().enumerate() {
        let changed = |current: Stat| {
            let current = stamp(&current);
            if index + 1 == pins.len() {
                current != pin.before
            } else {
                current.0 != pin.before.0
            }
        };
        if changed(fs::fstat(&pin.file).map_err(invalid)?)
            || (index > 0 && changed(named(&pins[index - 1].file, &pin.name)?))
        {
            return Err(invalid("root component changed during traversal"));
        }
    }
    let manifest = encode_manifest(&entries)?;
    let digest = Sha256::digest(&manifest).into();
    let root = stamp(&info).0;
    Ok(NativeMainTreeSnapshot {
        manifest,
        digest,
        root,
    })
}

#[cfg(test)]
#[path = "native_codex_main_snapshot_test.rs"]
mod tests;
