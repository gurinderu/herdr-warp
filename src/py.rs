//! Small pieces of Python semantics the original scripts relied on.
//!
//! The scripts this crate replaces leaned on Python's str methods, `or`
//! truthiness, `repr()` in log lines and `os.path.realpath`. Re-creating them
//! here keeps the port's behaviour (and its log output) the same.

use crate::unicode_tables::{DECIMAL, DIGIT, DROP, SPACE};
use serde_json::Value;
use std::io;
use std::path::Path;

fn in_table(c: char, table: &[(u32, u32)]) -> bool {
    let cp = c as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// General category Cc, Cf, Co, Cs, Cn, Zl or Zp.
pub fn is_drop(c: char) -> bool {
    in_table(c, DROP)
}

/// Python `str.isspace()` for one character.
pub fn is_space(c: char) -> bool {
    in_table(c, SPACE)
}

/// Python `re` `\d` on str (a decimal digit of any script).
pub fn is_decimal(c: char) -> bool {
    in_table(c, DECIMAL)
}

/// Python `str.isdigit()`: non-empty and every character a digit.
pub fn isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| in_table(c, DIGIT))
}

/// Python `str.split()` with no arguments.
pub fn split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|w| !w.is_empty())
}

/// Python `str.split(None, maxsplit)`.
pub fn split_ws_max(s: &str, maxsplit: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s.trim_start_matches(is_space);
    while !rest.is_empty() {
        if out.len() == maxsplit {
            out.push(rest);
            break;
        }
        match rest.find(is_space) {
            Some(i) => {
                out.push(&rest[..i]);
                rest = rest[i..].trim_start_matches(is_space);
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

/// Python `str.rstrip()` with no arguments.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// The first `n` characters (Python `s[:n]`).
pub fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Marks a spot where the Python original would have raised (a TypeError
/// on an unexpected JSON shape, say). The hooks then stop quietly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised;

/// Python truthiness of a decoded JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `d.get(key)`: JSON null and a missing key are both None.
pub fn get<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a Value> {
    match obj.get(key) {
        None | Some(Value::Null) => None,
        v => v,
    }
}

/// `a or b or ...`: the first truthy value, else the last one.
pub fn or<'a>(vals: &[Option<&'a Value>]) -> Option<&'a Value> {
    for v in vals {
        if truthy(*v) {
            return *v;
        }
    }
    vals.last().copied().flatten()
}

/// Python `isinstance(v, int) and not isinstance(v, bool)`.
pub fn as_int(v: Option<&Value>) -> Option<i128> {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from)),
        _ => None,
    }
}

/// Elements of `d.get(key, [])` used as `a + b`: a missing key gives [], a
/// list gives its items, anything else is a TypeError in Python (Err here).
pub fn list_for_concat<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], Raised> {
    match obj.get(key) {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(Raised),
    }
}

/// Elements of `d.get(key, [])` used in a `for` that only keeps dicts:
/// strings and dicts iterate to non-dicts (nothing kept); numbers, bools and
/// null raise TypeError in Python (Err here).
pub fn list_for_iter<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], Raised> {
    match obj.get(key) {
        None | Some(Value::String(_)) | Some(Value::Object(_)) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(Raised),
    }
}

fn printable(c: char) -> bool {
    c == ' ' || !(is_drop(c) || is_space(c))
}

/// Python `repr()` of a str.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if printable(c) => out.push(c),
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of a decoded JSON value.
pub fn repr_value(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => repr_float(f),
            _ => n.to_string(),
        },
        Value::String(s) => repr_str(s),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(repr_value).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(o) => {
            let items: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr_value(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Python `repr()` of a float: shortest round-trip digits, exponent form
/// from 1e16 up and below 1e-4, at least two exponent digits.
pub fn repr_float(f: f64) -> String {
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        let e = format!("{f:e}");
        let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
        let (sign, digits) = match exp.strip_prefix('-') {
            Some(d) => ('-', d),
            None => ('+', exp),
        };
        format!("{mant}e{sign}{digits:0>2}")
    } else if f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Python `str()` of a decoded JSON value (`"%s" % v`).
pub fn str_value(v: Option<&Value>) -> String {
    match v {
        None => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => repr_value(v),
    }
}

/// Python's text for an OSError: `[Errno 2] No such file or directory: '/x'`.
pub fn os_error(err: &io::Error, path: Option<&str>) -> String {
    match err.raw_os_error() {
        Some(code) => {
            let msg = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }
                .to_string_lossy()
                .into_owned();
            match path {
                Some(p) => format!("[Errno {code}] {msg}: {}", repr_str(p)),
                None => format!("[Errno {code}] {msg}"),
            }
        }
        None => err.to_string(),
    }
}

/// The home directory: $HOME when set and non-empty, else the passwd entry.
pub fn home_dir(home_env: Option<&str>) -> String {
    if let Some(h) = home_env
        && !h.is_empty()
    {
        return h.to_string();
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_dir.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned();
        }
    }
    "~".into()
}

