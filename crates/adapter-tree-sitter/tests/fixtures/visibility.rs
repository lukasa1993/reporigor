fn visibility_marker() {}

pub(crate) fn crate_visible() {
    visibility_marker();
}

pub fn public_visible() {
    visibility_marker();
}

fn private_visible() {
    visibility_marker();
}
