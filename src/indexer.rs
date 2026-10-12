//! File and application indexing for Trandex.
//!
//! Files are indexed from `$HOME`. `fd` is preferred; when it is not
//! available we fall back to `find`. Applications are parsed straight from the
//! XDG `applications/` directories.
//!
//! Everything in this module is plain `std` — no GTK/GIO types — so it can run
//! on worker threads and hand results back as [`Item`]s that are cheap to move
//! across threads. Lowercasing (the expensive part of indexing) and the final
//! sort happen here, off the GTK main thread.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Number of items to hand to the batch callback at once.
const BATCH_SIZE: usize = 512;

/// Options controlling what gets indexed.
#[derive(Debug, Clone)]
pub struct Options {
    /// Directory names to skip entirely.
    pub ignore: Vec<String>,
    /// Whether hidden files are included.
    pub hidden: bool,
}

/// A single searchable result: either an indexed file or a launchable app.
///
#[derive(Clone)]
pub struct Item {
    /// How the result is displayed and launched.
    pub source: Source,
}

/// The concrete backing of an [`Item`].
#[derive(Clone)]
pub enum Source {
    /// A file on disk, opened with `handlr`/`xdg-open`.
    File(PathBuf),
    /// A parsed `.desktop` entry, converted into a GIO `AppInfo` at launch time.
    App(DesktopEntry),
    /// An executable discovered in `$PATH`.
    Command(PathBuf),
}

impl Item {
    /// Build a file item without duplicating its path into display/search strings.
    pub fn file(path: PathBuf) -> Self {
        Self { source: Source::File(path) }
    }

    /// Build an app item from a parsed `.desktop` entry.
    pub fn app(entry: DesktopEntry) -> Option<Self> {
        if entry.name.is_empty() {
            return None;
        }
        Some(Self { source: Source::App(entry) })
    }

    /// The primary label, derived from a file path when needed.
    pub fn name(&self) -> Cow<'_, str> {
        match &self.source {
            Source::File(path) => path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| path.to_string_lossy()),
            Source::App(entry) => Cow::Borrowed(&entry.name),
            Source::Command(path) => path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| path.to_string_lossy()),
        }
    }

    /// The dimmed line shown beneath the name.
    pub fn subtitle(&self) -> Cow<'_, str> {
        match &self.source {
            Source::File(path) => path.to_string_lossy(),
            Source::App(entry) => Cow::Borrowed(&entry.id),
            Source::Command(path) => path.to_string_lossy(),
        }
    }
}

/// A minimal parse of a `.desktop` entry, enough to display, filter and launch.
///
/// Plain `String` fields so entries can cross thread boundaries cheaply; GIO
/// types are created from this only in the `launch()` path.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// `Name`: the display name.
    pub name: String,
    /// The desktop id, normally the file name (`firefox.desktop`).
    pub id: String,
    /// `Icon`: a themed icon name when present.
    pub icon: Option<String>,
    /// `Exec`: the command line used to launch.
    pub exec: String,
    /// `NoDisplay=true`: never shown in menus/launchers.
    #[allow(dead_code)] // part of the parsed entry, matched by the skip logic
    pub no_display: bool,
    /// `Hidden=true`: should be deleted, so never shown.
    #[allow(dead_code)]
    pub hidden: bool,
}

fn stream_lines(
    program: &str,
    args: &[String],
    options: &Options,
    on_batch: &mut impl FnMut(Vec<Item>),
) -> (bool, usize) {
    let spawned = std::time::Instant::now();
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (false, 0),
    };
    let mut items = Vec::with_capacity(BATCH_SIZE);
    let mut count = 0;
    let mut item_build = std::time::Duration::ZERO;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let item_started = std::time::Instant::now();
            let path = PathBuf::from(line);
            if path.as_os_str().is_empty() || is_ignored_path(&path, options) {
                continue;
            }
            items.push(Item::file(path));
            item_build += item_started.elapsed();
            count += 1;
            if items.len() == BATCH_SIZE {
                on_batch(std::mem::take(&mut items));
            }
        }
    }
    if !items.is_empty() {
        on_batch(items);
    }
    let eof_elapsed = spawned.elapsed();
    let succeeded = child.wait().is_ok_and(|status| status.success());
    debug_log(&format!(
        "{program}: spawn_to_eof_ms={} item_build_ms={} items={count}",
        eof_elapsed.as_millis(),
        item_build.as_millis()
    ));
    (succeeded, count)
}

