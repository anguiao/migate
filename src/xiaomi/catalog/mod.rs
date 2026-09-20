mod binding;
mod codec;
mod compiler;
mod data;
mod legacy;
mod mapping;
mod scope;
mod spec;

pub use binding::{
    CommandKind, CommandMapping, CompiledSpec, EventArgumentMapping, EventMapping,
    FeatureDescriptor, PropertyClass, PropertyMapping, WireOperation, WireTarget, WireValue,
    XiaomiBinding,
};
pub use codec::CompileError;
pub use compiler::compile_spec;
pub use data::{
    CatalogDevice, CatalogHome, CatalogRoom, DeviceCatalog, assemble_catalog, persist_catalog,
    restore_catalog,
};
pub use legacy::{LegacyMiioOperation, Mcn02LegacyMapping};
pub use scope::supports_spec;

#[cfg(test)]
mod tests;
