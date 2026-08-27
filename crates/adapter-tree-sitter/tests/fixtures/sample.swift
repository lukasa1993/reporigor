struct Thing {
    func choose(_ value: Int) -> Bool {
        let next = value + 1
        let scaled = next * 2
        if (scaled > 1 && value != 3) || value == 0 {
            return true
        }
        return false
    }
}