fn is_ignored_path(path: &Path, options: &Options) -> bool {
    let components: Vec<_> = path.components().collect();
    components.iter().enumerate().any(|(index, component)| {
        let name = component.as_os_str().to_string_lossy();
        if options.ignore.iter().any(|ignored| ignored == name.as_ref()) {
            return true;
        }
        if name == "target" {
            let project_root = components[..index].iter().fold(PathBuf::new(), |mut path, component| {
                path.push(component.as_os_str());
                path
            });
            return project_root.join("Cargo.toml").is_file();
        }
        !options.hidden && name.starts_with('.') && name != "." && name != ".."
    })
}

/// Index the home directory, invoking `on_batch` with ready-to-use batches.
///
/// Paths are converted into items and emitted as fd/find produces them. No
/// full-list sort or path metadata pass is needed because filtering ranks the
/// results later.
pub fn index_home_streaming(options: &Options, mut on_batch: impl FnMut(Vec<Item>)) {
    let home = match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home),
        None => return,
    };

    let (succeeded, count) = if command_exists("fd") {
        let result = stream_lines("fd", &fd_args(&home, options), options, &mut on_batch);
        if result.0 && result.1 > 0 {
            result
        } else {
            debug_log("fd failed or returned no paths; falling back to find");
            stream_lines("find", &find_args(&home, options), options, &mut on_batch)
        }
    } else {
        debug_log("fd not found; using find");
        stream_lines("find", &find_args(&home, options), options, &mut on_batch)
    };
    debug_log(&format!("file index backend success={succeeded} items={count}"));

    /// Enforce directory-name exclusions independently of the indexing backend.
    ///
    /// `fd` and `find` have different matching/pruning semantics, so applying the
    /// configured names to the returned paths keeps both backends consistent.
    fn is_ignored_path(path: &Path, options: &Options) -> bool {
        let components: Vec<_> = path.components().collect();
        components.iter().enumerate().any(|(index, component)| {
            let name = component.as_os_str().to_string_lossy();
            if options.ignore.iter().any(|ignored| ignored == name.as_ref()) {
                return true;
            }

            fn stream_lines(
                program: &str,
                args: &[String],
                options: &Options,
                on_batch: &mut impl FnMut(Vec<Item>),
            ) -> (bool, usize) {
                let spawned = std::time::Instant::now();
                let mut child = match Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(_) => return (false, 0),
                };
                let stdout = child.stdout.take();
                let mut items = Vec::with_capacity(BATCH_SIZE);
                let mut count = 0;
                let build_started = std::time::Instant::now();
                if let Some(stdout) = stdout {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        let path = PathBuf::from(line);
                        if path.as_os_str().is_empty() || is_ignored_path(&path, options) {
                            continue;
                        }
                        items.push(Item::file(path));
                        count += 1;
                        if items.len() == BATCH_SIZE {
                            on_batch(std::mem::take(&mut items));
                        }
                    }
                }
                if !items.is_empty() {
                    on_batch(items);
                }
                let succeeded = child.wait().is_ok_and(|status| status.success());
                debug_log(&format!(
                    "{program}: spawn_to_eof_ms={} item_build_ms={} items={count}",
                    spawned.elapsed().as_millis(),
                    build_started.elapsed().as_millis()
                ));
                (succeeded, count)
            }

            fn is_ignored_path(path: &Path, options: &Options) -> bool {
                let components: Vec<_> = path.components().collect();
                components.iter().enumerate().any(|(index, component)| {
                    let name = component.as_os_str().to_string_lossy();
                    if options.ignore.iter().any(|ignored| ignored == name.as_ref()) {
                        return true;
                    }
                    if name == "target" {
                        let project_root = components[..index].iter().fold(PathBuf::new(), |mut path, component| {
                            path.push(component.as_os_str());
                            path
                        });
                        return project_root.join("Cargo.toml").is_file();
                    }
                    !options.hidden && name.starts_with('.') && name != "." && name != ".."
                })
            }
            if name == "target" {
                let project_root = components[..index].iter().fold(PathBuf::new(), |mut path, component| {
                    path.push(component.as_os_str());
                    path
                });
                return project_root.join("Cargo.toml").is_file();
            }
            if !options.hidden && name.starts_with('.') && name != "." && name != ".." {
                return true;
            }
            false
        })
    }
}

