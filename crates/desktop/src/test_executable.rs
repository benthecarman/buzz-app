//! Test fixtures that create executable scripts.

use std::path::Path;

/// Creates an owner-only executable at `path` that holds `contents`.
///
/// Tests run in parallel threads, and other threads fork child processes. A
/// fork can copy this process's open write descriptor for a new script. Until
/// that child execs, Linux refuses to execute the script with ETXTBSY. This
/// process therefore never opens `path` for writing: it writes a sibling
/// source file, and a single-threaded `cp` process creates the executable.
#[cfg(unix)]
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    let mut source = path.as_os_str().to_owned();
    source.push(".source");
    let source = Path::new(&source);
    std::fs::write(source, contents).unwrap();
    let status = std::process::Command::new("/bin/cp")
        .arg(source)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "could not copy {}", path.display());
    std::fs::remove_file(source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Windows has no fork, so a plain write cannot leak a descriptor.
#[cfg(not(unix))]
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::write(path, contents).unwrap();
}
