import XCTest

@MainActor
final class CompanionUITests: XCTestCase {
    private func capture(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways; add(attachment)
    }
    func testPairReadAndSendOnFixtureHost() async throws {
        var reset = URLRequest(url: URL(string: "http://127.0.0.1:28777/reset")!)
        reset.httpMethod = "POST"
        _ = try await URLSession.shared.data(for: reset)
        let app = XCUIApplication()
        app.launchArguments = ["-appearance.mode", "dark"]
        app.launch()
        if app.buttons["pair-computer"].waitForExistence(timeout: 3) {
            app.buttons["pair-computer"].tap()
        } else {
            app.buttons["Appearance and computers"].tap()
            app.buttons["Pair a computer"].tap()
        }
        let field = app.secureTextFields["connection-code"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        let profile: [String: String] = ["id":"mobile-fixture", "name":"Studio fixture", "endpoint":"ws://127.0.0.1:28777", "token":String(repeating:"a",count:64), "deviceId":"fixture-mac"]
        let data = try JSONSerialization.data(withJSONObject: profile)
        let code = "noches-connect:" + data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
        field.tap(); field.typeText(code)
        app.buttons["Connect to computer"].tap()
        XCTAssertTrue(app.staticTexts["Connected"].waitForExistence(timeout: 15))
        capture("companion-connected-dark")
        let session = app.staticTexts["Build the mobile companion"]
        XCTAssertTrue(session.waitForExistence(timeout: 5)); session.tap()
        let composer = app.textViews["companion-composer"].exists ? app.textViews["companion-composer"] : app.textFields["companion-composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("Check the mobile companion fixture.")
        app.buttons["Send message"].tap()
        XCTAssertTrue(app.staticTexts["Received on the fixture host. No real agent was started."].waitForExistence(timeout: 8))
        app.buttons["Done"].tap()
        let replies = app.staticTexts.matching(NSPredicate(format: "label == %@", "Received on the fixture host. No real agent was started."))
        XCTAssertTrue(replies.allElementsBoundByIndex.last?.isHittable == true)
        capture("companion-transcript-dark")
        app.navigationBars.buttons.firstMatch.tap()
        app.buttons["New session"].tap()
        XCTAssertTrue(app.buttons["Create session"].waitForExistence(timeout: 5))
        app.buttons["Create session"].tap()
        XCTAssertTrue(app.navigationBars["New mobile session"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.descendants(matching: .any)["companion-composer"].firstMatch.exists)
        capture("companion-new-session")
    }

    func testThemeScreensAndAppearanceSettings() {
        let app = XCUIApplication()
        for (mode, theme) in [("light", "zeron-light"), ("dark", "catppuccin-mocha"), ("dark", "tokyo-night")] {
            app.launchArguments = ["-appearance.mode", mode, "-appearance.\(mode)", theme, "-appearance.surface", "opaque"]
            app.launch()
            XCTAssertTrue(app.buttons["Appearance and computers"].waitForExistence(timeout: 5))
            capture("companion-\(theme)")
            app.buttons["Appearance and computers"].tap()
            app.buttons["Appearance"].tap()
            XCTAssertTrue(app.staticTexts["Light theme"].waitForExistence(timeout: 5))
            XCTAssertTrue(app.staticTexts["Dark theme"].exists)
            capture("appearance-\(theme)")
        }
    }
}