/// Parse every `.desktop` file from `$XDG_DATA_HOME/applications` and each
/// `$XDG_DATA_DIRS/applications` directory, in precedence order.
///
/// Mirrors the subset of GIO's `should_show()` that matters here: entries with
/// `NoDisplay=true`, `Hidden=true`, `OnlyShowIn`/`NotShowIn` mismatches, or no
/// `Exec` are skipped, and the first directory wins for duplicate ids.
pub fn desktop_apps() -> Vec<DesktopEntry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for directory in applications_dirs() {
        let Ok(read) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(|item| item.ok().map(|item| item.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
            .collect();
        files.sort();

        for path in files {
            let Some(file_name) = path.file_name().map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !seen.insert(file_name.clone()) {
                continue;
            }

            if let Some(entry) = parse_desktop_file(&path, &file_name) {
                entries.push(entry);
            }
        }
    }

    entries
}

/// Return unique executable files available on `$PATH`, preserving path order.
pub fn path_commands() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else { return Vec::new() };
    let mut seen = HashSet::new();
    std::env::split_paths(&path)
        .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten())
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && is_executable(path)
                && path.file_name().is_some_and(|name| seen.insert(name.to_os_string()))
        })
        .collect()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `fd` arguments for a regular-file walk over `home`.
fn fd_args(home: &Path, options: &Options) -> Vec<String> {
    let mut args = vec![
        "--type".to_string(),
        "f".to_string(),
        "--absolute-path".to_string(),
        "--color=never".to_string(),
    ];

    if options.hidden {
        args.push("--hidden".to_string());
    }

    for dir in &options.ignore {
        args.push("--exclude".to_string());
        args.push(dir.clone());
    }

    // `.` matches every path.
    args.push(".".to_string());
    args.push(home.to_string_lossy().to_string());
    args
}

/// `find` arguments as a fallback for `fd`.
fn find_args(home: &Path, options: &Options) -> Vec<String> {
    let mut args = vec![home.to_string_lossy().to_string()];

    if options.ignore.is_empty() {
        args.push("-type".to_string());
        args.push("f".to_string());
    } else {
        // Prune every ignored directory, then print the remaining regular files.
        args.push("(".to_string());
        for (index, dir) in options.ignore.iter().enumerate() {
            if index > 0 {
                args.push("-o".to_string());
            }
            args.push("-type".to_string());
            args.push("d".to_string());
            args.push("-name".to_string());
            args.push(dir.clone());
        }
        args.push(")".to_string());
        args.push("-prune".to_string());
        args.push("-o".to_string());
        args.push("-type".to_string());
        args.push("f".to_string());
        args.push("-print".to_string());
    }

    args
}

/// Sort paths in place with `Path`'s component-wise ordering, without touching
/// scattered heap strings for every comparison.
///
/// A naive `paths.sort_unstable()` over ~500k paths dereferences two heap
/// allocations per comparison (each `PathBuf` owns its `OsString`) and scans
/// components, which costs ~3 s on disk walks the size of a home directory and
/// previously ran on the GTK main thread at `Done`. Here every path is packed
/// into one contiguous arena and compared with a byte scan that exits at the
/// first differing byte, which is ~8x faster.
///
/// `Path`'s component-wise ordering differs from a plain byte sort only when
/// the first differing byte is a `/` in one path but not the other (one
/// component is a prefix of the other's, e.g. `foo/…` vs `foo.bar/…`); the
/// comparator below encodes exactly that rule. Relative order of equal paths
/// is unspecified.
fn sort_paths(paths: &mut Vec<PathBuf>) {
    if paths.len() < 2 {
        return;
    }

    let mut arena = Vec::<u8>::new();
    // (start, len, original index) — sorted directly for cache locality.
    let mut keys = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let bytes = path.as_os_str().as_encoded_bytes();
        let start = arena.len();
        arena.extend_from_slice(bytes);
        keys.push((start as u32, bytes.len() as u32, index as u32));
    }

    keys.sort_unstable_by(|&(a_start, a_len, _), &(b_start, b_len, _)| {
        let a = &arena[a_start as usize..(a_start + a_len) as usize];
        let b = &arena[b_start as usize..(b_start + b_len) as usize];
        cmp_path_bytes(a, b)
    });

    // Reorder the original vector by moving each path to its sorted slot.
    let mut sorted = Vec::with_capacity(paths.len());
    for (_, _, index) in keys {
        sorted.push(std::mem::take(&mut paths[index as usize]));
    }
    *paths = sorted;
}

