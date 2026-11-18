//! A command's entry compiled to QuickJS bytecode and kept on this machine,
//! so a run after the first loads it instead of parsing the source again.
//!
//! Only bytecode this user's engine wrote is ever read: QuickJS does not
//! verify bytecode, and a crafted file can corrupt the host's memory, which is
//! why a package may not bring any. The file name is the SHA-256 of the
//! engine's version, the QuickJS build and the exact text compiled —
//! predictable, so what keeps a planted file out is that the directory is
//! this user's alone (0700, files 0600) and a file or directory anyone else
//! could read or write is neither read nor written. The directory keeps the
//! [`KEPT`] most recently used entries. rquickjs exposes compiling and
//! loading only for modules, and an entry runs as a script, hence the calls
//! into QuickJS itself.

use rquickjs::{qjs, Ctx};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const EXTENSION: &str = "qjsbc";

/// How old a staging file has to be before pruning takes it for one a crashed
/// run left, rather than one another run is still writing.
const STAGING_ABANDONED: Duration = Duration::from_secs(3600);

/// How many compiled entries a cache directory keeps.
pub(crate) const KEPT: usize = 64;

/// Where the bytecode of `source` compiled under `filename` is kept in `dir`.
pub(crate) fn cache_file(dir: &Path, filename: &str, source: &str) -> PathBuf {
    let mut hash = Sha256::new();
    for part in [env!("CARGO_PKG_VERSION"), quickjs_build(), filename, source] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    dir.join(format!("{}.{EXTENSION}", hex::encode(hash.finalize())))
}

/// The QuickJS this process runs: its version, and a digest of the bytecode
/// it writes for [`PROBE`]. Two builds under one engine version — another
/// rquickjs in a lockfile — can carry QuickJS sources that number opcodes or
/// predefined atoms differently, and must not read each other's entries.
fn quickjs_build() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| {
        // SAFETY: JS_GetVersion returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(qjs::JS_GetVersion()) };
        let probe = rquickjs::Runtime::new()
            .and_then(|runtime| rquickjs::Context::full(&runtime))
            .and_then(|context| context.with(|ctx| compile(&ctx, PROBE, "probe.js")))
            .map(|bytes| hex::encode(Sha256::digest(bytes)))
            .unwrap_or_else(|e| format!("unprobed: {e}"));
        format!(
            "QuickJS {} {} {probe}",
            version.to_string_lossy(),
            std::env::consts::ARCH
        )
    })
}

