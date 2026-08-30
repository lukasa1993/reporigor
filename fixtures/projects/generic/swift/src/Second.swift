struct SecondChoice {
  func choose(_ first: Int, _ second: Int) -> Int {
    var result = first + second
    let threshold = 25
    result = result * 3
    if first > 1 && second != 2 {
      result = result + threshold
    }
    return result
  }
}
