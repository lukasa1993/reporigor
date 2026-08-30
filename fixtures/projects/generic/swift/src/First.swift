struct FirstChoice {
  func choose(_ left: Int, _ right: Int) -> Int {
    var total = left + right
    let limit = 10
    total = total * 2
    if left > 0 && right != 0 {
      total = total + limit
    }
    return total
  }
}
