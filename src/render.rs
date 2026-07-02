//! Turn lldb's raw type/symbol/value strings into readable Rust.

/// `alloc::vec::Vec<demo::Item, alloc::alloc::Global>` -> `Vec<Item>`.
/// Drops lowercase module path segments (an identifier immediately followed by
/// `::`), keeping type names.
pub fn short_type(t: &str) -> String {
    if t.is_empty() {
        return String::new();
    }
    let b = t.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let at_boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if at_boundary && (b[i].is_ascii_lowercase() || b[i] == b'_') {
            let start = i;
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j + 1 < b.len() && b[j] == b':' && b[j + 1] == b':' {
                i = j + 2; // drop "module::"
                continue;
            }
            out.push_str(&t[start..j]);
            i = j;
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    let out = out.replace(", Global>", ">").replace("Global", "");
    let out = out.trim().to_string();
    if out.chars().count() > 80 {
        out.chars().take(80).collect()
    } else {
        out
    }
}

const MANGLE: &[(&str, &str)] = &[
    ("$LT$", "<"), ("$GT$", ">"), ("$u20$", " "), ("$C$", ","), ("$LP$", "("),
    ("$RP$", ")"), ("$u7b$", "{"), ("$u7d$", "}"), ("$RF$", "&"), ("$BP$", "*"),
    ("$u5b$", "["), ("$u5d$", "]"),
];

/// `demo::total::h71bcff96...` -> `demo::total`; decodes legacy `$LT$` mangling.
pub fn short_fn(name: &str) -> String {
    let mut s = name.to_string();
    // strip a trailing ::h<hex>
    if let Some(idx) = s.rfind("::h") {
        let tail = &s[idx + 3..];
        if tail.len() >= 8 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            s.truncate(idx);
        }
    }
    for (k, v) in MANGLE {
        s = s.replace(k, v);
    }
    s.replace("..", "::")
}

/// A value string that is already a complete leaf (no children worth expanding).
pub fn is_leaf_value(v: &str) -> bool {
    let v = v.trim_start();
    matches!(v.chars().next(), Some('"' | '\'' | '(' | '-'))
        || v.starts_with(|c: char| c.is_ascii_digit())
        || v.starts_with("0x")
        || v == "true"
        || v == "false"
        || v == "None"
}

/// A type that renders as a single leaf value (primitive / string / char).
pub fn is_leaf_type(t: &str) -> bool {
    t.ends_with("String")
        || t.ends_with("str")
        || t.ends_with("char")
        || t.ends_with("bool")
        || is_num_type(t)
}

fn is_num_type(t: &str) -> bool {
    for p in ["u", "i", "f"] {
        if let Some(rest) = t.strip_prefix(p) {
            if rest.starts_with(|c: char| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    t.starts_with("usize") || t.starts_with("isize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_module_paths() {
        assert_eq!(short_type("alloc::vec::Vec<demo::Item, alloc::alloc::Global>"), "Vec<Item>");
        assert_eq!(short_type("&str"), "&str");
        assert_eq!(short_type("core::option::Option<u32>"), "Option<u32>");
    }

    #[test]
    fn demangles_symbols() {
        assert_eq!(short_fn("demo::total::h71bcff9653903dac"), "demo::total");
        assert_eq!(short_fn("main"), "main");
        assert!(short_fn("core::ops::FnMut$LT$Args$GT$").contains('<'));
    }

    #[test]
    fn detects_leaves() {
        assert!(is_leaf_value("\"apple\""));
        assert!(is_leaf_value("3"));
        assert!(is_leaf_type("String"));
        assert!(is_leaf_type("u32"));
        assert!(!is_leaf_type("Item"));
    }
}
