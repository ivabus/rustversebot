use std::{fmt, str::FromStr};

use anyhow::{Result, bail, ensure};
use rig_core::{
    catalog::ModelFacts,
    completion::{Effort, Reasoning},
};
use serde::Deserialize;

use crate::DEFAULT_MODEL;

#[derive(Clone, Debug)]
pub struct ModelSelection {
    pub(crate) slug: String,
    pub(crate) reasoning: Option<Reasoning>,
    level: &'static str,
}

fn implicit_level(slug: &str) -> &'static str {
    if slug == DEFAULT_MODEL {
        "high"
    } else {
        "default"
    }
}

impl FromStr for ModelSelection {
    type Err = anyhow::Error;

    fn from_str(setting: &str) -> Result<Self> {
        let (slug, level) = setting
            .split_once('@')
            .unwrap_or((setting, implicit_level(setting)));
        ensure!(
            !slug.is_empty()
                && slug
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c)),
            "Некорректный идентификатор модели"
        );
        let (level, reasoning) = match level {
            "default" => ("default", None),
            "none" | "off" => ("none", Some(Reasoning::Off)),
            word => {
                let effort = Effort::deserialize(serde::de::value::StrDeserializer::<
                    serde::de::value::Error,
                >::new(word))
                .map_err(|_| anyhow::anyhow!("Неизвестный уровень reasoning"))?;
                (effort.as_str(), Some(Reasoning::Effort(effort)))
            }
        };
        if let Some(reasoning) = reasoning
            && let Some(spec) = ModelFacts::default().for_model("openai", slug)
            && spec.reasoning.refusal(&reasoning).is_some()
        {
            bail!("Модель {slug} не поддерживает уровень {level}");
        }
        Ok(Self {
            slug: slug.into(),
            level,
            reasoning,
        })
    }
}

impl fmt::Display for ModelSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.slug, self.level)
    }
}

impl ModelSelection {
    /// Older reports used a bare model ID and the built-in reasoning policy.
    pub fn cache_key(&self) -> String {
        if self.level == implicit_level(&self.slug) {
            self.slug.clone()
        } else {
            self.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_validate_and_preserve_existing_cache_keys() {
        for input in [DEFAULT_MODEL, &format!("{DEFAULT_MODEL}@high")] {
            let selection: ModelSelection = input.parse().unwrap();
            assert_eq!(selection.cache_key(), DEFAULT_MODEL);
            assert_eq!(selection.to_string(), format!("{DEFAULT_MODEL}@high"));
        }
        for input in ["gpt-6.2-sol@medium", "gpt-6.2-sol@default", "new-model@low"] {
            assert_eq!(input.parse::<ModelSelection>().unwrap().cache_key(), input);
        }
        for input in [
            "@high",
            "x@",
            "x@low@high",
            "a b@high",
            "x@fast",
            "gpt-6.2-sol@none",
        ] {
            assert!(input.parse::<ModelSelection>().is_err(), "{input}");
        }
    }
}
