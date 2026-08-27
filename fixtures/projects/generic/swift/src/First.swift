struct FirstChoice {
  func choose(_ left: Int, _ right: Int) -> Int {
    if left > 0 && right != 0 {
      return left + right
    }
    return 0
  }
}
