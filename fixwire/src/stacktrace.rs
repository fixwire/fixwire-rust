//! Stacks as frames: names demangled and split into module and function,
//! the app's frames told from the standard library's and dependencies', and
//! source lines around the app's.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use backtrace::Backtrace;
use regex::Regex;

use crate::hub::lock;
use crate::options::Options;
use crate::types::Frame;

/// Bounds a captured stack.
const MAX_FRAMES: usize = 100;

/// The calling thread's stack, the oldest call first, without the SDK's own
/// frames.
pub(crate) fn capture(opts: &Options) -> Vec<Frame> {
    frames_of(&Backtrace::new(), opts, false, &[])
}

/// The calling thread's stack without the frames of the given crates either
/// (an integration's library, such as `tracing`'s).
#[cfg_attr(not(feature = "tracing"), allow(dead_code))]
pub(crate) fn capture_skipping(opts: &Options, crates: &[&str]) -> Vec<Frame> {
    frames_of(&Backtrace::new(), opts, false, crates)
}

/// The stack of a panic, from inside the panic hook: the frames below the
/// panic machinery.
pub(crate) fn capture_panic(opts: &Options) -> Vec<Frame> {
    frames_of(&Backtrace::new(), opts, true, &[])
}

/// Functions of the panic machinery, newest first in a panic's stack: what
/// is above the last of them is the hook's.
fn panic_machinery(function: &str) -> bool {
    [
        "std::panicking::",
        "core::panicking::",
        "std::panic::",
        "core::panic::",
        "std::sys::backtrace::",
        "std::sys_common::backtrace::",
        "rust_begin_unwind",
        "__rustc::rust_begin_unwind",
        "<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call",
    ]
    .iter()
    .any(|p| function.starts_with(p))
}

fn frames_of(bt: &Backtrace, opts: &Options, from_panic: bool, skip: &[&str]) -> Vec<Frame> {
    let mut newest = Vec::new(); // newest first, as the backtrace has them
    for frame in bt.frames() {
        for symbol in frame.symbols() {
            let function = symbol.name().map(|n| format!("{n:#}")).unwrap_or_default();
            let file = symbol.filename().map(|p| p.to_string_lossy().into_owned());
            newest.push((function, file, symbol.lineno(), symbol.colno()));
        }
    }
    if from_panic {
        // The hook's frames and the machinery that called it sit above where the code panicked;
        // a thread's own catch_unwind, far below, isn't part of it.
        if let Some(first) = newest.iter().position(|(f, ..)| panic_machinery(f)) {
            let end = newest[first..]
                .iter()
                .position(|(f, ..)| !panic_machinery(f))
                .map_or(newest.len(), |n| first + n);
            newest.drain(..end);
        }
    }
    let root = opts.project_root.as_deref();
    let main = main_crate();
    let mut frames: Vec<Frame> = newest
        .into_iter()
        .filter(|(function, file, ..)| {
            !function.is_empty()
                && !is_sdk(function)
                && !is_sdk_file(file.as_deref())
                && !skip.contains(&root_crate(function))
        })
        .take(MAX_FRAMES)
        .map(|(function, abs_path, line, column)| {
            let (module, function) = split_function(&function);
            let in_app = in_app(
                module.as_deref(),
                abs_path.as_deref(),
                root,
                main.as_deref(),
                opts,
            );
            Frame {
                file: abs_path.as_deref().map(|p| short_file(p, root)),
                abs_path,
                function,
                module,
                line,
                column,
                in_app,
                ..Frame::default()
            }
        })
        .collect();
    frames.reverse();
    if opts.context_lines > 0 {
        for f in frames.iter_mut().filter(|f| f.in_app) {
            add_context(f, opts.context_lines);
        }
    }
    frames
}

/// The crate a function belongs to: the first path segment, after a
/// trait impl's `<`.
fn root_crate(path: &str) -> &str {
    let path = path.trim_start_matches(['<', '_']);
    path.split("::").next().unwrap_or(path)
}

/// Frames of the SDK and of the backtrace it takes.
fn is_sdk(function: &str) -> bool {
    matches!(root_crate(function), "fixwire" | "backtrace")
}

/// Splits `shop_api::cart::charge` into `shop_api::cart` and `charge`, at the
/// last `::` outside a type's angle brackets. Closures and async blocks lose
/// their index (`{closure#1}`, or `{{closure}}` in legacy symbols, is
/// `{closure}`): it moves when another closure is written before them.
pub(crate) fn split_function(name: &str) -> (Option<String>, String) {
    let name = without_generics(&without_indexes(name));
    let (mut depth, mut split) = (0usize, None);
    let bytes = name.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => split = Some(i),
            _ => {}
        }
    }
    match split {
        Some(i) if i > 0 => (Some(name[..i].to_owned()), name[i + 2..].to_owned()),
        _ => (None, name),
    }
}

