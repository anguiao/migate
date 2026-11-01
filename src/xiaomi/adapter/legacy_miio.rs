use crate::xiaomi::capability::PropertyBinding;
use serde_json::{Value, json};

pub(super) struct Mapping {
    fields: &'static [Field],
}

struct Field {
    binding: PropertyBinding,
    name: &'static str,
    setter: &'static str,
    conversion: Conversion,
}

enum Conversion {
    OnOff,
    Enum(&'static [&'static str]),
    Temperature,
}

static MCN02: Mapping = Mapping {
    fields: &[
        Field {
            binding: PropertyBinding { siid: 2, piid: 1 },
            name: "power",
            setter: "set_power",
            conversion: Conversion::OnOff,
        },
        Field {
            binding: PropertyBinding { siid: 2, piid: 2 },
            name: "mode",
            setter: "set_mode",
            conversion: Conversion::Enum(&["auto", "cool", "dry", "heat", "wind"]),
        },
        Field {
            binding: PropertyBinding { siid: 2, piid: 3 },
            name: "tar_temp",
            setter: "set_tar_temp",
            conversion: Conversion::Temperature,
        },
        Field {
            binding: PropertyBinding { siid: 3, piid: 1 },
            name: "fan_level",
            setter: "set_fan_level",
            conversion: Conversion::Enum(&["auto_fan", "small_fan", "medium_fan", "large_fan"]),
        },
        Field {
            binding: PropertyBinding { siid: 3, piid: 2 },
            name: "ver_swing",
            setter: "set_ver_swing",
            conversion: Conversion::OnOff,
        },
    ],
};

pub(super) fn mapping(model: &str) -> Option<&'static Mapping> {
    match model {
        "lumi.acpartner.mcn02" => Some(&MCN02),
        _ => None,
    }
}

impl Mapping {
    pub(super) fn fields(&self) -> Vec<&'static str> {
        self.fields.iter().map(|field| field.name).collect()
    }

    pub(super) fn contains(&self, binding: PropertyBinding) -> bool {
        self.fields.iter().any(|field| field.binding == binding)
    }

    pub(super) fn encode(
        &self,
        binding: PropertyBinding,
        value: &Value,
    ) -> Option<(&'static str, Value)> {
        let field = self.fields.iter().find(|field| field.binding == binding)?;
        let native = match field.conversion {
            Conversion::OnOff => json!(if value.as_bool()? { "on" } else { "off" }),
            Conversion::Enum(names) => json!(*names.get(usize::try_from(value.as_i64()?).ok()?)?),
            Conversion::Temperature => json!(temperature(value)?),
        };
        Some((field.setter, native))
    }

    pub(super) fn decode(&self, result: &Value) -> Result<Vec<Result<Value, ()>>, ()> {
        let values = result
            .as_array()
            .filter(|values| values.len() == self.fields.len())
            .ok_or(())?;
        Ok(self
            .fields
            .iter()
            .zip(values)
            .map(|(field, value)| match field.conversion {
                Conversion::OnOff => match value.as_str() {
                    Some("on") => Ok(json!(true)),
                    Some("off") => Ok(json!(false)),
                    _ => Err(()),
                },
                Conversion::Enum(names) => names
                    .iter()
                    .position(|name| Some(*name) == value.as_str())
                    .map(|index| json!(index))
                    .ok_or(()),
                Conversion::Temperature => temperature(value).map(|value| json!(value)).ok_or(()),
            })
            .collect())
    }

    pub(super) fn index(&self, binding: PropertyBinding) -> Option<usize> {
        self.fields
            .iter()
            .position(|field| field.binding == binding)
    }
}

fn temperature(value: &Value) -> Option<i64> {
    let value = value.as_f64()?;
    (value.fract() == 0.0 && (16.0..=30.0).contains(&value)).then_some(value as i64)
}

#[derive(Debug)]
pub(super) enum WriteError {
    Mapping,
    Request(crate::xiaomi::lan::RequestError),
}

pub(super) async fn set_property(
    mapping: &Mapping,
    client: &crate::xiaomi::lan::Client,
    properties: &[(PropertyBinding, Value)],
    guard: impl Fn() -> bool,
    on_sent: impl FnOnce(),
) -> Result<
    crate::xiaomi::property::PropertyResults<(), crate::xiaomi::lan::RequestError>,
    WriteError,
> {
    use crate::xiaomi::{
        lan::{Failure, RequestError, SendStage},
        property::PropertyResults,
    };
    let [(binding, value)] = properties else {
        return Err(WriteError::Mapping);
    };
    let Some((method, value)) = mapping.encode(*binding, value) else {
        return Err(WriteError::Mapping);
    };
    let result = client
        .request_guarded(method, json!([value]), guard, on_sent)
        .await
        .map_err(WriteError::Request)?;
    if result != json!(["ok"]) {
        return Err(WriteError::Request(RequestError {
            stage: SendStage::Sent,
            failure: Failure::InvalidResponse,
        }));
    }
    Ok(PropertyResults {
        items: vec![Ok(())],
        protocol_error: false,
    })
}

pub(super) async fn read(
    mapping: &Mapping,
    client: &crate::xiaomi::lan::Client,
    guard: impl Fn() -> bool,
) -> Result<Vec<Result<Value, ()>>, crate::xiaomi::lan::RequestError> {
    use crate::xiaomi::lan::{Failure, RequestError, SendStage};
    let result =
        Box::pin(client.request_guarded("get_prop", json!(mapping.fields()), guard, || {})).await?;
    mapping.decode(&result).map_err(|_| RequestError {
        stage: SendStage::Sent,
        failure: Failure::InvalidResponse,
    })
}

#[cfg(test)]
mod tests;
