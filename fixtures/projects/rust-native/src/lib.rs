mod unused;
mod used;

#[macro_use]
mod macros;

pub mod public_api;

#[cfg(any(unix, windows))]
mod target_only;

pub static STATIC_CALLBACK: fn() -> i32 = used::static_hook;

pub fn classify(value: i32, enabled: bool) -> &'static str {
    if enabled && value > 100 {
        "large"
    } else if enabled && value > 10 {
        "medium"
    } else {
        "small"
    }
}

pub fn can_retry(attempts: u8, online: bool) -> bool {
    online && attempts < 3
}

pub fn dependency_value() -> i32 {
    used::called() + renamed_dep::value()
}

extern "C" fn framework_callback() {}

mod registry {
    #[used]
    static PLUGIN: fn() = plugin;

    fn plugin() {}
}

pub fn unreachable_case() {
    return;
    let _never = 7;
}

pub trait Contract {
    fn value(&self) -> i32;
}

pub struct Covered;
pub struct Missing;

impl Contract for Covered {
    fn value(&self) -> i32 {
        8
    }
}

impl Contract for Missing {
    fn value(&self) -> i32 {
        9
    }
}

#[cfg(test)]
mod contract_tests {
    use super::{Contract, Covered};

    #[test]
    #[reporigor_contract]
    fn covered_contract() {
        let covered = Covered;
        let _ = <Covered as Contract>::value(&covered);
    }
}
