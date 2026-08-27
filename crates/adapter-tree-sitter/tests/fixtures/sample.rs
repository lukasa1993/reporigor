struct Thing;

impl Thing {
    fn choose(&self, value: i32) -> bool {
        fn nested(item: i32) -> bool {
            if item > 0 { true } else { false }
        }
        let positive = |item: i32| {
            if item > 0 { true } else { false }
        };
        if value > 1 && value != 3 {
            positive(value + 1)
        } else {
            false
        }
    }
}

/// duplicate marker true == false
