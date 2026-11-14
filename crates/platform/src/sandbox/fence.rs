//! The process-level fence table: what this process serves its clients on, which no
//! confined command may reach. Backends read it when they write a policy.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// What this process serves its clients on, one entry per live [`Fence`].
static SERVED: RwLock<Vec<(u64, Served)>> = RwLock::new(Vec::new());

static NEXT_FENCE: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(not(feature = "sandbox"), allow(dead_code))]
enum Served {
    Socket(PathBuf),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Port(SocketAddr),
    Secret(PathBuf),
}

/// One listener's or one file's place in what no confined command may reach,
/// until it is dropped. Held as long as what it fences: a port or a path
/// outlives the listener or file that had it, and whatever is there next is
/// not this process's to fence.
#[must_use = "the fence comes down when this is dropped"]
pub struct Fence(u64);

impl Drop for Fence {
    fn drop(&mut self) {
        let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
        served.retain(|(id, _)| *id != self.0);
    }
}

fn fence(entry: Served) -> Fence {
    let id = NEXT_FENCE.fetch_add(1, Ordering::Relaxed);
    let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
    served.push((id, entry));
    Fence(id)
}

/// No command this process confines may connect to the unix socket at
/// `path` while the returned fence is held. For a daemon's own listener,
/// wherever `--socket` put it: a command connected to it would be its host.
pub fn fence_socket(path: &Path) -> Fence {
    fence(Served::Socket(
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

/// [`fence_socket`] for a TCP listener. Only the macOS backend can refuse a
/// port; bwrap can cut the network only as a whole.
pub fn fence_port(addr: SocketAddr) -> Fence {
    fence(Served::Port(addr))
}

/// No command this process confines may read the file at `path` while the
/// returned fence is held, whatever `sandbox.allow_read` says. For a
/// credential the daemon writes for its own clients, such as the unix
/// socket's token.
pub fn fence_secret(path: &Path) -> Fence {
    fence(Served::Secret(
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

#[cfg(feature = "sandbox")]
fn fenced<T: Ord>(pick: impl Fn(&Served) -> Option<T>) -> Vec<T> {
    let served = SERVED.read().unwrap_or_else(|e| e.into_inner());
    let mut found: Vec<T> = served.iter().filter_map(|(_, s)| pick(s)).collect();
    found.sort();
    found.dedup();
    found
}

/// The fenced secrets that are still there.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
pub(super) fn fenced_secrets() -> Vec<PathBuf> {
    fenced(|s| match s {
        Served::Secret(p) if p.exists() => Some(p.clone()),
        _ => None,
    })
}

/// The fenced sockets that are still there.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
pub(super) fn fenced_sockets() -> Vec<PathBuf> {
    fenced(|s| match s {
        Served::Socket(p) if is_socket(p) => Some(p.clone()),
        _ => None,
    })
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
pub(super) fn fenced_ports() -> Vec<SocketAddr> {
    fenced(|s| match s {
        Served::Port(addr) => Some(*addr),
        _ => None,
    })
}

#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
pub(super) fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_socket())
}