/// A script whose bytecode touches many opcodes and predefined atoms.
const PROBE: &str = r#"
async function* probe(a, ...rest) {
  outer: for (let i = 0n; ; i++) { if (i > 2n) break outer; else continue; }
  for await (const x of rest) yield x?.length ?? a;
  const { b = [1, 2], ...c } = { c: /x+/gu, d: `t${a}` };
  class K extends Array { #p = 1; static s() { return super.prototype; } get g() { return this.#p; } }
  try { throw new TypeError(`${typeof b} ${void 0} ${b instanceof K} ${"p" in c}`); }
  catch { delete c.c; } finally { a **= 2; a >>>= 1; a ||= !a; }
  switch (a) { case 1: a = -a; break; default: a = ~a; }
  return [new Proxy({}, {}), Symbol.iterator, Reflect, JSON, Math, Promise, Map, Set, WeakRef,
    globalThis, arguments, new.target, Object.keys, Array.from, String.raw, Number.isNaN,
    Date.now, BigInt, ArrayBuffer, Uint8Array, DataView, Atomics, Error, RangeError];
}
"#;

/// Compile `source` as a script without running it.
pub(crate) fn compile(ctx: &Ctx<'_>, source: &str, filename: &str) -> rquickjs::Result<Vec<u8>> {
    let mut input = source.as_bytes().to_vec();
    let len = input.len();
    // QuickJS reads one byte past the end and needs it to be NUL.
    input.push(0);
    let filename = CString::new(filename)?;
    let raw = ctx.as_raw().as_ptr();
    let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    unsafe {
        let function = qjs::JS_Eval(
            raw,
            input.as_ptr().cast(),
            len as _,
            filename.as_ptr(),
            flags,
        );
        if qjs::JS_IsException(function) {
            return Err(rquickjs::Error::Exception);
        }
        let mut size = 0;
        let written =
            qjs::JS_WriteObject(raw, &mut size, function, qjs::JS_WRITE_OBJ_BYTECODE as i32);
        qjs::JS_FreeValue(raw, function);
        if written.is_null() {
            return Err(rquickjs::Error::Exception);
        }
        let bytes = std::slice::from_raw_parts(written, size as usize).to_vec();
        qjs::js_free(raw, written.cast());
        Ok(bytes)
    }
}

/// What reading kept bytecode came to.
pub(crate) enum Loaded {
    /// It ran; an exception it threw is pending in the context.
    Ran(rquickjs::Result<()>),
    /// It is not bytecode this QuickJS reads.
    Unreadable,
}

/// Run bytecode [`compile`] produced in this process or an earlier one with
/// the same engine.
pub(crate) fn run(ctx: &Ctx<'_>, bytes: &[u8]) -> Loaded {
    let raw = ctx.as_raw().as_ptr();
    unsafe {
        let function = qjs::JS_ReadObject(
            raw,
            bytes.as_ptr(),
            bytes.len() as _,
            qjs::JS_READ_OBJ_BYTECODE as i32,
        );
        if qjs::JS_IsException(function) {
            let _ = ctx.catch();
            return Loaded::Unreadable;
        }
        let result = qjs::JS_EvalFunction(raw, function);
        if qjs::JS_IsException(result) {
            return Loaded::Ran(Err(rquickjs::Error::Exception));
        }
        qjs::JS_FreeValue(raw, result);
        Loaded::Ran(Ok(()))
    }
}

/// The bytecode kept at `file`, when it and its directory are this user's
/// alone; marked used, so pruning keeps it.
pub(crate) fn load(file: &Path) -> Option<Vec<u8>> {
    if !private(&std::fs::symlink_metadata(file.parent()?).ok()?) {
        return None;
    }
    let seen = std::fs::symlink_metadata(file).ok()?;
    if !seen.is_file() || !private(&seen) {
        return None;
    }
    let mut opened = std::fs::File::open(file).ok()?;
    if !same_file(&seen, &opened.metadata().ok()?) {
        return None;
    }
    let mut bytes = Vec::new();
    opened.read_to_end(&mut bytes).ok()?;
    let _ = opened.set_modified(std::time::SystemTime::now());
    Some(bytes)
}

/// Keep `bytes` at `file`, replacing it whole or not at all: another process
/// may be reading or writing the same entry. Nothing is written into a
/// directory, or over a file, that is not this user's alone.
pub(crate) fn store(file: &Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else {
        return;
    };
    if let Err(e) = write_private(dir, file, bytes) {
        tracing::warn!(file = %file.display(), error = %e, "the compiled command is not kept");
    }
    prune(dir, KEPT);
}

fn write_private(dir: &Path, file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let not_ours = |what: &Path| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not this user's alone", what.display()),
        )
    };
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match private_dir_builder().create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    if !private(&std::fs::symlink_metadata(dir)?) {
        return Err(not_ours(dir));
    }
    if let Ok(existing) = std::fs::symlink_metadata(file) {
        if !existing.is_file() || !owned(&existing) {
            return Err(not_ours(file));
        }
    }
    let opts = base::fs::WriteOpts {
        mode: Some(0o600),
        fsync: false,
    };
    base::fs::write_atomic(file, bytes, opts)
}

/// Remove all but the `keep` most recently used entries in `dir`, and the
/// staging files runs that never finished left.
pub(crate) fn prune(dir: &Path, keep: usize) {
    let Ok(listed) = std::fs::read_dir(dir) else {
        return;
    };
    let staging = format!(".{EXTENSION}.");
    let mut entries = Vec::new();
    for entry in listed.flatten() {
        let Some(used) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        let path = entry.path();
        if path.extension().is_some_and(|x| x == EXTENSION) {
            entries.push((used, path));
        } else if entry.file_name().to_string_lossy().contains(&staging)
            && used.elapsed().is_ok_and(|age| age > STAGING_ABANDONED)
        {
            let _ = std::fs::remove_file(path);
        }
    }
    if entries.len() <= keep {
        return;
    }
    entries.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    for (_, path) in entries.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
fn owned(meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    meta.uid() == unsafe { libc::geteuid() }
}

/// This user's, and readable and writable by nobody else.
#[cfg(unix)]
fn private(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    owned(meta) && meta.permissions().mode() & 0o077 == 0
}

#[cfg(unix)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn owned(_: &Metadata) -> bool {
    true
}

#[cfg(not(unix))]
fn private(_: &Metadata) -> bool {
    true
}

#[cfg(not(unix))]
fn same_file(_: &Metadata, _: &Metadata) -> bool {
    true
}

