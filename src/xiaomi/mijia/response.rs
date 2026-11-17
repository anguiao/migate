use serde::{Deserialize, Deserializer};

pub(super) fn valid_description(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

pub(super) fn description<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if !valid_description(&value) {
        return Err(serde::de::Error::custom("invalid description"));
    }
    Ok(value)
}

pub(super) fn null_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
pub(super) struct Members {
    #[serde(default, deserialize_with = "null_list")]
    dids: Vec<String>,
    #[serde(default, deserialize_with = "null_list")]
    roomlist: Vec<Room>,
}

#[derive(Deserialize)]
pub(super) struct Room {
    #[serde(default, deserialize_with = "null_list")]
    pub(super) dids: Vec<String>,
}

impl Members {
    pub(in crate::xiaomi) fn into_dids(self) -> impl Iterator<Item = String> {
        self.dids
            .into_iter()
            .chain(self.roomlist.into_iter().flat_map(|room| room.dids))
    }
}