/// Compare two absolute paths (as bytes) with `Path`'s ordering, exiting at the
/// first differing byte instead of walking every component.
///
/// - one path is a byte-prefix of the other → the shorter sorts first, which
///   matches `Path` (a component that is a prefix of another also sorts first);
/// - the first differing byte is `/` in exactly one path → that path's
///   component ends there, so it is a component-prefix and sorts first;
/// - otherwise the plain byte comparison is what `Path` would do.
fn cmp_path_bytes(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let common = a.len().min(b.len());
    for i in 0..common {
        let a_byte = a[i];
        let b_byte = b[i];
        if a_byte != b_byte {
            if a_byte == b'/' {
                return std::cmp::Ordering::Less;
            }
            if b_byte == b'/' {
                return std::cmp::Ordering::Greater;
            }
            return a_byte.cmp(&b_byte);
        }
    }
    a.len().cmp(&b.len())
}

/// Run `program` and collect all of its stdout, one path per line.
fn collect_lines(program: &str, args: &[String]) -> (Vec<PathBuf>, bool) {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (Vec::new(), false),
    };

    let mut paths = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !line.is_empty() {
                paths.push(PathBuf::from(line));
            }
        }
    }

    let succeeded = child.wait().is_ok_and(|status| status.success());
    (paths, succeeded)
}

fn debug_log(message: &str) {
    if std::env::var_os("TRANDEX_DEBUG").is_some() {
        eprintln!("trandex: {message}");
    }
}

/// The `applications` directories from the XDG data dirs, in precedence order.
fn applications_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        });
    if let Some(base) = data_home {
        dirs.push(base.join("applications"));
    }

    let data_dirs = match std::env::var_os("XDG_DATA_DIRS") {
        Some(value) if !value.is_empty() => std::env::split_paths(&value).collect(),
        _ => vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    };
    for base in data_dirs {
        dirs.push(base.join("applications"));
    }

    dirs
}

/// Parse a single `.desktop` file. Returns `None` when the entry should not be
/// shown (hidden, no name, no `Exec`, or a non-application type).
fn parse_desktop_file(path: &Path, id: &str) -> Option<DesktopEntry> {
    let text = std::fs::read_to_string(path).ok()?;

    let mut entry_type = String::new();
    let mut name = String::new();
    let mut icon = None;
    let mut exec = String::new();
    let mut no_display = false;
    let mut hidden = false;
    let mut only_show_in: Vec<String> = Vec::new();
    let mut not_show_in: Vec<String> = Vec::new();
    let mut in_desktop_entry = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Only the main group applies to the launchable entry; keys in
            // `Desktop Action ...` groups describe menu actions, not the app.
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => entry_type = value.to_string(),
            "Name" if name.is_empty() => name = value.to_string(),
            "Icon" if icon.is_none() => icon = Some(value.to_string()),
            "Exec" if exec.is_empty() => exec = value.to_string(),
            "NoDisplay" => no_display = parse_bool(value),
            "Hidden" => hidden = parse_bool(value),
            "OnlyShowIn" => only_show_in.extend(semicolon_list(value)),
            "NotShowIn" => not_show_in.extend(semicolon_list(value)),
            _ => {}
        }
    }

    // Not launchable: no name, no command, or not an application entry.
    if name.is_empty() || exec.is_empty() {
        return None;
    }
    if entry_type == "Link" || entry_type == "Directory" {
        return None;
    }
    if no_display || hidden {
        return None;
    }

    // OnlyShowIn / NotShowIn against the current desktop, as GIO does.
    if let Some(current) = std::env::var_os("XDG_CURRENT_DESKTOP") {
        let current = current.to_string_lossy().to_lowercase();
        if !only_show_in.is_empty() && !only_show_in.iter().any(|desktop| desktop.to_lowercase() == current)
        {
            return None;
        }
        if not_show_in.iter().any(|desktop| desktop.to_lowercase() == current) {
            return None;
        }
    }

    Some(DesktopEntry {
        name,
        id: id.to_string(),
        icon,
        exec,
        no_display,
        hidden,
    })
}

/// `true` for `true|1` (case-insensitive), as the spec allows.
fn parse_bool(value: &str) -> bool {
    value.eq_ignore_ascii_case("true") || value == "1"
}

/// Split a `;`-separated list value, dropping empty entries.
fn semicolon_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Return `true` if `name` resolves to a file somewhere on `$PATH`.
fn command_exists(name: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(path) => path,
        None => return false,
    };

    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}