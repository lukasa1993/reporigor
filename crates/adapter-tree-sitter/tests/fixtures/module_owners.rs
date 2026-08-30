mod left {
    fn same(value: bool) -> i32 {
        if value { 1 } else { 0 }
    }
}

mod right {
    fn same(value: bool) -> i32 {
        if value { 2 } else { 3 }
    }
}

mod raw {
    fn r#match() {}

    fn call() {
        r#match();
    }
}