/// A name without the generic arguments of its functions: `catch_unwind::<…>`
/// and `index<&str>` are `catch_unwind` and `index`. A trait impl's
/// `<Type as Trait>` stays: it names where the function is.
fn without_generics(name: &str) -> String {
    let bytes = name.as_bytes();
    let (mut out, mut copied, mut i) = (String::with_capacity(name.len()), 0, 0);
    while i < bytes.len() {
        let turbofish = bytes[i..].starts_with(b"::<");
        // Arguments right after a name (`index<&str>`), not a path's leading `<Type as Trait>`.
        let trailing = bytes[i] == b'<'
            && i > 0
            && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !(turbofish || trailing) {
            i += 1;
            continue;
        }
        // The brackets are ASCII: these are character boundaries.
        out.push_str(&name[copied..i]);
        let mut depth = 0usize;
        let mut j = if turbofish { i + 2 } else { i };
        while j < bytes.len() {
            match bytes[j] {
                b'<' => depth += 1,
                b'>' if depth == 1 => break,
                b'>' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        i = j + 1;
        copied = i.min(bytes.len());
    }
    out.push_str(&name[copied..]);
    out
}

/// `{closure#0}` and `{{closure}}` as `{closure}`, `{async_block#2}` as
/// `{async_block}`, `{shim:vtable#0}` as `{shim:vtable}`.
fn without_indexes(name: &str) -> String {
    static INDEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\{\{([a-z_]+)\}\}|\{([a-z_:]+)#[0-9]+\}").expect("a valid pattern")
    });
    INDEX
        .replace_all(name, |c: &regex::Captures<'_>| {
            format!(
                "{{{}}}",
                c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str())
            )
        })
        .into_owned()
}

/// Whether a frame is the app's: the options say so; else, with debug
/// information, its file is the app's (not the standard library's, a
/// registry's or a git dependency's); else its crate is the binary's.
fn in_app(
    module: Option<&str>,
    file: Option<&str>,
    root: Option<&Path>,
    main: Option<&str>,
    opts: &Options,
) -> bool {
    let module = module.unwrap_or_default();
    if opts
        .in_app_exclude
        .iter()
        .any(|p| module.starts_with(p.as_str()))
    {
        return false;
    }
    if opts
        .in_app_include
        .iter()
        .any(|p| module.starts_with(p.as_str()))
    {
        return true;
    }
    let krate = root_crate(module);
    if matches!(krate, "std" | "core" | "alloc" | "proc_macro" | "test") {
        return false;
    }
    match file.map(|f| f.replace('\\', "/")) {
        // Builds with little debug information name functions without their module: the file
        // still tells.
        Some(file) => {
            !is_dependency(&file)
                && root.is_none_or(|r| {
                    let r = r.to_string_lossy().replace('\\', "/");
                    file.starts_with(&format!("{}/", r.trim_end_matches('/')))
                        || !file.starts_with('/')
                })
        }
        None => !krate.is_empty() && main == Some(krate),
    }
}

/// Files of the standard library (wherever the toolchain keeps its sources),
/// of crates from a registry or git, and vendored ones.
fn is_dependency(file: &str) -> bool {
    file.contains("/.cargo/registry/")
        || file.contains("/.cargo/git/")
        || file.starts_with("/rustc/")
        || [
            "/library/std/",
            "/library/core/",
            "/library/alloc/",
            "/library/proc_macro/",
        ]
        .iter()
        .any(|l| file.contains(l))
        || file.contains("/vendor/")
}

/// The SDK's own sources, where it was built (`file!()` follows
/// `--remap-path-prefix`, as the debug information does).
fn is_sdk_file(file: Option<&str>) -> bool {
    static SDK_SRC: LazyLock<String> = LazyLock::new(|| {
        let me = file!().replace('\\', "/");
        me.rsplit_once('/')
            .map_or(String::new(), |(dir, _)| format!("{dir}/"))
    });
    let Some(file) = file else { return false };
    let file = file.replace('\\', "/");
    (!SDK_SRC.is_empty()
        && (file.starts_with(SDK_SRC.as_str()) || file.ends_with(SDK_SRC.trim_start_matches('/'))))
        || file.contains("/backtrace-0.")
}

/// The binary's crate, from its file name (`shop-api` is `shop_api`).
fn main_crate() -> Option<String> {
    static MAIN: LazyLock<Option<String>> = LazyLock::new(|| {
        let exe = std::env::current_exe().ok()?;
        Some(exe.file_stem()?.to_string_lossy().replace('-', "_"))
    });
    MAIN.clone()
}

