#[derive(Debug)]
pub struct CookbookTest {
    pub name: &'static str,
    // pub test_fn: fn(String) -> Result<(), ()>,
    pub test_fn: fn(),
}

// Instantiate plugin registry
inventory::collect!(CookbookTest);

pub mod hello;
