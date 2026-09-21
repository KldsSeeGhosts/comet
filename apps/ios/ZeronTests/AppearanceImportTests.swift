import XCTest
@testable import Zeron

/// Import, persistence, and repair behavior of `AppearanceSettings`. The
/// built-in catalog contract itself is covered by `AppearanceSettingsTests`.
final class AppearanceImportTests: XCTestCase {
    // MARK: - Helpers

    private func suite() -> UserDefaults { UserDefaults(suiteName: "theme-import-" + UUID().uuidString)! }

    private func builtIn(_ appearance: String) -> MobileThemeVariant {
        let catalog = AppearanceSettings(defaults: suite()).catalog
        return catalog.families.flatMap(\.variants).first { $0.appearance == appearance }!
    }

    /// A resolved family cloned from a built-in variant, with ids rekeyed so it
    /// is importable.
    private func family(id: String, lightID: String, darkID: String,
                        presets: [String: [String: JSONValue]]? = nil,
                        colors: [String: String]? = nil,
                        accent: [String: JSONValue]? = nil) -> MobileThemeFamily {
        let light = builtIn("light"), dark = builtIn("dark")
        return MobileThemeFamily(id: id, name: "Personal", variants: [
            MobileThemeVariant(id: lightID, name: "Personal Light", appearance: "light",
                               recommendedSurfaceTreatment: light.recommendedSurfaceTreatment,
                               colors: colors ?? light.colors, accent: accent ?? light.accent,
                               syntax: light.syntax, presets: presets),
            MobileThemeVariant(id: darkID, name: "Personal Dark", appearance: "dark",
                               recommendedSurfaceTreatment: dark.recommendedSurfaceTreatment,
                               colors: dark.colors, accent: dark.accent, syntax: dark.syntax,
                               presets: presets),
        ])
    }

    // MARK: - Import

    func testImportInstallsResolvedFamilyAndPersistsIt() throws {
        let defaults = suite()
        let settings = AppearanceSettings(defaults: defaults)
        settings.light = "zeron-light" // unrelated preference must survive

        try settings.importFamily(JSONEncoder().encode(family(id: "personal", lightID: "personal-light", darkID: "personal-dark")))

        XCTAssertEqual(settings.custom.map(\.id), ["personal"])
        XCTAssertTrue(settings.variants.contains { $0.id == "personal-light" })
        let restored = AppearanceSettings(defaults: defaults)
        XCTAssertEqual(restored.custom.map(\.id), ["personal"])
        XCTAssertEqual(restored.light, "zeron-light")
    }

    func testImportedFamilyWithoutPresetsDerivesDesktopAccents() throws {
        let settings = AppearanceSettings(defaults: suite())
        try settings.importFamily(JSONEncoder().encode(family(id: "personal", lightID: "personal-light", darkID: "personal-dark")))
        settings.mode = "light"
        settings.light = "personal-light"
        settings.accent = "blue"

        XCTAssertEqual(settings.current.id, "personal-light")
        // The desktop derives preset roles for a variant appearance; the same
        // derivation backs imported families, keyed off the variant, not the
        // device mode.
        let expected = try XCTUnwrap(ThemeRGBA.roles(preset: "blue", dark: false, background: settings.current.colors["background"]!))
        for key in AppearanceSettings.accentRoleKeys {
            XCTAssertEqual(settings.accents[key]?.stringValue, expected[key]?.stringValue, key)
        }
    }

    func testImportSanitizesMalformedOptionalPresets() throws {
        let settings = AppearanceSettings(defaults: suite())
        let valid = builtIn("light").accent
        var brokenGlyph = valid
        brokenGlyph["glyph"] = .string("#8b7cf6") // a ramp must be three colors
        let presets: [String: [String: JSONValue]] = [
            "cyan": valid,
            "banana": valid, // not an AccentPreset
            "blue": ["primary": .string("#60a5fa")], // incomplete roles
            "pink": brokenGlyph,
        ]
        try settings.importFamily(JSONEncoder().encode(family(id: "personal", lightID: "personal-light", darkID: "personal-dark", presets: presets)))
        settings.mode = "light"
        settings.light = "personal-light"

        let installed = try XCTUnwrap(settings.variants.first { $0.id == "personal-light" })
        XCTAssertEqual(Set(installed.presets!.keys), ["cyan", "pink"])
        XCTAssertEqual(installed.presets!["cyan"]?["primary"]?.stringValue, valid["primary"]?.stringValue)
        XCTAssertEqual(installed.presets!["cyan"]?["glyph"]?.arrayValue?.count, 3)
        XCTAssertNil(installed.presets!["pink"]?["glyph"])

        // Preset roles are always the desktop derivation keyed off the resolved
        // variant, so a family cannot paint roles the desktop would not produce.
        settings.accent = "cyan"
        let derivedCyan = try XCTUnwrap(ThemeRGBA.roles(preset: "cyan", dark: false, background: installed.colors["background"]!))
        XCTAssertEqual(settings.accents["primary"]?.stringValue, derivedCyan["primary"]?.stringValue)
        settings.accent = "pink"
        let derivedPink = try XCTUnwrap(ThemeRGBA.roles(preset: "pink", dark: false, background: installed.colors["background"]!))
        XCTAssertEqual(settings.accents["primary"]?.stringValue, derivedPink["primary"]?.stringValue)
        settings.accent = "banana" // unknown preset: theme default roles
        XCTAssertEqual(settings.accents["primary"]?.stringValue, installed.accent["primary"]?.stringValue)
    }