// ---------------------------------------------------------------- realpath

fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// posixpath.split
fn split(p: &str) -> (String, String) {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, tail) = (&p[..i], &p[i..]);
    let mut head = head.to_string();
    if !head.is_empty() && head != "/".repeat(head.len()) {
        head = head.trim_end_matches('/').to_string();
    }
    (head, tail.to_string())
}

/// posixpath.normpath
pub fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") {
        2
    } else if p.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(c);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let s = format!("{}{}", "/".repeat(initial), comps.join("/"));
    if s.is_empty() { ".".into() } else { s }
}

fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        normpath(p)
    } else {
        let cwd = std::env::current_dir()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        normpath(&join(&cwd, p))
    }
}

fn join_real(
    mut path: String,
    rest: &str,
    seen: &mut std::collections::HashMap<String, Option<String>>,
) -> (String, bool) {
    let mut rest = rest.to_string();
    if rest.starts_with('/') {
        rest = rest[1..].to_string();
        path = "/".into();
    }
    while !rest.is_empty() {
        let (name, tail) = match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].to_string()),
            None => (rest.clone(), String::new()),
        };
        rest = tail;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if !path.is_empty() {
                let (head, last) = split(&path);
                path = head;
                if last == ".." {
                    path = join(&join(&path, ".."), "..");
                }
            } else {
                path = "..".into();
            }
            continue;
        }
        let newpath = join(&path, &name);
        let is_link = std::fs::symlink_metadata(&newpath)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(prev) = seen.get(&newpath) {
            match prev {
                Some(p) => {
                    path = p.clone();
                    continue;
                }
                None => return (join(&newpath, &rest), false),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = match std::fs::read_link(&newpath) {
            Ok(t) => t.to_string_lossy().into_owned(),
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        let (p, ok) = join_real(path, &target, seen);
        path = p;
        if !ok {
            return (join(&path, &rest), false);
        }
        seen.insert(newpath, Some(path.clone()));
    }
    (path, true)
}

/// `os.path.realpath(p)` (non-strict): symlinks resolved as far as they
/// exist, the rest appended as is.
pub fn realpath(p: &str) -> String {
    let mut seen = std::collections::HashMap::new();
    let (path, _) = join_real(String::new(), p, &mut seen);
    abspath(&path)
}

/// `os.path.exists`.
pub fn exists(p: impl AsRef<Path>) -> bool {
    std::fs::metadata(p).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_like_python() {
        assert_eq!(
            split_ws(" a\u{3000}b\n c ").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(
            split_ws_max("  1  0 ??  /a b c", 3),
            ["1", "0", "??", "/a b c"]
        );
        assert_eq!(split_ws_max("x", 3), ["x"]);
        assert!(split_ws_max("   ", 1).is_empty());
    }

    #[test]
    fn digits() {
        assert!(isdigit("12"));
        assert!(isdigit("²"));
        assert!(!isdigit(""));
        assert!(!isdigit("1a"));
        assert!(is_decimal('٣'));
        assert!(!is_decimal('²'));
    }

    #[test]
    fn truthiness_and_or() {
        let a = json!("");
        let b = json!(0);
        let c = json!("x");
        assert_eq!(or(&[Some(&a), Some(&b), Some(&c)]), Some(&c));
        assert_eq!(or(&[Some(&a), None, Some(&b)]), Some(&b));
        assert!(!truthy(Some(&json!(0.0))));
        assert!(truthy(Some(&json!([0]))));
    }

    #[test]
    fn repr() {
        assert_eq!(repr_str("a'b"), "\"a'b\"");
        assert_eq!(repr_str("a'\"b"), "'a\\'\"b'");
        assert_eq!(repr_str("x\u{1b}y\u{a0}"), "'x\\x1by\\xa0'");
        assert_eq!(repr_str("Ситуация"), "'Ситуация'");
        assert_eq!(
            repr_value(&json!({"ok": true, "code": "x"})),
            "{'code': 'x', 'ok': True}"
        );
    }

    #[test]
    fn realpath_follows_links() {
        let dir = std::env::temp_dir().join(format!("hw-rp-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();
        let d = realpath(dir.to_str().unwrap());
        assert_eq!(
            realpath(&format!("{}/link/x.sock", dir.display())),
            format!("{d}/real/x.sock")
        );
        assert_eq!(realpath("/a/b/../c"), "/a/c");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truthy_strings_and_numbers() {
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!("x"))));
        assert!(!truthy(Some(&json!(0))));
        assert!(truthy(Some(&json!(2))));
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!({}))));
        assert!(truthy(Some(&json!({"a": 1}))));
        assert!(!truthy(Some(&json!([]))));
        assert!(!truthy(None));
    }

    #[test]
    fn repr_escapes_like_python() {
        // Non-printables below 0x100, below 0x10000 and above.
        assert_eq!(repr_str("\u{ad}"), "'\\xad'");
        assert_eq!(repr_str("a\u{200b}b"), "'a\\u200bb'");
        assert_eq!(repr_str("\u{e0001}"), "'\\U000e0001'");
        assert_eq!(repr_str("\u{7f}\t\\"), "'\\x7f\\t\\\\'");
        assert_eq!(repr_str("\u{10000}"), "'\u{10000}'");
        assert_eq!(repr_str(""), "''");
    }

    #[test]
    fn float_repr_like_python() {
        let r = |f: f64| repr_value(&json!(f));
        assert_eq!(r(1.0), "1.0");
        assert_eq!(r(1.5), "1.5");
        assert_eq!(r(-3.0), "-3.0");
        assert_eq!(r(1e15), "1000000000000000.0");
        assert_eq!(r(123456789012345.0), "123456789012345.0");
        assert_eq!(r(1e16), "1e+16");
        assert_eq!(r(1e20), "1e+20");
        assert_eq!(r(-2.5e22), "-2.5e+22");
        assert_eq!(r(1e-4), "0.0001");
        assert_eq!(r(1.5e-5), "1.5e-05");
        assert_eq!(r(0.0), "0.0");
        assert_eq!(repr_value(&json!(7)), "7");
        assert_eq!(
            repr_value(&json!([1, "a", null, false])),
            "[1, 'a', None, False]"
        );
        assert_eq!(str_value(Some(&json!("x"))), "x");
        assert_eq!(str_value(None), "None");
        assert_eq!(str_value(Some(&json!([true]))), "[True]");
    }

    #[test]
    fn normpath_like_python() {
        for (i, o) in [
            ("", "."),
            (".", "."),
            ("./", "."),
            ("//a", "//a"),
            ("///a", "/a"),
            ("//a//b/", "//a/b"),
            ("a/../..", ".."),
            ("../a/./b/..", "../a"),
            ("/..", "/"),
            ("/a/b/../../..", "/"),
            ("a/b/", "a/b"),
            ("//", "//"),
        ] {
            assert_eq!(normpath(i), o, "{i:?}");
        }
    }

    #[test]
    fn realpath_relative_dotdot_and_loops() {
        let cwd = std::env::current_dir().unwrap();
        let cwd = realpath(cwd.to_str().unwrap());
        assert_eq!(realpath("x/y"), format!("{cwd}/x/y"));
        assert_eq!(realpath(""), cwd);
        assert_eq!(realpath("/a//b/./c/"), "/a/b/c");
        assert_eq!(realpath("/nonexistent-a/.."), "/");
        assert_eq!(realpath("/nonexistent-a/./.."), "/");
        assert_eq!(realpath("/.."), "/");
        assert_eq!(
            realpath("../../../x"),
            normpath(&format!("{cwd}/../../../x"))
        );
        let dir = std::env::temp_dir().join(format!("hw-rp2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d/e")).unwrap();
        let d = realpath(dir.to_str().unwrap());
        // A relative link, a link through "..", and a loop.
        std::os::unix::fs::symlink("d/e", dir.join("rel")).unwrap();
        std::os::unix::fs::symlink("../d", dir.join("d/e/up")).unwrap();
        std::os::unix::fs::symlink("loop2", dir.join("loop1")).unwrap();
        std::os::unix::fs::symlink("loop1", dir.join("loop2")).unwrap();
        assert_eq!(realpath(&format!("{d}/rel/s")), format!("{d}/d/e/s"));
        assert_eq!(realpath(&format!("{d}/rel/up/e")), format!("{d}/d/d/e"));
        assert_eq!(realpath(&format!("{d}/rel/../x")), format!("{d}/d/x"));
        assert_eq!(realpath(&format!("{d}/loop1/z")), format!("{d}/loop1/z"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn home_and_exists() {
        assert_eq!(home_dir(Some("/h")), "/h");
        let pw = home_dir(None);
        assert!(pw.starts_with('/'), "{pw}");
        assert_eq!(home_dir(Some("")), pw);
        assert!(exists("/"));
        assert!(!exists("/nonexistent/x"));
    }

    #[test]
    fn os_error_text() {
        let e = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(os_error(&e, None), "[Errno 2] No such file or directory");
        assert_eq!(
            os_error(&e, Some("/x")),
            "[Errno 2] No such file or directory: '/x'"
        );
        let other = std::io::Error::other("boom");
        assert_eq!(os_error(&other, None), "boom");
    }
}
