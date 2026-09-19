//! Schema declarations. A domain's records are typed by its schema, and schemas
//! only ever grow: fields are appended, never changed or removed. That is what
//! makes "no migrations" true — old records stay valid under every later schema,
//! and old readers ignore fields they do not know.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Int,
    Float,
    Bool,
    /// Milliseconds since the epoch.
    Time,
    /// Hex in JSON.
    Bytes,
    /// A `craftec://` address of another record.
    Ref,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Schema {
    /// Dictionary type name, e.g. `Task`, `Person`.
    #[serde(rename = "type")]
    pub type_name: String,
    pub fields: Vec<Field>,
}

pub const MAX_FIELDS: usize = 256;
pub const MAX_NAME: usize = 64;

impl Schema {
    pub fn check(&self) -> Result<(), String> {
        if self.type_name.is_empty() || self.type_name.len() > MAX_NAME {
            return Err("schema type name must be 1–64 bytes".into());
        }
        if self.fields.len() > MAX_FIELDS {
            return Err(format!("at most {MAX_FIELDS} fields"));
        }
        for (i, f) in self.fields.iter().enumerate() {
            if f.name.is_empty() || f.name.len() > MAX_NAME {
                return Err(format!("field {i}: name must be 1–64 bytes"));
            }
            if self.fields[..i].iter().any(|g| g.name == f.name) {
                return Err(format!("duplicate field `{}`", f.name));
            }
        }
        Ok(())
    }

    /// May `next` replace `self`? Only by appending optional fields.
    pub fn allows(&self, next: &Schema) -> Result<(), String> {
        if next.type_name != self.type_name {
            return Err(format!(
                "type is `{}`; it cannot become `{}`",
                self.type_name, next.type_name
            ));
        }
        if next.fields.len() < self.fields.len()
            || next.fields[..self.fields.len()] != self.fields[..]
        {
            return Err(
                "existing fields cannot be changed, reordered or removed — only appended to".into(),
            );
        }
        if let Some(f) = next.fields[self.fields.len()..].iter().find(|f| f.required) {
            return Err(format!(
                "new field `{}` cannot be required: existing records do not have it",
                f.name
            ));
        }
        Ok(())
    }
}