    func testImportRejectsMalformedOrDuplicateIDs() throws {
        let settings = AppearanceSettings(defaults: suite())
        let fragment = family(id: "personal", lightID: "personal-light", darkID: "personal-dark")

        XCTAssertThrowsError(try settings.importFamily(Data("{}".utf8)))
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(settings.catalog.families[0]))) // built-in family id

        var missingColor = fragment.variants[0].colors
        missingColor.removeValue(forKey: "borderStrong")
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(
            family(id: "personal", lightID: "personal-light", darkID: "personal-dark", colors: missingColor))))

        var missingRole = fragment.variants[0].accent
        missingRole.removeValue(forKey: "wash")
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(
            family(id: "personal", lightID: "personal-light", darkID: "personal-dark", accent: missingRole))))

        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(
            family(id: "personal", lightID: "personal-light", darkID: "zeron-light")))) // built-in variant id

        try settings.importFamily(JSONEncoder().encode(fragment))
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(
            family(id: "other", lightID: "personal-light", darkID: "other-dark")))) // installed variant id

        // Re-importing the same family id replaces it instead of duplicating.
        try settings.importFamily(JSONEncoder().encode(fragment))
        XCTAssertEqual(settings.custom.map(\.id), ["personal"])
        XCTAssertEqual(settings.variants.filter { $0.id == "personal-light" }.count, 1)
    }

    // MARK: - Persistence

    func testPersistedCustomsAreRepairedOnLoad() throws {
        let defaults = suite()
        var broken = family(id: "broken", lightID: "broken-light", darkID: "broken-dark")
        var variant = broken.variants[0]
        var colors = variant.colors
        colors.removeValue(forKey: "background")
        variant = MobileThemeVariant(id: variant.id, name: variant.name, appearance: variant.appearance,
                                     recommendedSurfaceTreatment: variant.recommendedSurfaceTreatment,
                                     colors: colors, accent: variant.accent, syntax: variant.syntax,
                                     presets: variant.presets)
        broken = MobileThemeFamily(id: broken.id, name: broken.name,
                                   variants: [variant, broken.variants[1]])

        let stored = [
            broken, // palette no longer resolves
            family(id: "shadow", lightID: "zeron-light", darkID: "shadow-dark"), // built-in id collision
            family(id: "good", lightID: "good-light", darkID: "good-dark",
                   presets: ["zelda": builtIn("light").accent]), // malformed optional preset
            family(id: "good", lightID: "twin-light", darkID: "twin-dark"), // duplicate family id
        ]
        defaults.set(try JSONEncoder().encode(stored), forKey: "appearance.custom")

        let settings = AppearanceSettings(defaults: defaults)
        XCTAssertEqual(settings.custom.map(\.id), ["good"])
        XCTAssertNil(settings.variants.first { $0.id == "broken-light" })
        XCTAssertEqual(settings.variants.filter { $0.id == "shadow-dark" }.count, 0)
        XCTAssertEqual(settings.variants.map(\.id).count, Set(settings.variants.map(\.id)).count)
        XCTAssertNil(settings.variants.first { $0.id == "good-light" }?.presets)
        // The repaired blob is what the next launch reads.
        let rewritten = try JSONDecoder().decode([MobileThemeFamily].self, from: defaults.data(forKey: "appearance.custom")!)
        XCTAssertEqual(rewritten.map(\.id), ["good"])
    }

    func testUnreadableCustomBlobIsCleared() throws {
        let defaults = suite()
        defaults.set(Data("not json".utf8), forKey: "appearance.custom")
        let settings = AppearanceSettings(defaults: defaults)
        XCTAssertTrue(settings.custom.isEmpty)
        XCTAssertEqual(defaults.data(forKey: "appearance.custom"), try JSONEncoder().encode([MobileThemeFamily]()))
    }

    func testSelectionsThatDoNotResolveAreRepairedAndPersisted() throws {
        let defaults = suite()
        defaults.set("banana", forKey: "appearance.mode")
        defaults.set("does-not-exist", forKey: "appearance.light")
        defaults.set("zeron-light", forKey: "appearance.dark") // wrong appearance
        defaults.set("banana", forKey: "appearance.accent")
        defaults.set("chrome", forKey: "appearance.surface")

        let settings = AppearanceSettings(defaults: defaults)
        XCTAssertEqual(settings.mode, "system")
        XCTAssertEqual(settings.light, "zeron-light")
        XCTAssertEqual(settings.dark, "zeron-dark")
        XCTAssertEqual(settings.accent, "themeDefault")
        XCTAssertEqual(settings.surface, "themeDefault")
        XCTAssertEqual(defaults.string(forKey: "appearance.light"), "zeron-light")
        XCTAssertEqual(defaults.string(forKey: "appearance.surface"), "themeDefault")
        XCTAssertEqual(AppearanceSettings(defaults: defaults).dark, "zeron-dark")
    }

    func testReplacingAFamilyRepairsASelectionIntoIt() throws {
        let defaults = suite()
        let settings = AppearanceSettings(defaults: defaults)
        try settings.importFamily(JSONEncoder().encode(family(id: "personal", lightID: "personal-light", darkID: "personal-dark")))
        settings.mode = "light"
        settings.light = "personal-light"

        try settings.importFamily(JSONEncoder().encode(family(id: "personal", lightID: "personal2-light", darkID: "personal2-dark")))
        XCTAssertEqual(settings.light, "zeron-light")
        XCTAssertEqual(defaults.string(forKey: "appearance.light"), "zeron-light")
        XCTAssertNil(settings.variants.first { $0.id == "personal-light" })
    }
}
