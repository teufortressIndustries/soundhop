//! Build script for `soundhop-cli`: embeds the Windows application icon,
//! DPI/UTF-8/asInvoker manifest, and `VERSIONINFO` metadata into Windows binaries.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/soundhop.ico");
    println!("cargo:rerun-if-changed=../../packaging/windows/soundhop.manifest");

    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    use std::path::PathBuf;

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("soundhop-cli should live under crates/soundhop-cli");

    let icon_path = repo_root.join("assets").join("soundhop.ico");
    let manifest_path = repo_root
        .join("packaging")
        .join("windows")
        .join("soundhop.manifest");

    assert!(
        icon_path.is_file(),
        "Missing Windows icon resource at {}",
        icon_path.display()
    );
    assert!(
        manifest_path.is_file(),
        "Missing Windows application manifest at {}",
        manifest_path.display()
    );

    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let packed_version = pack_version_u64(&pkg_version);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(
        icon_path
            .to_str()
            .expect("icon path must be valid UTF-8"),
    );
    res.set_manifest_file(
        manifest_path
            .to_str()
            .expect("manifest path must be valid UTF-8"),
    );
    res.set("ProductName", "SoundHop");
    res.set(
        "FileDescription",
        "SoundHop - Wi-Fi Speaker Sharing & Multi-Room Audio",
    );
    res.set("CompanyName", "SoundHop contributors");
    res.set(
        "LegalCopyright",
        "Copyright (c) SoundHop contributors (MIT License)",
    );
    res.set("ProductVersion", &pkg_version);
    res.set("FileVersion", &pkg_version);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed_version);
    res.set_version_info(winresource::VersionInfo::FILEVERSION, packed_version);

    if let Err(err) = res.compile() {
        panic!("Failed to compile Windows resources: {err}");
    }
}

#[cfg(windows)]
fn pack_version_u64(version: &str) -> u64 {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|s| s.parse::<u16>().unwrap_or(0));
    let major = parts.next().unwrap_or(0) as u64;
    let minor = parts.next().unwrap_or(0) as u64;
    let patch = parts.next().unwrap_or(0) as u64;
    let build = parts.next().unwrap_or(0) as u64;
    (major << 48) | (minor << 32) | (patch << 16) | build
}
