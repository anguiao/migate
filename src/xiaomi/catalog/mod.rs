mod loader;

pub use loader::{CatalogError, load_scoped};

#[cfg(test)]
mod tests;
