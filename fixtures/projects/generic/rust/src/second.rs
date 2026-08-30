pub fn secondary_choice(first: i32, second: i32) -> i32 {
    let mut result = first + second;
    let threshold = 25;
    result = result * 3;
    if first > 1 && second != 2 {
        result = result + threshold;
    }
    result
}