#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> std::io::Result<u32> {
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
    }

    fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    #[test]
    fn a_stored_entry_is_private_and_loads_back() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cache/.bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        store(&file, b"compiled");
        assert_eq!(mode(&dir).unwrap(), 0o700);
        assert_eq!(mode(&file).unwrap(), 0o600);
        assert_eq!(load(&file).as_deref(), Some(&b"compiled"[..]));
    }

    #[test]
    fn a_file_others_could_write_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let file = cache_file(root.path(), "cmd.js", "main()");
        chmod(root.path(), 0o700).unwrap();
        store(&file, b"compiled");
        chmod(&file, 0o620).unwrap();
        assert_eq!(load(&file), None);
        chmod(&file, 0o602).unwrap();
        assert_eq!(load(&file), None);
    }

    #[test]
    fn a_directory_others_could_write_is_neither_read_nor_written() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        store(&file, b"compiled");
        chmod(&dir, 0o777).unwrap();
        assert_eq!(load(&file), None);
        std::fs::remove_file(&file).unwrap();
        store(&file, b"again");
        assert!(!file.exists(), "nothing is written into a shared directory");
        chmod(&dir, 0o700).unwrap();
    }

    #[test]
    fn a_link_in_place_of_an_entry_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        store(&file, b"compiled");
        let elsewhere = root.path().join("elsewhere");
        std::fs::write(&elsewhere, b"planted").unwrap();
        chmod(&elsewhere, 0o600).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
        assert_eq!(load(&file), None);
        store(&file, b"compiled");
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"planted");
    }

    #[test]
    fn someone_elses_entry_or_directory_is_not_this_users() {
        let root = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(root.path()).unwrap();
        assert!(owned(&meta));
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        use std::os::unix::fs::MetadataExt;
        assert_eq!(meta.uid(), me);
        if me != 0 {
            let root_owned = std::fs::metadata("/").unwrap();
            assert!(!owned(&root_owned) && !private(&root_owned));
        }
    }

    #[test]
    fn the_key_names_the_quickjs_build_not_only_the_engine_version() {
        let dir = Path::new("/cache");
        let mut hash = Sha256::new();
        hash.update(env!("CARGO_PKG_VERSION").as_bytes());
        hash.update([0]);
        hash.update(b"cmd.js");
        hash.update([0]);
        hash.update(b"main()");
        let by_engine_version = dir.join(format!("{}.{EXTENSION}", hex::encode(hash.finalize())));
        assert_ne!(cache_file(dir, "cmd.js", "main()"), by_engine_version);
    }

    #[test]
    fn the_quickjs_build_is_probed() {
        let build = quickjs_build();
        // SAFETY: JS_GetVersion returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(qjs::JS_GetVersion()) };
        assert!(build.contains(&*version.to_string_lossy()), "{build}");
        assert!(!build.contains("unprobed"), "{build}");
    }

    #[test]
    fn an_entry_or_directory_others_can_read_is_not_this_users_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        store(&file, b"compiled");
        chmod(&file, 0o640).unwrap();
        assert_eq!(load(&file), None);
        chmod(&file, 0o604).unwrap();
        assert_eq!(load(&file), None);
        chmod(&file, 0o600).unwrap();
        chmod(&dir, 0o750).unwrap();
        assert_eq!(load(&file), None);
        std::fs::remove_file(&file).unwrap();
        store(&file, b"again");
        assert!(
            !file.exists(),
            "nothing is written into a readable directory"
        );
        chmod(&dir, 0o700).unwrap();
    }

    #[test]
    fn concurrent_stores_of_one_entry_leave_it_whole_and_no_staging_behind() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        let bytes: Vec<u8> = (0..(2 << 20)).map(|i: u32| (i % 251) as u8).collect();
        for _ in 0..10 {
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    scope.spawn(|| store(&file, &bytes));
                }
            });
            assert!(
                std::fs::read(&file).unwrap() == bytes,
                "a torn entry was installed"
            );
        }
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(left, [file]);
    }

    #[test]
    fn a_stale_staging_file_is_pruned_and_a_fresh_one_kept() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".bytecode");
        let file = cache_file(&dir, "cmd.js", "main()");
        store(&file, b"compiled");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let stale = dir.join(format!(".{name}.1.0"));
        let fresh = dir.join(format!(".{name}.2.0"));
        std::fs::write(&stale, b"half").unwrap();
        std::fs::write(&fresh, b"half").unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        store(&cache_file(&dir, "cmd.js", "other()"), b"compiled");
        assert!(!stale.exists(), "a staging file a crash left is removed");
        assert!(
            fresh.exists(),
            "a staging file being written may be another run's"
        );
    }

    #[test]
    fn pruning_keeps_the_most_recently_used() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        chmod(dir, 0o700).unwrap();
        let now = std::time::SystemTime::now();
        let files: Vec<_> = (0..5)
            .map(|i| {
                let file = cache_file(dir, "cmd.js", &format!("main{i}()"));
                store(&file, b"x");
                let at = now - std::time::Duration::from_secs(100 - i);
                std::fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(at)
                    .unwrap();
                file
            })
            .collect();
        load(&files[0]).unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"kept").unwrap();
        prune(dir, 3);
        let left: Vec<bool> = files.iter().map(|f| f.exists()).collect();
        assert_eq!(left, [true, false, false, true, true]);
        assert!(dir.join("unrelated.txt").exists());
    }
}
