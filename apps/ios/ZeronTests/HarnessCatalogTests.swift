import XCTest
@testable import Zeron

final class HarnessCatalogTests: XCTestCase {
    func testCodexFallbackStartsWithAstraAndExposesItsTraits() {
        let models = HarnessCatalog.models(for: "codex")
        let astra = models.first

        XCTAssertEqual(astra?.id, "gpt-6-astra")
        XCTAssertEqual(astra?.label, "GPT-6-Astra")
        XCTAssertEqual(astra?.reasoningLevels,
                       ["low", "medium", "high", "xhigh", "max", "ultra"])
        XCTAssertEqual(astra?.options.first?.id, "serviceTier")
        XCTAssertEqual(astra?.options.first?.choices.map(\.id), ["default", "fast"])
    }

    func testDefaultReasoningMatchesDesktopPreference() {
        let astra = HarnessCatalog.defaultModel(for: "codex")
        XCTAssertEqual(HarnessCatalog.defaultReasoning(for: astra), "high")

        let pi = HarnessCatalog.defaultModel(for: "pi")
        XCTAssertEqual(pi.id, "default")
        XCTAssertEqual(HarnessCatalog.defaultReasoning(for: pi), "high")

        let short = ModelInfo(id: "short", label: "Short", description: nil,
                              reasoningLevels: ["low", "medium"])
        XCTAssertEqual(HarnessCatalog.defaultReasoning(for: short), "medium")
    }

    func testNativePiMetadataMatchesRegistry() {
        let pi = HarnessCatalog.staticInfo(for: "pi")
        XCTAssertEqual(pi.id, "pi")
        XCTAssertEqual(pi.label, "Pi")
        XCTAssertEqual(pi.supportsSteering, true)
        XCTAssertEqual(pi.steeringMode, "step-boundary")
        XCTAssertEqual(pi.midTurnSteering, true)

        let models = HarnessCatalog.models(for: "pi")
        XCTAssertEqual(models.count, 1)
        XCTAssertEqual(models.first?.id, "default")
        XCTAssertEqual(models.first?.reasoningLevels,
                       ["off", "minimal", "low", "medium", "high", "xhigh", "max"])
    }

    func testChoiceFallsBackToAdvertisedDefault() {
        let option = HarnessCatalog.defaultModel(for: "codex").options[0]
        XCTAssertEqual(HarnessCatalog.selectedChoice(for: option, selectedId: "fast").label, "Fast")
        XCTAssertEqual(HarnessCatalog.selectedChoice(for: option, selectedId: "stale").id, "default")
    }
}
