//! Crate docs are not attached to any symbol.

use std::fmt;

/// A server that does things.
///
/// Second paragraph.
#[derive(Debug, Clone)]
pub struct Server {
    port: u16,
}

pub const MAX_CONNECTIONS: usize = 1024;
static COUNTER: u32 = 0;

pub type Handler = Box<dyn Fn(&str) -> String>;

/// Behavior shared by services.
pub trait Service {
    /// Start the service.
    fn start(&self) -> Result<(), String>;
    fn name(&self) -> &str {
        "service"
    }
}

impl Server {
    /// Create a server.
    pub fn new(port: u16) -> Self {
        let local_value = 5; // locals are not symbols
        Server { port }
    }

    fn private_helper(&self) {}

    pub async fn run(&self) -> std::io::Result<()> {
        fn nested_fn() {}
        Ok(())
    }
}

impl Service for Server {
    fn start(&self) -> Result<(), String> {
        Ok(())
    }
}

impl<T: fmt::Debug> fmt::Display for Wrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

pub enum Mode {
    Fast,
    Slow(u32),
}

pub mod inner {
    pub fn deep() {}

    mod hidden {
        pub struct Secret;
    }
}

macro_rules! make_thing {
    () => {};
}

pub fn generic<T>(items: Vec<T>) -> usize
where
    T: Clone,
{
    items.len()
}
