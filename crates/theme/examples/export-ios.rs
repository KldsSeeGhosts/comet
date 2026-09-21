//! Regenerate the iOS catalog from the same resolved variants used by GPUI:
//! cargo run -q -p zeron-theme --example export-ios > apps/ios/Zeron/Theme/DesktopThemes.json
use zeron_theme::{AccentPreset, AccentSelection, ThemeRegistry};
fn main() -> anyhow::Result<()> {
    let families: Vec<_> = ThemeRegistry::builtin().families.iter().map(|family| {
        let variants: Vec<_> = family.variants.iter().map(|variant| {
            let mut value = serde_json::to_value(variant).unwrap();
            let presets: serde_json::Map<_, _> = AccentPreset::ALL.iter().map(|preset| {
                (serde_json::to_value(preset).unwrap().as_str().unwrap().to_owned(),
                 serde_json::to_value(variant.accent_for(AccentSelection::Preset(*preset))).unwrap())
            }).collect();
            value["presets"] = serde_json::Value::Object(presets);
            value
        }).collect();
        serde_json::json!({"id": family.id, "name": family.name, "variants": variants})
    }).collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({"families": families}))?);
    Ok(())
}
