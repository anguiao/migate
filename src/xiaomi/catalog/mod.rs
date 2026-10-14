mod loader;

pub use loader::{CatalogEntry, CatalogError, current_home, load_entry, select_local_home};

#[cfg(test)]
mod tests;
