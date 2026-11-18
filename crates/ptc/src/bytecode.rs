//! A command's entry compiled to QuickJS bytecode and kept on this machine,
//! so a run after the first loads it instead of parsing the source again.
//!
//! Only bytecode this process wrote is ever read: QuickJS does not verify
//! bytecode, and a crafted file can corrupt the host's memory, which is why a
//! package may not bring any. The file name is the SHA-256 of the engine's
//! version and the exact text compiled, so a changed entry or another engine
//! version names a different file. rquickjs exposes compiling and loading only
//! for modules, and an entry runs as a script, hence the calls into QuickJS
//! itself.

use rquickjs::{qjs, Ctx};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// Where the bytecode of `source` compiled under `filename` is kept in `dir`.
pub(crate) fn cache_file(dir: &Path, filename: &str, source: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(env!("CARGO_PKG_VERSION").as_bytes());
    hash.update([0]);
    hash.update(filename.as_bytes());
    hash.update([0]);
    hash.update(source.as_bytes());
    dir.join(format!("{}.qjsbc", hex::encode(hash.finalize())))
}

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

/// Keep `bytes` at `file`, replacing it whole or not at all: another process
/// may be reading or writing the same entry.
pub(crate) fn store(file: &Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else {
        return;
    };
    let staged = dir.join(format!(
        ".{}.{}",
        file.file_name().and_then(|n| n.to_str()).unwrap_or("entry"),
        std::process::id()
    ));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&staged, bytes))
        .and_then(|()| std::fs::rename(&staged, file));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&staged);
        tracing::warn!(file = %file.display(), error = %e, "the compiled command is not kept");
    }
}
