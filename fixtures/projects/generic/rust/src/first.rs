pub fn primary_choice(left: i32, right: i32) -> i32 {
    if left > 0 && right != 0 {
        left + right
    } else {
        0
    }
}
