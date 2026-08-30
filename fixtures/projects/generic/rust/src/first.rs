pub fn primary_choice(left: i32, right: i32) -> i32 {
    let mut total = left + right;
    let limit = 10;
    total = total * 2;
    if left > 0 && right != 0 {
        total = total + limit;
    }
    total
}