/// A frame's file as people read it, with forward slashes: relative to the
/// project root; a dependency's from its crate (`tokio-1.47.1/src/…`); the
/// standard library's from `library/`.
fn short_file(path: &str, root: Option<&Path>) -> String {
    let path = path.replace('\\', "/");
    if let Some(root) = root {
        let root = root.to_string_lossy().replace('\\', "/");
        if let Some(rest) = path.strip_prefix(&format!("{}/", root.trim_end_matches('/'))) {
            return rest.to_owned();
        }
    }
    for marker in ["/.cargo/registry/src/", "/.cargo/git/checkouts/"] {
        if let Some(i) = path.find(marker) {
            let rest = &path[i + marker.len()..];
            // past the index's or the checkout's directory
            return rest.split_once('/').map_or(rest, |(_, r)| r).to_owned();
        }
    }
    if let Some(i) = path.find("/library/") {
        return path[i + 1..].to_owned();
    }
    path
}

/// The lines of the files frames point to, kept for the next events.
static SOURCES: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);
const MAX_SOURCE_FILES: usize = 64;
const MAX_SOURCE_BYTES: u64 = 1 << 20;

fn add_context(f: &mut Frame, around: usize) {
    let (Some(path), Some(line)) = (f.abs_path.as_deref(), f.line) else {
        return;
    };
    let mut sources = lock(&SOURCES);
    if !sources.contains_key(path) {
        if sources.len() >= MAX_SOURCE_FILES {
            sources.clear();
        }
        let lines = std::fs::metadata(path)
            .ok()
            .filter(|m| m.len() <= MAX_SOURCE_BYTES)
            .and_then(|_| std::fs::read_to_string(path).ok())
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        sources.insert(path.to_owned(), lines);
    }
    let lines = &sources[path];
    let i = line as usize;
    if i == 0 || i > lines.len() {
        return;
    }
    let i = i - 1;
    f.context_line = Some(lines[i].clone());
    f.pre_context = lines[i.saturating_sub(around)..i].to_vec();
    f.post_context = lines[i + 1..(i + 1 + around).min(lines.len())].to_vec();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_functions() {
        let cases = [
            ("shop_api::cart::charge", (Some("shop_api::cart"), "charge")),
            (
                "shop_api::main::{{closure}}",
                (Some("shop_api::main"), "{closure}"),
            ),
            (
                "shop_api::main::{closure#1}::{closure#0}",
                (Some("shop_api::main::{closure}"), "{closure}"),
            ),
            (
                "std::panic::catch_unwind::<&dyn core::ops::function::Fn<(), Output = i32>, i32>",
                (Some("std::panic"), "catch_unwind"),
            ),
            (
                "<alloc::vec::Vec<T,A> as core::ops::index::Index<I>>::index",
                (
                    Some("<alloc::vec::Vec as core::ops::index::Index>"),
                    "index",
                ),
            ),
            ("index<&str, usize>", (None, "index")),
            ("crème::brûlée::<u8>", (Some("crème"), "brûlée")),
            (
                "shop_api::checkout::{async_fn#0}",
                (Some("shop_api::checkout"), "{async_fn}"),
            ),
            (
                "<shop_api::Cart as core::fmt::Debug>::fmt",
                (Some("<shop_api::Cart as core::fmt::Debug>"), "fmt"),
            ),
            ("main", (None, "main")),
        ];
        for (name, (module, function)) in cases {
            assert_eq!(
                split_function(name),
                (module.map(str::to_owned), function.to_owned()),
                "{name}"
            );
        }
    }

    #[test]
    fn names_files_as_people_read_them() {
        let root = Path::new("/srv/shop");
        let cases = [
            ("/srv/shop/src/cart.rs", "src/cart.rs"),
            (
                "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.47.1/src/runtime/task.rs",
                "tokio-1.47.1/src/runtime/task.rs",
            ),
            (
                "/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/std/src/panicking.rs",
                "library/std/src/panicking.rs",
            ),
            ("C:\\srv\\other\\main.rs", "C:/srv/other/main.rs"),
        ];
        for (path, short) in cases {
            assert_eq!(short_file(path, Some(root)), short, "{path}");
        }
        assert_eq!(
            short_file(
                "C:\\srv\\shop\\src\\cart.rs",
                Some(Path::new("C:\\srv\\shop"))
            ),
            "src/cart.rs"
        );
    }

    #[test]
    fn tells_the_apps_frames() {
        let o = Options::default();
        let root = Some(Path::new("/srv/shop"));
        assert!(in_app(
            Some("shop::cart"),
            Some("/srv/shop/src/cart.rs"),
            root,
            None,
            &o
        ));
        assert!(!in_app(
            Some("tokio::runtime"),
            Some("/home/u/.cargo/registry/src/x/tokio-1/src/a.rs"),
            root,
            None,
            &o
        ));
        assert!(!in_app(
            Some("std::rt"),
            Some("/rustc/abc/library/std/src/rt.rs"),
            root,
            None,
            &o
        ));
        assert!(
            in_app(Some("shop::cart"), None, root, Some("shop"), &o),
            "no debug information: the binary's crate"
        );
        assert!(!in_app(Some("serde::de"), None, root, Some("shop"), &o));
        let o = Options {
            in_app_include: vec!["serde::".into()],
            ..Options::default()
        };
        assert!(in_app(Some("serde::de"), None, root, Some("shop"), &o));
    }
}
