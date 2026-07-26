//! Sessiongator configuration: theme and keybindings.
//!
//! Discovery, layering, schema generation, and `$schema` injection come from
//! gator; this file defines only sessiongator's config shape and how a parsed
//! file merges into the loaded settings.

use crate::keybindings::{default_keymap, target_is_compatible, BindingContext, Keymap};
use gator::config::LayerSource;
use gator::theme::Theme;
use gator::AppResult;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

pub const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/Yarden-zamir/sessiongator/main/config-schema.json";

#[derive(Default, Deserialize, JsonSchema)]
#[schemars(
    title = "Sessiongator Config",
    description = "Configuration file for sessiongator theme and keybindings."
)]
struct ConfigFile {
    #[serde(default, rename = "$schema")]
    #[schemars(
        title = "Schema URL",
        description = "Optional JSON Schema URL for editor autocompletion and validation."
    )]
    _schema_url: Option<String>,
    #[serde(default)]
    #[schemars(
        title = "UI",
        description = "User interface color and display settings."
    )]
    ui: Option<ConfigUi>,
    #[serde(default)]
    #[schemars(
        title = "Keybindings",
        description = "Context-specific mappings from key chords to action identifiers."
    )]
    keybindings: Option<ConfigKeybindings>,
}

impl gator::config::AppConfig for ConfigFile {
    fn has_schema_url(&self) -> bool {
        self._schema_url.is_some()
    }
}

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "UI Settings", description = "User interface color settings.")]
struct ConfigUi {
    #[serde(default)]
    #[schemars(
        title = "Theme",
        description = "Color theme to use: auto, light, or dark. Defaults to auto."
    )]
    theme: Option<Theme>,
}

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    title = "Keybindings",
    description = "Key chord to action mappings for each context."
)]
struct ConfigKeybindings {
    #[serde(default)]
    #[schemars(description = "Bindings that apply in every context.")]
    global: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[schemars(description = "Bindings for the session list and search input.")]
    list: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[schemars(description = "Bindings for the transcript pane.")]
    transcript: Option<BTreeMap<String, String>>,
}

impl ConfigKeybindings {
    fn into_layer(self) -> AppResult<Keymap> {
        let tables = [
            (BindingContext::Global, self.global),
            (BindingContext::List, self.list),
            (BindingContext::Transcript, self.transcript),
        ]
        .into_iter()
        .filter_map(|(context, table)| table.map(|table| (context, table)));
        gator::keymap::layer_from_tables(tables).map_err(Into::into)
    }
}

/// Settings after all config layers are applied.
pub struct LoadedConfig {
    /// Theme from config, if any. Command-line and environment settings take
    /// precedence over this.
    pub theme: Option<Theme>,
    pub keymap: Keymap,
}

#[derive(Default)]
struct LoadState {
    theme: Option<Theme>,
    layer: Keymap,
}

impl LoadState {
    fn apply(
        &mut self,
        config: ConfigFile,
        _base_dir: &Path,
        _home: &Path,
        _source: LayerSource,
    ) -> AppResult<()> {
        if let Some(ui) = config.ui {
            if let Some(theme) = ui.theme {
                self.theme = Some(theme);
            }
        }
        if let Some(keybindings) = config.keybindings {
            let layer = keybindings.into_layer()?;
            self.layer.apply_layer(&layer);
        }
        Ok(())
    }

    fn finish(self) -> AppResult<LoadedConfig> {
        let mut keymap = default_keymap();
        keymap.apply_layer(&self.layer);
        keymap
            .validate_targets(|context, target| {
                target_is_compatible(context, target)
                    .then_some(())
                    .ok_or_else(|| format!("{} is not available in this context", target.as_str()))
            })
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        Ok(LoadedConfig {
            theme: self.theme,
            keymap,
        })
    }
}

pub fn load_config(config_entries: &[String]) -> AppResult<LoadedConfig> {
    let default_contents = default_config_contents();
    loader(&default_contents).load(
        config_entries,
        LoadState::default(),
        |state: &mut LoadState, config, base_dir, home, source| {
            state.apply(config, base_dir, home, source)
        },
        LoadState::finish,
    )
}

