pub fn classify(value: i32) -> &'static str {
    if value > 10 && value < 100 {
        "medium"
    } else if value >= 100 {
        "large"
    } else {
        "small"
    }
}

pub fn enabled(left: bool, right: bool) -> bool {
    left && right
}
