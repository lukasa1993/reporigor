
mod raw {
    fn r#match() {}

    fn call() {
        let callback = r#match;
        callback();
    }
}

mod left {
    fn same(value: bool) -> i32 {
        i32::from(value) * 10
    }
}

mod right {
    fn same(value: bool) -> i32 {
        match value {
            true => 20,
            false => 30,
        }
    }
}