fn loader(default_contents: &str) -> gator::config::ConfigLoader<'_> {
    gator::config::ConfigLoader {
        app_name: "sessiongator",
        env_var: "SESSIONGATOR_CONFIG",
        schema_url: CONFIG_SCHEMA_URL,
        default_contents,
    }
}

pub fn config_schema_json() -> AppResult<String> {
    gator::config::schema_json::<ConfigFile>()
}

fn default_config_contents() -> String {
    format!(
        r#""$schema" = "{CONFIG_SCHEMA_URL}"

[ui]
theme = "auto"

# Key chords map to action identifiers. Use "none" to disable a default.
# Contexts: global, list, transcript.
# [keybindings.global]
# "ctrl+s" = "cycle-sort"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{BindingTarget, CoreAction};
    use figment::providers::{Format, Toml};
    use figment::Figment;
    use gator::keymap::KeyChord;
    use std::path::PathBuf;

    fn toml_config(contents: &str) -> ConfigFile {
        Figment::from(Toml::string(contents))
            .extract()
            .expect("valid config")
    }

    fn apply(contents: &str) -> AppResult<LoadedConfig> {
        let mut state = LoadState::default();
        state.apply(
            toml_config(contents),
            &PathBuf::from("/config"),
            &PathBuf::from("/home/example"),
            LayerSource::File,
        )?;
        state.finish()
    }

    #[test]
    fn checked_in_schema_matches_generated_schema() {
        let generated = config_schema_json().expect("generated schema");
        assert_eq!(
            generated.trim(),
            include_str!("../config-schema.json").trim()
        );
    }

    #[test]
    fn theme_is_read_from_config() {
        let loaded = apply("[ui]\ntheme = \"dark\"\n").expect("load");
        assert_eq!(loaded.theme, Some(Theme::Dark));
        let empty = apply("").expect("load");
        assert_eq!(empty.theme, None);
    }

    #[test]
    fn configured_bindings_override_defaults_and_none_disables() {
        let loaded = apply(
            r#"
            [keybindings.global]
            "ctrl+r" = "cycle-sort"
            "ctrl+y" = "none"
            "#,
        )
        .expect("load");

        let target = |context, chord: &str| {
            let chord = KeyChord::parse(chord).unwrap();
            loaded
                .keymap
                .bindings_for_context(context)
                .iter()
                .find(|binding| binding.chord == chord)
                .map(|binding| binding.target.clone())
        };
        assert_eq!(
            target(BindingContext::Global, "ctrl+r"),
            Some(BindingTarget::Core(CoreAction::CycleSort))
        );
        assert_eq!(
            target(BindingContext::Global, "ctrl+y"),
            Some(BindingTarget::Disabled)
        );
        // untouched defaults survive
        assert_eq!(
            target(BindingContext::Global, "ctrl+s"),
            Some(BindingTarget::Core(CoreAction::CycleSort))
        );
    }

    #[test]
    fn invalid_chords_targets_and_contexts_fail_loading() {
        assert!(apply("[keybindings.global]\n\"ctrl+nope\" = \"cancel\"\n").is_err());
        assert!(apply("[keybindings.global]\n\"ctrl+r\" = \"not-an-action\"\n").is_err());
        // move-left is meaningless in the list, which only moves right into
        // the transcript
        assert!(apply("[keybindings.list]\n\"ctrl+r\" = \"move-left\"\n").is_err());
        // unknown context names are rejected by deserialization
        assert!(Figment::from(Toml::string(
            "[keybindings.sidebar]\n\"ctrl+r\" = \"cancel\"\n"
        ))
        .extract::<ConfigFile>()
        .is_err());
    }

    #[test]
    fn config_paths_follow_the_shared_discovery_order() {
        let paths = loader("").config_paths(&PathBuf::from("/home/example"));
        assert!(paths.contains(&PathBuf::from(
            "/home/example/.config/sessiongator/config.toml"
        )));
    }
}
