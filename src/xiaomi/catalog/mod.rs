mod loader;

pub(in crate::xiaomi) use loader::{CatalogError, current_home, load_device, select_local_home};

#[cfg(test)]
mod tests;
