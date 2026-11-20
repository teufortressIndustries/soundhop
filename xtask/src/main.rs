use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const TARGET_CARGO_TOML: &str = "Cargo.toml";
const TARGET_PKGBUILD: &str = "packaging/PKGBUILD";
const TARGET_SPEC: &str = "packaging/soundhop.spec";
const TARGET_DESKTOP: &str = "packaging/io.github.soundhop.desktop";
const TARGET_METAINFO: &str = "packaging/io.github.soundhop.metainfo.xml";
const TARGET_INSTALL_SH: &str = "packaging/install.sh";
const TARGET_INNO_SETUP: &str = "packaging/windows/soundhop.iss";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
}

impl SemVer {
    fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let mut parts = stripped.split('.');

        let major = parse_semver_part(parts.next(), input)?;
        let minor = parse_semver_part(parts.next(), input)?;
        let patch = parse_semver_part(parts.next(), input)?;

        if parts.next().is_some() {
            return Err(format!(
                "invalid version '{input}': expected strict X.Y.Z semver"
            ));
        }

        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    fn bump_patch(self) -> Result<Self, String> {
        let patch = self
            .patch
            .checked_add(1)
            .ok_or_else(|| "patch version overflow".to_string())?;
        Ok(Self {
            major: self.major,
            minor: self.minor,
            patch,
        })
    }

    fn bump_minor(self) -> Result<Self, String> {
        let minor = self
            .minor
            .checked_add(1)
            .ok_or_else(|| "minor version overflow".to_string())?;
        Ok(Self {
            major: self.major,
            minor,
            patch: 0,
        })
    }

    fn bump_major(self) -> Result<Self, String> {
        let major = self
            .major
            .checked_add(1)
            .ok_or_else(|| "major version overflow".to_string())?;
        Ok(Self {
            major,
            minor: 0,
            patch: 0,
        })
    }
}

fn parse_semver_part(part: Option<&str>, original: &str) -> Result<u64, String> {
    let Some(p) = part else {
        return Err(format!(
            "invalid version '{original}': expected strict X.Y.Z semver"
        ));
    };
    if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "invalid version '{original}': expected strict X.Y.Z semver"
        ));
    }
    if p.len() > 1 && p.starts_with('0') {
        return Err(format!(
            "invalid version '{original}': leading zeros are not allowed in semver"
        ));
    }
    p.parse::<u64>()
        .map_err(|_| format!("invalid version '{original}': numeric overflow"))
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BumpTarget {
    Patch,
    Minor,
    Major,
    Explicit(SemVer),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BumpCommand {
    Check,
    Bump {
        target: BumpTarget,
        dry_run: bool,
        no_test: bool,
    },
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedUpdate {
    rel_path: &'static str,
    old_version: String,
    new_version: String,
    new_content: String,
    changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DriftEntry {
    rel_path: &'static str,
    detail: String,
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_top_usage();
        return Err("missing subcommand".to_string());
    };

    match subcommand {
        "-h" | "--help" | "help" => {
            print_top_usage();
            Ok(())
        }
        "bump-version" => {
            let cmd = parse_bump_args(&args[1..])?;
            let root = find_repo_root()?;
            match cmd {
                BumpCommand::Help => {
                    print_bump_usage();
                    Ok(())
                }
                BumpCommand::Check => run_check(&root),
                BumpCommand::Bump {
                    target,
                    dry_run,
                    no_test,
                } => {
                    let today = today_utc()?;
                    run_bump(&root, target, &today, dry_run, no_test)
                }
            }
        }
        other => {
            print_top_usage();
            Err(format!("unknown xtask subcommand '{other}'"))
        }
    }
}

fn print_top_usage() {
    eprintln!(
        "Usage: cargo xtask <COMMAND>\n\n\
         Commands:\n\
         \x20 bump-version   Check or bump workspace and packaging versions"
    );
}

fn print_bump_usage() {
    eprintln!(
        "Usage:\n\
         \x20 cargo xtask bump-version --check\n\
         \x20 cargo xtask bump-version <X.Y.Z | --patch | --minor | --major> [--dry-run] [--no-test]\n\n\
         Options:\n\
         \x20 --check     Verify all packaging manifests match [workspace.package] version\n\
         \x20 --patch     Increment patch version (X.Y.Z -> X.Y.(Z+1))\n\
         \x20 --minor     Increment minor version (X.Y.Z -> X.(Y+1).0)\n\
         \x20 --major     Increment major version (X.Y.Z -> (X+1).0.0)\n\
         \x20 --dry-run   Print planned updates without writing any files\n\
         \x20 --no-test   Skip running `cargo test --workspace` after bumping\n\
         \x20 -h, --help  Show this help message"
    );
}

fn parse_bump_args(args: &[String]) -> Result<BumpCommand, String> {
    if args.is_empty() {
        print_bump_usage();
        return Err(
            "expected --check, --patch, --minor, --major, or an explicit <X.Y.Z> version"
                .to_string(),
        );
    }

    let mut check = false;
    let mut target: Option<BumpTarget> = None;
    let mut dry_run = false;
    let mut no_test = false;

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(BumpCommand::Help),
            "--check" => {
                if check {
                    return Err("--check specified more than once".to_string());
                }
                check = true;
            }
            "--patch" => set_target(&mut target, BumpTarget::Patch)?,
            "--minor" => set_target(&mut target, BumpTarget::Minor)?,
            "--major" => set_target(&mut target, BumpTarget::Major)?,
            "--dry-run" => dry_run = true,
            "--no-test" => no_test = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag '{flag}'"));
            }
            positional => {
                let ver = SemVer::parse(positional)?;
                set_target(&mut target, BumpTarget::Explicit(ver))?;
            }
        }
    }

    if check {
        if target.is_some() {
            return Err("--check cannot be combined with a bump target or version".to_string());
        }
        if dry_run || no_test {
            return Err("--check cannot be combined with --dry-run or --no-test".to_string());
        }
        return Ok(BumpCommand::Check);
    }

    let Some(target) = target else {
        return Err(
            "missing version target: pass <X.Y.Z>, --patch, --minor, --major, or --check"
                .to_string(),
        );
    };

    Ok(BumpCommand::Bump {
        target,
        dry_run,
        no_test,
    })
}

