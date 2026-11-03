pub trait Greeter {
    fn greet(&self) -> u32;
}

pub struct Shouter;

impl Greeter for Shouter {
    fn greet(&self) -> u32 {
        7
    }
}

pub fn target() -> u32 {
    42
}

pub fn caller() -> u32 {
    target() + Shouter.greet()
}

pub mod bad;
