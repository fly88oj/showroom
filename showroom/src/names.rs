//! 名字/tag 校验（SPEC §4：project/slot `^[a-z0-9][a-z0-9-]{0,30}$`，保留 v2/_api/_bootstrap/_console）。

/// project/slot：`^[a-z0-9][a-z0-9-]{0,30}$`（总长 1..=31，ASCII，手写避免 regex 依赖）。
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 31
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// docker tag 规则：首字符 [A-Za-z0-9_]，其余 [A-Za-z0-9_.-]，总长 ≤128。
pub fn valid_tag(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

pub const RESERVED: [&str; 4] = ["v2", "_api", "_bootstrap", "_console"];

pub fn check_name(s: &str, what: &str) -> Result<(), String> {
    if valid_name(s) {
        Ok(())
    } else {
        Err(format!("非法 {what} “{s}”：须匹配 ^[a-z0-9][a-z0-9-]{{0,30}}$"))
    }
}

pub fn check_project(s: &str) -> Result<(), String> {
    check_name(s, "project")?;
    if RESERVED.contains(&s) {
        return Err(format!("project “{s}” 是保留名"));
    }
    Ok(())
}

pub fn check_tag(s: &str) -> Result<(), String> {
    if valid_tag(s) {
        Ok(())
    } else {
        Err(format!("非法 tag “{s}”：首字符字母/数字/_，仅 [A-Za-z0-9_.-]，≤128 字符"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        for s in ["a", "demo", "pr42", "0x", "a234567890123456789012345678901"] {
            assert!(valid_name(s), "应合法: {s}");
        }
        for s in ["", "-a", "Aa", "a_b", "a b", "a2345678901234567890123456789012", "中文"] {
            assert!(!valid_name(s), "应非法: {s}");
        }
    }

    #[test]
    fn project_reserved() {
        for s in RESERVED {
            assert!(check_project(s).is_err(), "保留名应拒绝: {s}");
        }
        assert!(check_project("demo").is_ok());
        assert!(check_name("pr42", "slot").is_ok());
        assert!(check_name("v2", "slot").is_ok(), "保留名只约束 project");
    }

    #[test]
    fn tag_rules() {
        for s in ["v1", "1.0.0", "latest", "a_b.c-d", "build_2024", "0"] {
            assert!(valid_tag(s), "应合法: {s}");
        }
        for s in ["", ".x", "-x", "a b", "中"] {
            assert!(!valid_tag(s), "应非法: {s}");
        }
        assert!(valid_tag(&"a".repeat(128)));
        assert!(!valid_tag(&"a".repeat(129)));
    }
}