fn set_target(slot: &mut Option<BumpTarget>, next: BumpTarget) -> Result<(), String> {
    if slot.is_some() {
        return Err("multiple version targets specified; pass only one of <X.Y.Z>, --patch, --minor, or --major".to_string());
    }
    *slot = Some(next);
    Ok(())
}

fn find_repo_root() -> Result<PathBuf, String> {
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let candidate = PathBuf::from(manifest_dir);
        if let Some(parent) = candidate.parent() {
            if is_workspace_root(parent) {
                return Ok(parent.to_path_buf());
            }
        }
    }

    let mut dir = std::env::current_dir()
        .map_err(|e| format!("failed to determine current directory: {e}"))?;
    loop {
        if is_workspace_root(&dir) {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }

    Err(
        "could not locate workspace root containing Cargo.toml with [workspace.package]"
            .to_string(),
    )
}

fn is_workspace_root(dir: &Path) -> bool {
    let cargo_toml = dir.join(TARGET_CARGO_TOML);
    fs::read_to_string(cargo_toml)
        .map(|text| {
            text.lines()
                .any(|line| line.trim() == "[workspace.package]")
        })
        .unwrap_or(false)
}

fn today_utc() -> Result<String, String> {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock is before Unix epoch: {e}"))?;
    Ok(format_utc_date(dur.as_secs()))
}

fn format_utc_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

fn run_check(root: &Path) -> Result<(), String> {
    let expected = read_workspace_version(root)?;
    let drifts = check_versions(root, expected);

    if drifts.is_empty() {
        println!("OK: all packaging manifests match workspace version {expected}");
        return Ok(());
    }

    let mut report = format!("version drift detected (workspace Cargo.toml is {expected}):\n");
    for drift in &drifts {
        report.push_str(&format!("  - {}: {}\n", drift.rel_path, drift.detail));
    }
    Err(report.trim_end().to_string())
}

