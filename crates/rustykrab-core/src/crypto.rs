/// Constant-time string comparison to prevent timing attacks.
///
/// Compares all bytes up to the length of the longer string
/// so that the length of neither input is leaked through timing.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let len = a_bytes.len().max(b_bytes.len());
    let mut result = (a_bytes.len() != b_bytes.len()) as u8;
    for i in 0..len {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }
}
