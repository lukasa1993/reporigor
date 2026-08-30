trait Left {
    fn same(&self, input: i32) -> i32;
}

trait Right {
    fn same(&self, input: i32) -> i32;
}

struct Value;

impl Left for Value {
    fn same(&self, input: i32) -> i32 {
        input + 1
    }
}

impl Right for Value {
    fn same(&self, input: i32) -> i32 {
        input + 2
    }
}