fn run_bump(
    root: &Path,
    target: BumpTarget,
    today: &str,
    dry_run: bool,
    no_test: bool,
) -> Result<(), String> {
    let current = read_workspace_version(root)?;
    let next = match target {
        BumpTarget::Patch => current.bump_patch()?,
        BumpTarget::Minor => current.bump_minor()?,
        BumpTarget::Major => current.bump_major()?,
        BumpTarget::Explicit(v) => v,
    };

    let plan = plan_bump(root, next, today)?;

    if dry_run {
        println!("Dry run: bumping SoundHop version {current} -> {next} (date {today})");
        for item in &plan {
            let status = if item.changed { "update" } else { "unchanged" };
            println!(
                "  [{status}] {} ({} -> {})",
                item.rel_path, item.old_version, item.new_version
            );
        }
        return Ok(());
    }

    println!("Bumping SoundHop version: {current} -> {next} (date {today})");
    apply_plan(root, &plan)?;
    for item in &plan {
        let marker = if item.changed {
            "updated"
        } else {
            "already at target"
        };
        println!(
            "  ✓ {} ({} -> {}, {marker})",
            item.rel_path, item.old_version, item.new_version
        );
    }

    if !no_test {
        println!("\nRunning `cargo test --workspace` to refresh Cargo.lock and verify...");
        let status = Command::new("cargo")
            .args(["test", "--workspace"])
            .current_dir(root)
            .status()
            .map_err(|e| format!("failed to execute `cargo test --workspace`: {e}"))?;
        if !status.success() {
            return Err(format!(
                "`cargo test --workspace` failed with exit status {status}"
            ));
        }
    }

    Ok(())
}

fn read_workspace_version(root: &Path) -> Result<SemVer, String> {
    let path = root.join(TARGET_CARGO_TOML);
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {TARGET_CARGO_TOML}: {e}"))?;
    let ver_str =
        extract_cargo_toml_version(&content).map_err(|e| format!("{TARGET_CARGO_TOML}: {e}"))?;
    SemVer::parse(&ver_str)
        .map_err(|e| format!("{TARGET_CARGO_TOML} has invalid workspace version: {e}"))
}

type VersionExtractor = fn(&str) -> Result<String, String>;
type VersionUpdater = fn(&str, &str, &str) -> Result<(String, String), String>;

fn check_versions(root: &Path, expected: SemVer) -> Vec<DriftEntry> {
    let expected_str = expected.to_string();
    let checks: [(&'static str, VersionExtractor); 6] = [
        (TARGET_PKGBUILD, extract_pkgbuild_version),
        (TARGET_SPEC, extract_spec_version),
        (TARGET_DESKTOP, extract_desktop_version),
        (TARGET_METAINFO, extract_metainfo_version),
        (TARGET_INSTALL_SH, extract_install_sh_version),
        (TARGET_INNO_SETUP, extract_inno_setup_version),
    ];

    let mut drifts = Vec::new();
    for (rel_path, extractor) in checks {
        let full_path = root.join(rel_path);
        match fs::read_to_string(&full_path) {
            Err(e) => drifts.push(DriftEntry {
                rel_path,
                detail: format!("failed to read file: {e}"),
            }),
            Ok(content) => match extractor(&content) {
                Err(e) => drifts.push(DriftEntry {
                    rel_path,
                    detail: e,
                }),
                Ok(found) if found != expected_str => drifts.push(DriftEntry {
                    rel_path,
                    detail: format!("expected {expected_str}, found {found}"),
                }),
                Ok(_) => {}
            },
        }
    }

    drifts
}

fn plan_bump(root: &Path, next: SemVer, today: &str) -> Result<Vec<PlannedUpdate>, String> {
    let next_str = next.to_string();
    let specs: [(&'static str, VersionUpdater); 7] = [
        (TARGET_CARGO_TOML, update_cargo_toml),
        (TARGET_PKGBUILD, update_pkgbuild),
        (TARGET_SPEC, update_spec),
        (TARGET_DESKTOP, update_desktop),
        (TARGET_METAINFO, update_metainfo),
        (TARGET_INSTALL_SH, update_install_sh),
        (TARGET_INNO_SETUP, update_inno_setup),
    ];

    let mut errors = Vec::new();
    let mut updates = Vec::with_capacity(specs.len());

    for (rel_path, updater) in specs {
        let full_path = root.join(rel_path);
        let content = match fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{rel_path}: failed to read file: {e}"));
                continue;
            }
        };

        match updater(&content, &next_str, today) {
            Ok((old_version, new_content)) => {
                let changed = new_content != content;
                updates.push(PlannedUpdate {
                    rel_path,
                    old_version,
                    new_version: next_str.clone(),
                    new_content,
                    changed,
                });
            }
            Err(e) => errors.push(format!("{rel_path}: {e}")),
        }
    }

    if !errors.is_empty() {
        let mut msg = String::from("validation failed before writing any files:\n");
        for err in &errors {
            msg.push_str(&format!("  - {err}\n"));
        }
        return Err(msg.trim_end().to_string());
    }

    Ok(updates)
}

