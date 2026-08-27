pub fn classify(value: i32, enabled: bool) -> &'static str {
    if enabled && value > 100 {
        "large"
    } else if enabled && value > 10 {
        "medium"
    } else {
        "small"
    }
}

pub fn can_retry(attempts: u8, online: bool) -> bool {
    online && attempts < 3
}
