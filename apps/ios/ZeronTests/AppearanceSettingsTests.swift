import XCTest
@testable import Zeron

final class AppearanceSettingsTests: XCTestCase {
    func testCatalogHasDesktopFamiliesAndAppearanceSelections() throws {
        let defaults = UserDefaults(suiteName: "theme-test-" + UUID().uuidString)!
        let settings = AppearanceSettings(defaults: defaults)
        XCTAssertEqual(settings.catalog.families.count, 19)
        settings.mode = "light"
        XCTAssertEqual(settings.current.id, "zeron-light")
        settings.light = "catppuccin-latte"
        settings.mode = "dark"
        XCTAssertEqual(settings.current.id, "zeron-dark")
        settings.mode = "light"
        XCTAssertEqual(settings.current.id, "catppuccin-latte")
        settings.surface = "opaque"
        XCTAssertFalse(settings.frosted)
        settings.surface = "frosted"
        XCTAssertTrue(settings.frosted)
        let restored = AppearanceSettings(defaults: defaults)
        XCTAssertEqual(restored.light, "catppuccin-latte")
        XCTAssertEqual(restored.surface, "frosted")
        settings.mode = "system"; settings.systemDark = true
        XCTAssertEqual(settings.current.appearance, "dark")
        settings.systemDark = false
        XCTAssertEqual(settings.current.appearance, "light")
    }
    func testDerivedCustomAccentsMatchExportedDesktopRoles() throws {
        let settings = AppearanceSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!)
        for variant in settings.variants {
            for (preset, exported) in variant.presets ?? [:] {
                let derived = try XCTUnwrap(ThemeRGBA.roles(preset: preset, dark: variant.appearance == "dark", background: variant.colors["background"]!))
                // Rust omits ff on opaque colors. Compare numeric RGBA values.
                for key in ["primary", "strong", "wash", "on", "selection", "activity", "caret"] {
                    let expected = ThemeRGBA(try XCTUnwrap(exported[key]?.stringValue)).hex
                    XCTAssertEqual(derived[key]?.stringValue, expected, "\(variant.id)/\(preset)/\(key)")
                }
            }
        }
    }
    func testCustomThemeCannotReplaceBuiltInIDs() throws {
        let settings = AppearanceSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!)
        let original = settings.catalog.families[0]
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(original)))
        let custom = MobileThemeFamily(id: "personal", name: "Personal", variants: original.variants)
        XCTAssertThrowsError(try settings.importFamily(JSONEncoder().encode(custom)))
    }
}