fn apply_plan(root: &Path, plan: &[PlannedUpdate]) -> Result<(), String> {
    for item in plan {
        if item.changed {
            let full_path = root.join(item.rel_path);
            fs::write(&full_path, &item.new_content)
                .map_err(|e| format!("failed to write {}: {e}", item.rel_path))?;
        }
    }
    Ok(())
}

fn split_line_ending(raw_line: &str) -> (&str, &str) {
    if let Some(stripped) = raw_line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = raw_line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (raw_line, "")
    }
}

fn parse_quoted_toml_value(line: &str, key: &str) -> Option<(String, String, String)> {
    let rest = line.strip_prefix(key)?;
    let trimmed = rest.trim_start();
    let after_eq = trimmed.strip_prefix('=')?;
    let after_eq_ws = after_eq.trim_start();
    let inside = after_eq_ws.strip_prefix('"')?;
    let close_idx = inside.find('"')?;
    let value = &inside[..close_idx];
    let suffix = &inside[close_idx + 1..];
    let prefix_len = line.len() - inside.len();
    let prefix = &line[..prefix_len];
    Some((prefix.to_string(), value.to_string(), format!("\"{suffix}")))
}

fn extract_cargo_toml_version(content: &str) -> Result<String, String> {
    let (old, _) = update_cargo_toml(content, "0.0.0", "")?;
    Ok(old)
}

fn update_cargo_toml(
    content: &str,
    new_ver: &str,
    _today: &str,
) -> Result<(String, String), String> {
    let mut in_workspace_package = false;
    let mut found_section = false;
    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_workspace_package = trimmed == "[workspace.package]";
            if in_workspace_package {
                found_section = true;
            }
            out.push_str(raw_line);
            continue;
        }

        if in_workspace_package {
            if let Some((prefix, val, suffix)) = parse_quoted_toml_value(line, "version") {
                if old_version.is_some() {
                    return Err(
                        "multiple `version = \"...\"` entries found in [workspace.package]"
                            .to_string(),
                    );
                }
                old_version = Some(val);
                out.push_str(&prefix);
                out.push_str(new_ver);
                out.push_str(&suffix);
                out.push_str(ending);
                continue;
            }
        }

        out.push_str(raw_line);
    }

    if !found_section {
        return Err("missing `[workspace.package]` section".to_string());
    }
    let old = old_version
        .ok_or_else(|| "missing `version = \"...\"` under `[workspace.package]`".to_string())?;
    Ok((old, out))
}

fn extract_pkgbuild_version(content: &str) -> Result<String, String> {
    let (old, _) = update_pkgbuild(content, "0.0.0", "")?;
    Ok(old)
}

fn update_pkgbuild(content: &str, new_ver: &str, _today: &str) -> Result<(String, String), String> {
    let mut old_version: Option<String> = None;
    let mut found_pkgrel = false;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if let Some(val) = line.strip_prefix("pkgver=") {
            if old_version.is_some() {
                return Err("multiple `pkgver=` lines found".to_string());
            }
            old_version = Some(val.trim().to_string());
            out.push_str("pkgver=");
            out.push_str(new_ver);
            out.push_str(ending);
        } else if line.starts_with("pkgrel=") {
            if found_pkgrel {
                return Err("multiple `pkgrel=` lines found".to_string());
            }
            found_pkgrel = true;
            out.push_str("pkgrel=1");
            out.push_str(ending);
        } else {
            out.push_str(raw_line);
        }
    }

    let old = old_version.ok_or_else(|| "missing `pkgver=<version>` line".to_string())?;
    if !found_pkgrel {
        return Err("missing `pkgrel=<release>` line".to_string());
    }
    Ok((old, out))
}

fn extract_spec_version(content: &str) -> Result<String, String> {
    let (old, _) = update_spec(content, "0.0.0", "")?;
    Ok(old)
}

fn update_spec(content: &str, new_ver: &str, _today: &str) -> Result<(String, String), String> {
    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if let Some(rest) = line.strip_prefix("Version:") {
            if old_version.is_some() {
                return Err("multiple `Version:` headers found".to_string());
            }
            let val = rest.trim();
            if val.is_empty() {
                return Err("empty `Version:` header".to_string());
            }
            old_version = Some(val.to_string());
            out.push_str("Version:        ");
            out.push_str(new_ver);
            out.push_str(ending);
        } else {
            out.push_str(raw_line);
        }
    }

    let old = old_version.ok_or_else(|| "missing `Version:` header".to_string())?;
    Ok((old, out))
}

