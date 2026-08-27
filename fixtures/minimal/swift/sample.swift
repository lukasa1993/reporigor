struct Choice {
  func choose(_ a: Bool, _ b: Bool) -> Int {
    if a && b { return 1 }
    return 0
  }
}
