//! Config file formats: the single extension point for parsing/serializing
//! configuration documents.
//!
//! Every config file (global `config.*`, per-app `apps.d/<id>.*`) is parsed
//! into one canonical in-memory representation — [`serde_json::Value`] — and
//! everything downstream of this module is format-agnostic. Adding a future
//! format means: one enum variant here, one arm in each of
//! [`ConfigFormat::from_extension`], [`ConfigFormat::parse`] and
//! [`ConfigFormat::serialize`], plus the Cargo dependency. Nothing else in
//! the crate changes.

use anyhow::{Context, bail};
use std::path::Path;

/// File formats recognized for updatium configuration files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
    Json,
}

impl ConfigFormat {
    /// Map a filename extension to a format; `None` for anything unknown.
    pub fn from_extension(ext: &str) -> Option<ConfigFormat> {
        match ext {
            "toml" => Some(ConfigFormat::Toml),
            "yaml" | "yml" => Some(ConfigFormat::Yaml),
            "json" => Some(ConfigFormat::Json),
            _ => None,
        }
    }

    /// Like [`Self::from_extension`], but errors listing what IS supported.
    pub fn from_path(path: &Path) -> anyhow::Result<ConfigFormat> {
        let ext = path.extension().and_then(|e| e.to_str()).context(format!(
            "{}: no file extension (expected {})",
            path.display(),
            supported_extensions()
        ))?;
        Self::from_extension(ext).with_context(|| {
            format!(
                "{}: unsupported config extension '.{ext}' (expected {})",
                path.display(),
                supported_extensions()
            )
        })
    }

    /// Canonical output extension ("toml" | "yaml" | "json").
    pub fn extension(self) -> &'static str {
        match self {
            ConfigFormat::Toml => "toml",
            ConfigFormat::Yaml => "yaml",
            ConfigFormat::Json => "json",
        }
    }

    /// Parse text into the canonical representation shared by all consumers.
    pub fn parse(self, text: &str) -> anyhow::Result<serde_json::Value> {
        match self {
            ConfigFormat::Toml => toml::from_str(text).map_err(anyhow::Error::msg),
            ConfigFormat::Yaml => serde_yaml_ng::from_str(text).map_err(anyhow::Error::msg),
            ConfigFormat::Json => serde_json::from_str(text).map_err(anyhow::Error::msg),
        }
    }

    /// Serialize the canonical representation back to text.
    pub fn serialize(self, value: &serde_json::Value) -> anyhow::Result<String> {
        match self {
            ConfigFormat::Json => Ok(serde_json::to_string_pretty(value)?),
            ConfigFormat::Yaml => Ok(serde_yaml_ng::to_string(value)?),
            // Values-before-tables ordering is enforced per level by the
            // converter; plain `toml::to_string_pretty` would reject an
            // interleaved map.
            ConfigFormat::Toml => {
                let ordered = json_to_toml_ordered(value)?;
                Ok(toml::to_string_pretty(&ordered)?)
            }
        }
    }
}

fn supported_extensions() -> &'static str {
    ".toml, .yaml/.yml, .json"
}

/// Convert the IR into a `toml::Value`, emitting non-table entries before
/// table entries at every nesting level.
fn json_to_toml_ordered(v: &serde_json::Value) -> anyhow::Result<toml::Value> {
    Ok(match v {
        serde_json::Value::Object(map) => {
            let mut scalars = Vec::with_capacity(map.len());
            let mut tables = Vec::new();
            for (k, val) in map {
                if val.is_object() {
                    tables.push((k, val));
                } else {
                    scalars.push((k, val));
                }
            }
            let mut out = toml::map::Map::with_capacity(map.len());
            for (k, val) in scalars.into_iter().chain(tables) {
                out.insert(k.clone(), json_to_toml_ordered(val)?);
            }
            toml::Value::Table(out)
        }
        serde_json::Value::Array(a) => toml::Value::Array(
            a.iter()
                .map(json_to_toml_ordered)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                toml::Value::Integer(i64::try_from(u).context("integer out of range for TOML")?)
            } else {
                toml::Value::Float(n.as_f64().expect("JSON number is i64, u64 or f64"))
            }
        }
        serde_json::Value::Null => bail!("null is not representable in TOML"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_BUILD_TOML: &str = r#"
id = "llamacpp"
provider = "source-build"
repo_url = "https://github.com/ggml-org/llama.cpp"
ref_regex = '^refs/tags/b\d+$'
timeout_secs = 3600

[build]
steps = [["cmake", "-B", "build"], ["cmake", "--build", "build", "-j"]]
tracked_files = ["CMakeLists.txt"]
artifacts = ["build/bin/*"]
env = { CMAKE_BUILD_TYPE = "Release" }

[install]
type = "dir"
link_bins = ["llama-cli"]
"#;

    #[test]
    fn roundtrip_all_formats_equivalent() {
        let ir = ConfigFormat::Toml.parse(SRC_BUILD_TOML).unwrap();
        for fmt in [ConfigFormat::Toml, ConfigFormat::Yaml, ConfigFormat::Json] {
            let text = fmt.serialize(&ir).unwrap();
            let back = fmt.parse(&text).unwrap();
            assert_eq!(back, ir, "roundtrip through {fmt:?}");
        }
    }

    #[test]
    fn toml_serialize_emits_values_before_tables() {
        let ir = ConfigFormat::Toml.parse(SRC_BUILD_TOML).unwrap();
        let text = ConfigFormat::Toml.serialize(&ir).unwrap();
        // Alphabetical IR ordering would place the [build] table header
        // before scalar keys like "provider"; the converter must not.
        let scalar = text.find("provider =").expect("top-level scalar present");
        let table = text.find("[build]").expect("table header present");
        assert!(scalar < table, "values emitted before tables:\n{text}");
    }

    #[test]
    fn toml_serialize_rejects_null() {
        let err = ConfigFormat::Toml
            .serialize(&serde_json::json!({ "a": null }))
            .unwrap_err();
        assert!(err.to_string().contains("null"), "{err}");
    }

    #[test]
    fn from_extension_cases() {
        assert_eq!(ConfigFormat::from_extension("toml"), Some(ConfigFormat::Toml));
        assert_eq!(ConfigFormat::from_extension("yaml"), Some(ConfigFormat::Yaml));
        assert_eq!(ConfigFormat::from_extension("yml"), Some(ConfigFormat::Yaml));
        assert_eq!(ConfigFormat::from_extension("json"), Some(ConfigFormat::Json));
        assert_eq!(ConfigFormat::from_extension("ini"), None);
    }
}