fn extract_desktop_version(content: &str) -> Result<String, String> {
    let (old, _) = update_desktop(content, "0.0.0", "")?;
    Ok(old)
}

fn update_desktop(content: &str, new_ver: &str, _today: &str) -> Result<(String, String), String> {
    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if let Some(val) = line.strip_prefix("X-AppImage-Version=") {
            if old_version.is_some() {
                return Err("multiple `X-AppImage-Version=` entries found".to_string());
            }
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return Err("empty `X-AppImage-Version=` entry".to_string());
            }
            old_version = Some(trimmed.to_string());
            out.push_str("X-AppImage-Version=");
            out.push_str(new_ver);
            out.push_str(ending);
        } else {
            out.push_str(raw_line);
        }
    }

    let old = old_version.ok_or_else(|| "missing `X-AppImage-Version=` entry".to_string())?;
    Ok((old, out))
}

fn extract_metainfo_version(content: &str) -> Result<String, String> {
    let (old, _) = update_metainfo(content, "0.0.0", "1970-01-01")?;
    Ok(old)
}

fn update_metainfo(content: &str, new_ver: &str, today: &str) -> Result<(String, String), String> {
    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if old_version.is_none() {
            if let Some(tag_start) = line.find("<release ") {
                let after_start = &line[tag_start..];
                let tag_end_rel = after_start
                    .find('>')
                    .ok_or_else(|| "unclosed `<release ...>` tag".to_string())?;
                let tag = &after_start[..=tag_end_rel];
                let version_attr = "version=\"";
                let ver_start = tag.find(version_attr).ok_or_else(|| {
                    "missing `version=\"...\"` attribute in `<release>` tag".to_string()
                })? + version_attr.len();
                let ver_rest = &tag[ver_start..];
                let ver_end = ver_rest.find('"').ok_or_else(|| {
                    "unterminated `version` attribute in `<release>` tag".to_string()
                })?;
                let found_ver = &ver_rest[..ver_end];
                if found_ver.is_empty() {
                    return Err("empty `version` attribute in `<release>` tag".to_string());
                }

                let self_closing = tag.ends_with("/>");
                let closing = if self_closing { "/>" } else { ">" };
                let before = &line[..tag_start];
                let after = &after_start[tag_end_rel + 1..];

                old_version = Some(found_ver.to_string());
                out.push_str(before);
                out.push_str(&format!(
                    "<release version=\"{new_ver}\" date=\"{today}\"{closing}"
                ));
                out.push_str(after);
                out.push_str(ending);
                continue;
            }
        }

        out.push_str(raw_line);
    }

    let old = old_version
        .ok_or_else(|| "missing `<release version=\"...\" date=\"...\">` element".to_string())?;
    Ok((old, out))
}

fn extract_install_sh_version(content: &str) -> Result<String, String> {
    let (old, _) = update_install_sh(content, "0.0.0", "")?;
    Ok(old)
}

fn update_install_sh(
    content: &str,
    new_ver: &str,
    _today: &str,
) -> Result<(String, String), String> {
    const PREFIX: &str = "VERSION=\"${VERSION:-";
    const SUFFIX: &str = "}\"";

    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if let Some(rest) = line.strip_prefix(PREFIX) {
            let Some(val) = rest.strip_suffix(SUFFIX) else {
                return Err(format!(
                    "malformed fallback version line (expected `{PREFIX}<version>{SUFFIX}`)"
                ));
            };
            if old_version.is_some() {
                return Err("multiple `VERSION=\"${VERSION:-...}\"` lines found".to_string());
            }
            if val.is_empty() {
                return Err("empty fallback version in `VERSION=\"${VERSION:-}\"`".to_string());
            }
            old_version = Some(val.to_string());
            out.push_str(PREFIX);
            out.push_str(new_ver);
            out.push_str(SUFFIX);
            out.push_str(ending);
        } else {
            out.push_str(raw_line);
        }
    }

    let old = old_version
        .ok_or_else(|| "missing `VERSION=\"${VERSION:-<version>}\"` fallback line".to_string())?;
    Ok((old, out))
}

fn extract_inno_setup_version(content: &str) -> Result<String, String> {
    let (old, _) = update_inno_setup(content, "0.0.0", "")?;
    Ok(old)
}

