use super::{
    CompileError, CompiledSpec, mapping::compile_features, scope::supports_device,
    spec::SpecDocument,
};

pub fn compile_spec(model: &str, document: &str) -> Result<CompiledSpec, CompileError> {
    let document = SpecDocument::parse(document)?;
    let features = if supports_device(model, document.device_type()) {
        compile_features(model, document.device_type(), &document.services()?)?
    } else {
        Vec::new()
    };
    Ok(CompiledSpec {
        type_urn: document.type_urn,
        features,
    })
}
