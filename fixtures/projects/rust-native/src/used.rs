pub(crate) fn called() -> i32 {
    static_hook()
}

pub(crate) fn static_hook() -> i32 {
    1
}