fn update_inno_setup(
    content: &str,
    new_ver: &str,
    _today: &str,
) -> Result<(String, String), String> {
    const PREFIX: &str = "#define MyAppVersion \"";
    const SUFFIX: &str = "\"";

    let mut old_version: Option<String> = None;
    let mut out = String::with_capacity(content.len());

    for raw_line in content.split_inclusive('\n') {
        let (line, ending) = split_line_ending(raw_line);

        if let Some(rest) = line.strip_prefix(PREFIX) {
            let Some(val) = rest.strip_suffix(SUFFIX) else {
                return Err(format!(
                    "malformed `#define MyAppVersion` line (expected `{PREFIX}<version>{SUFFIX}`)"
                ));
            };
            if old_version.is_some() {
                return Err("multiple `#define MyAppVersion \"...\"` lines found".to_string());
            }
            if val.is_empty() {
                return Err("empty version in `#define MyAppVersion \"\"`".to_string());
            }
            old_version = Some(val.to_string());
            out.push_str(PREFIX);
            out.push_str(new_ver);
            out.push_str(SUFFIX);
            out.push_str(ending);
        } else {
            out.push_str(raw_line);
        }
    }

    let old = old_version
        .ok_or_else(|| "missing `#define MyAppVersion \"<version>\"` line".to_string())?;
    Ok((old, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new(version: &str) -> Self {
            let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("soundhop-xtask-test-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("packaging").join("windows"))
                .expect("create temp packaging/windows dir");

            let ws = Self { root };
            ws.write_fixture(version);
            ws
        }

        fn write_fixture(&self, version: &str) {
            fs::write(
                self.root.join(TARGET_CARGO_TOML),
                format!(
                    "[workspace]\nmembers = [\"crates/soundhop-core\"]\n\n\
                     [workspace.package]\nversion = \"{version}\"\nedition = \"2021\"\n\n\
                     [workspace.dependencies]\nserde = \"1.0\"\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_PKGBUILD),
                format!(
                    "pkgname=soundhop\npkgver={version}\npkgrel=3\n\
                     pkgdesc=\"SoundHop\"\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_SPEC),
                format!(
                    "Name:           soundhop\n\
                     Version:        {version}\n\
                     Release:        1%{{?dist}}\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_DESKTOP),
                format!(
                    "[Desktop Entry]\n\
                     Type=Application\n\
                     Version=1.0\n\
                     Name=SoundHop\n\
                     X-AppImage-Version={version}\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_METAINFO),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <component type=\"desktop-application\">\n\
                     \x20 <releases>\n\
                     \x20   <release version=\"{version}\" date=\"2026-01-01\"/>\n\
                     \x20 </releases>\n\
                     </component>\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_INSTALL_SH),
                format!(
                    "#!/usr/bin/env bash\n\
                     VERSION=\"$(sed -n 's/^version = \"\\([^\"]*\\)\"/\\1/p' \"$REPO_DIR/Cargo.toml\" | head -n1)\"\n\
                     VERSION=\"${{VERSION:-{version}}}\"\n"
                ),
            )
            .unwrap();

            fs::write(
                self.root.join(TARGET_INNO_SETUP),
                format!(
                    "#define MyAppName \"SoundHop\"\n\
                     #define MyAppVersion \"{version}\"\n\
                     [Setup]\n\
                     AppVersion={{#MyAppVersion}}\n"
                ),
            )
            .unwrap();
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn semver_parsing_valid_and_invalid() {
        assert_eq!(
            SemVer::parse("2.1.2").unwrap(),
            SemVer {
                major: 2,
                minor: 1,
                patch: 2
            }
        );
        assert_eq!(
            SemVer::parse("v0.10.0").unwrap(),
            SemVer {
                major: 0,
                minor: 10,
                patch: 0
            }
        );

        for invalid in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "vv1.2.3",
            "1.2.3-rc1",
            "1.2.a",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "-1.2.3",
            "+1.2.3",
        ] {
            assert!(
                SemVer::parse(invalid).is_err(),
                "expected '{invalid}' to fail semver parsing"
            );
        }
    }

    #[test]
    fn semver_bumping() {
        let v = SemVer::parse("2.1.9").unwrap();
        assert_eq!(v.bump_patch().unwrap().to_string(), "2.1.10");
        assert_eq!(v.bump_minor().unwrap().to_string(), "2.2.0");
        assert_eq!(v.bump_major().unwrap().to_string(), "3.0.0");
    }

    #[test]
    fn cli_arg_parsing() {
        let args = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            parse_bump_args(&args(&["--check"])).unwrap(),
            BumpCommand::Check
        );
        assert_eq!(
            parse_bump_args(&args(&["--patch", "--dry-run", "--no-test"])).unwrap(),
            BumpCommand::Bump {
                target: BumpTarget::Patch,
                dry_run: true,
                no_test: true,
            }
        );
        assert_eq!(
            parse_bump_args(&args(&["v3.4.5"])).unwrap(),
            BumpCommand::Bump {
                target: BumpTarget::Explicit(SemVer {
                    major: 3,
                    minor: 4,
                    patch: 5
                }),
                dry_run: false,
                no_test: false,
            }
        );

        assert!(parse_bump_args(&args(&[])).is_err());
        assert!(parse_bump_args(&args(&["--check", "--patch"])).is_err());
        assert!(parse_bump_args(&args(&["--check", "--dry-run"])).is_err());
        assert!(parse_bump_args(&args(&["--patch", "--minor"])).is_err());
        assert!(parse_bump_args(&args(&["--patch", "2.3.0"])).is_err());
        assert!(parse_bump_args(&args(&["--unknown"])).is_err());
        assert!(parse_bump_args(&args(&["not-a-version"])).is_err());
    }

    #[test]
    fn civil_date_formatting_from_unix_timestamps() {
        assert_eq!(format_utc_date(0), "1970-01-01");
        assert_eq!(format_utc_date(86_400 - 1), "1970-01-01");
        assert_eq!(format_utc_date(86_400), "1970-01-02");
        // 2000-02-29 00:00:00 UTC (leap year divisible by 400)
        assert_eq!(format_utc_date(951_782_400), "2000-02-29");
        // 2024-02-29 12:34:56 UTC (leap year)
        assert_eq!(format_utc_date(1_709_210_096), "2024-02-29");
        // 2026-11-19 00:00:00 UTC
        assert_eq!(format_utc_date(1_795_046_400), "2026-11-19");
        // 2100-03-01 00:00:00 UTC (non-leap century year)
        assert_eq!(format_utc_date(4_107_542_400), "2100-03-01");
    }

    #[test]
    fn bump_updates_all_six_files_and_check_passes() {
        let ws = TempWorkspace::new("2.1.2");

        // Dry run should not modify anything.
        run_bump(&ws.root, BumpTarget::Minor, "2026-11-19", true, true).unwrap();
        assert_eq!(
            read_workspace_version(&ws.root).unwrap().to_string(),
            "2.1.2"
        );

        // Real bump (with --no-test since this is a fixture temp dir).
        run_bump(&ws.root, BumpTarget::Minor, "2026-11-19", false, true).unwrap();
        assert_eq!(
            read_workspace_version(&ws.root).unwrap().to_string(),
            "2.2.0"
        );

        // --check must pass after bump.
        run_check(&ws.root).unwrap();

        // Verify specific formatting details across all 6 files.
        let pkgbuild = fs::read_to_string(ws.root.join(TARGET_PKGBUILD)).unwrap();
        assert!(pkgbuild.contains("pkgver=2.2.0\n"));
        assert!(pkgbuild.contains("pkgrel=1\n"), "pkgrel must reset to 1");

        let spec = fs::read_to_string(ws.root.join(TARGET_SPEC)).unwrap();
        assert!(spec.contains("Version:        2.2.0\n"));

        let desktop = fs::read_to_string(ws.root.join(TARGET_DESKTOP)).unwrap();
        assert!(
            desktop.contains("Version=1.0\n"),
            "Desktop Entry spec Version must stay 1.0"
        );
        assert!(desktop.contains("X-AppImage-Version=2.2.0\n"));

        let metainfo = fs::read_to_string(ws.root.join(TARGET_METAINFO)).unwrap();
        assert!(metainfo.contains("<release version=\"2.2.0\" date=\"2026-11-19\"/>"));

        let install = fs::read_to_string(ws.root.join(TARGET_INSTALL_SH)).unwrap();
        assert!(install.contains("VERSION=\"${VERSION:-2.2.0}\"\n"));
        assert!(
            install.contains("sed -n"),
            "sed extraction line in install.sh must remain untouched"
        );

        let inno = fs::read_to_string(ws.root.join(TARGET_INNO_SETUP)).unwrap();
        assert!(inno.contains("#define MyAppVersion \"2.2.0\"\n"));
        assert!(inno.contains("AppVersion={#MyAppVersion}\n"));
    }

    #[test]
    fn inno_setup_version_update_preserves_crlf_and_rejects_malformed() {
        let crlf = "#define MyAppName \"SoundHop\"\r\n#define MyAppVersion \"2.2.0\"\r\n";
        let (old, updated) = update_inno_setup(crlf, "2.3.0", "2026-11-20").unwrap();
        assert_eq!(old, "2.2.0");
        assert_eq!(
            updated,
            "#define MyAppName \"SoundHop\"\r\n#define MyAppVersion \"2.3.0\"\r\n"
        );

        let dup = "#define MyAppVersion \"2.2.0\"\n#define MyAppVersion \"2.2.0\"\n";
        assert!(update_inno_setup("#define MyAppVersion \"\"\n", "2.3.0", "").is_err());
        assert!(update_inno_setup("#define MyAppVersion \"2.2.0\n", "2.3.0", "").is_err());
        assert!(update_inno_setup(dup, "2.3.0", "").is_err());
    }

    #[test]
    fn check_fails_loudly_when_any_single_file_drifts() {
        let targets: [(&str, &str, &str); 6] = [
            (TARGET_PKGBUILD, "pkgver=2.1.2", "pkgver=2.0.0"),
            (
                TARGET_SPEC,
                "Version:        2.1.2",
                "Version:        2.0.0",
            ),
            (
                TARGET_DESKTOP,
                "X-AppImage-Version=2.1.2",
                "X-AppImage-Version=2.0.0",
            ),
            (TARGET_METAINFO, "version=\"2.1.2\"", "version=\"2.0.0\""),
            (
                TARGET_INSTALL_SH,
                "VERSION=\"${VERSION:-2.1.2}\"",
                "VERSION=\"${VERSION:-2.0.0}\"",
            ),
            (
                TARGET_INNO_SETUP,
                "#define MyAppVersion \"2.1.2\"",
                "#define MyAppVersion \"2.0.0\"",
            ),
        ];

        for (rel_path, from, to) in targets {
            let ws = TempWorkspace::new("2.1.2");
            let full = ws.root.join(rel_path);
            let content = fs::read_to_string(&full).unwrap().replace(from, to);
            fs::write(&full, content).unwrap();

            let err = run_check(&ws.root).unwrap_err();
            assert!(
                err.contains(rel_path) && err.contains("2.1.2") && err.contains("2.0.0"),
                "expected clear drift error for {rel_path}, got: {err}"
            );
        }
    }

    #[test]
    fn bump_is_atomic_and_fails_loudly_when_marker_missing() {
        let missing_marker_cases: [(&str, &str); 7] = [
            (TARGET_CARGO_TOML, "[workspace]\nmembers = []\n"),
            (TARGET_PKGBUILD, "pkgname=soundhop\npkgrel=1\n"),
            (TARGET_SPEC, "Name: soundhop\n"),
            (TARGET_DESKTOP, "[Desktop Entry]\nVersion=1.0\n"),
            (
                TARGET_METAINFO,
                "<component><releases></releases></component>\n",
            ),
            (TARGET_INSTALL_SH, "#!/usr/bin/env bash\necho hi\n"),
            (TARGET_INNO_SETUP, "[Setup]\nAppName=SoundHop\n"),
        ];

        for (rel_path, broken_content) in missing_marker_cases {
            let ws = TempWorkspace::new("2.1.2");
            let full = ws.root.join(rel_path);
            fs::write(&full, broken_content).unwrap();

            let err = if rel_path == TARGET_CARGO_TOML {
                run_bump(&ws.root, BumpTarget::Patch, "2026-11-19", false, true).unwrap_err()
            } else {
                plan_bump(&ws.root, SemVer::parse("2.1.3").unwrap(), "2026-11-19").unwrap_err()
            };
            assert!(
                err.contains(rel_path),
                "expected error mentioning {rel_path}, got: {err}"
            );

            // Ensure no other file was partially mutated.
            if rel_path != TARGET_CARGO_TOML {
                assert_eq!(
                    read_workspace_version(&ws.root).unwrap().to_string(),
                    "2.1.2",
                    "Cargo.toml must not be modified when {rel_path} fails validation"
                );
            }
            if rel_path != TARGET_SPEC {
                let spec = fs::read_to_string(ws.root.join(TARGET_SPEC)).unwrap();
                assert!(
                    spec.contains("Version:        2.1.2"),
                    "soundhop.spec must not be modified when {rel_path} fails validation"
                );
            }
        }
    }
}
