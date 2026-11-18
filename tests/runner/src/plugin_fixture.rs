//! 把 `examples/plugins/quickjs/`（示例插件包 `demo-quickjs` 的源码）、
//! `examples/plugins/forge/`（规格驱动开发的样例 `forge`）、
//! `tests/fixtures/ext_plugin/`（带 `[[command]]` 的 `notes-plugin`）或用例现造的
//! 插件目录打成 zip，给 `plugin.install` RPC 用。
//!
//! 为什么不直接提交一个 .zip：未打包的目录 diff 可读、可 review；打包是纯机械
//! 操作，测试运行时现打现用，没必要把二进制 blob 放进 git history。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEMO_PACKAGE: &str = "demo-quickjs";
pub const DEMO_PACKAGE_VERSION: &str = "1.0.0";

pub fn demo_package_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/quickjs")
}

pub const FORGE_PACKAGE: &str = "forge";
pub const FORGE_PACKAGE_VERSION: &str = "0.1.0";

pub fn forge_package_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/forge")
}

/// 打包 `forge_package_dir()`，返回 `(zip 绝对路径, sha256 hex 校验和)`。
pub fn package_forge(out_dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    package_dir(&forge_package_dir(), out_dir, FORGE_PACKAGE)
}

pub const EXT_PACKAGE: &str = "notes-plugin";
pub const EXT_PACKAGE_VERSION: &str = "1.0.0";

pub fn ext_package_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/ext_plugin")
}

/// 打包 `ext_package_dir()`，返回 `(zip 绝对路径, sha256 hex 校验和)`。
pub fn package_ext(out_dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    package_dir(&ext_package_dir(), out_dir, EXT_PACKAGE)
}

/// 打包 `demo_package_dir()`，返回 `(zip 绝对路径, sha256 hex 校验和)`。
pub fn package_demo(out_dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    package_dir(&demo_package_dir(), out_dir, DEMO_PACKAGE)
}

/// 打包任意一个插件源码目录到 `out_dir/<name>-<n>.zip`（`<n>` 自增，避免并发测试
/// 互相覆盖）。用例现造的包（JS 本身就是被测的东西，放在用例里比放在目录里好读）
/// 直接走这里。
pub fn package_dir(src: &Path, out_dir: &Path, name: &str) -> anyhow::Result<(PathBuf, String)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    anyhow::ensure!(
        src.join("plugin.toml").exists(),
        "expected plugin.toml under {}",
        src.display()
    );

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        add_dir_to_zip(&mut writer, src, src, opts)?;
        writer.finish()?;
    }

    std::fs::create_dir_all(out_dir)?;
    let zip_path = out_dir.join(format!("{name}-{n}.zip"));
    std::fs::write(&zip_path, &buf)?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let checksum = hex::encode(hasher.finalize());

    Ok((zip_path, checksum))
}

fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    writer: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    opts: zip::write::FileOptions<'_, ()>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            add_dir_to_zip(writer, root, &path, opts)?;
        } else {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            writer.start_file(rel, opts)?;
            let bytes = std::fs::read(&path)?;
            std::io::Write::write_all(writer, &bytes)?;
        }
    }
    Ok(())
}

/// `file://` URL for `plugin.install`'s `download_url` param. This scheme
/// doesn't require a checksum (`crates/packages/src/install.rs`), but the
/// packers compute one anyway and a real install sends one.
pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// 一个 zip 条目：普通文件、或指向 `target` 的符号链接。
pub enum Entry<'a> {
    File(&'a str, Vec<u8>),
    Symlink { name: &'a str, target: &'a str },
}

/// 用 `entries` 逐条打成 `out_dir/<name>-<n>.zip`，条目名原样写入（`../escape`、
/// `/abs/path` 都不会被规整），返回 `(zip 路径, sha256 hex)`。给恶意包用例造
/// 解包器必须自己拦下的输入。
pub fn zip_with_entries(
    out_dir: &Path,
    name: &str,
    entries: &[Entry<'_>],
) -> anyhow::Result<(PathBuf, String)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for entry in entries {
            match entry {
                Entry::File(entry_name, bytes) => {
                    writer.start_file(*entry_name, opts)?;
                    std::io::Write::write_all(&mut writer, bytes)?;
                }
                Entry::Symlink { name, target } => writer.add_symlink(*name, *target, opts)?,
            }
        }
        writer.finish()?;
    }

    std::fs::create_dir_all(out_dir)?;
    let zip_path = out_dir.join(format!("{name}-{n}.zip"));
    std::fs::write(&zip_path, &buf)?;

    use sha2::{Digest, Sha256};
    Ok((zip_path, hex::encode(Sha256::digest(&buf))))
}

/// 最小的合法清单，恶意条目就加在它旁边。
pub fn minimal_manifest(name: &str) -> Vec<u8> {
    format!("[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n")
        .into_bytes()
}

/// `bytes` 个零：压缩后只有几十 KB，解开是 zip bomb。
pub fn bomb_payload(bytes: usize) -> Vec<u8> {
    vec![0u8; bytes]
}
