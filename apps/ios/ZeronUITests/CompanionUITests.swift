import XCTest

@MainActor
final class CompanionUITests: XCTestCase {
    private func capture(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways; add(attachment)
    }
    private func openSession(_ app: XCUIApplication, id: String = "fixture-chat") {
        let link = app.buttons["open-session-\(id)"]
        for _ in 0..<6 {
            let frame = link.frame
            if link.isHittable && frame.minY > 120 && frame.maxY < app.frame.maxY - 150 { break }
            if frame.minY < 120 { app.swipeDown() } else { app.swipeUp() }
        }
        XCTAssertTrue(link.waitForExistence(timeout: 5))
        link.tap()
        let opened = app.descendants(matching: .any)["companion-composer"].firstMatch.waitForExistence(timeout: 5)
        if !opened { print(app.debugDescription); capture("navigation-failure") }
        XCTAssertTrue(opened)
    }
    func testPairReadAndSendOnFixtureHost() async throws {
        var reset = URLRequest(url: URL(string: "http://127.0.0.1:28777/reset")!)
        reset.httpMethod = "POST"
        _ = try await URLSession.shared.data(for: reset)
        let app = XCUIApplication()
        app.launchArguments = ["-appearance.mode", "dark", "-appearance.dark", "zeron-dark", "-appearance.surface", "frosted"]
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
        XCTAssertTrue(session.waitForExistence(timeout: 5)); openSession(app)
        app.buttons["Browse files"].tap()
        XCTAssertTrue(app.staticTexts["Sources"].waitForExistence(timeout: 5))
        app.staticTexts["Sources"].tap()
        XCTAssertTrue(app.staticTexts["Client.swift"].waitForExistence(timeout: 5))
        app.staticTexts["Client.swift"].tap()
        XCTAssertTrue(app.staticTexts["    let connected = true"].waitForExistence(timeout: 5))
        capture("companion-file-preview")
        app.buttons["Done"].tap()
        app.buttons["Review changes"].tap()
        XCTAssertTrue(app.staticTexts["+let connected = true"].waitForExistence(timeout: 5))
        capture("companion-changes")
        app.buttons["Done"].tap()
        let composer = app.descendants(matching: .any)["companion-composer"].firstMatch
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

    func testSessionManagementAndDraftRetention() async throws {
        var reset = URLRequest(url: URL(string: "http://127.0.0.1:28777/reset")!)
        reset.httpMethod = "POST"
        _ = try await URLSession.shared.data(for: reset)
        let app = XCUIApplication()
        app.launchArguments = ["-appearance.mode", "dark", "-appearance.dark", "zeron-dark", "-appearance.surface", "frosted"]
        app.launch()
        // The pairing test establishes the fixture profile, or pair on a clean device.
        if app.buttons["pair-computer"].waitForExistence(timeout: 2) {
            app.buttons["pair-computer"].tap()
            let profile: [String: String] = ["id":"mobile-fixture", "name":"Studio fixture", "endpoint":"ws://127.0.0.1:28777", "token":String(repeating:"a",count:64), "deviceId":"fixture-mac"]
            let data = try JSONSerialization.data(withJSONObject: profile)
            let code = "noches-connect:" + data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
            let field = app.secureTextFields["connection-code"]
            field.tap(); field.typeText(code)
            app.buttons["Connect to computer"].tap()
        }
        XCTAssertTrue(app.staticTexts["Connected"].waitForExistence(timeout: 15))
        app.buttons["Filter sessions"].tap()
        app.buttons["scope-Needs you"].tap()
        XCTAssertTrue(app.staticTexts["Review the authentication flow"].exists)
        XCTAssertFalse(app.staticTexts["Build the mobile companion"].exists)
        capture("companion-needs-you")
        app.buttons["Filter sessions"].tap()
        app.buttons["scope-All"].tap()
        let search = app.textFields["session-search"]
        search.tap(); search.typeText("companion")
        let session = app.staticTexts["Build the mobile companion"]
        XCTAssertTrue(session.waitForExistence(timeout: 5)); openSession(app)
        let composer = app.descendants(matching: .any)["companion-composer"].firstMatch
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("Keep my unsent draft")
        app.buttons["Done"].tap()
        app.navigationBars.buttons.firstMatch.tap()
        openSession(app)
        XCTAssertEqual(composer.value as? String, "Keep my unsent draft")
        app.buttons["Session actions for Build the mobile companion"].tap()
        app.buttons["Rename session"].tap()
        let title = app.alerts.textFields.firstMatch
        title.tap(withNumberOfTaps: 3, numberOfTouches: 1)
        title.typeText("Pocket control")
        XCTAssertEqual(title.value as? String, "Pocket control")
        app.alerts.buttons["Save"].tap()
        XCTAssertTrue(app.navigationBars["Pocket control"].waitForExistence(timeout: 5))
        app.buttons["Session actions for Pocket control"].tap()
        app.buttons["Archive session"].tap()
        XCTAssertTrue(app.buttons["Filter sessions"].waitForExistence(timeout: 5))
        if app.buttons["Clear search"].exists { app.buttons["Clear search"].tap() }
        app.buttons["Filter sessions"].tap()
        app.buttons["scope-Archived"].tap()
        XCTAssertTrue(app.staticTexts["Pocket control"].waitForExistence(timeout: 5))
        app.buttons["open-session-fixture-chat"].press(forDuration: 1)
        app.buttons["Restore session"].tap()
        XCTAssertTrue(app.staticTexts["Pocket control"].waitForNonExistence(timeout: 5))
        app.buttons["Filter sessions"].tap()
        app.buttons["scope-All"].tap()
        XCTAssertTrue(app.staticTexts["Pocket control"].waitForExistence(timeout: 5))
        capture("companion-restored-session")
    }

    func testApprovalQueueAndStop() async throws {
        var reset = URLRequest(url: URL(string: "http://127.0.0.1:28777/reset")!)
        reset.httpMethod = "POST"
        _ = try await URLSession.shared.data(for: reset)
        let app = XCUIApplication()
        app.launchArguments = ["-appearance.mode", "dark", "-appearance.dark", "zeron-dark"]
        app.launch()
        if app.buttons["pair-computer"].waitForExistence(timeout: 2) {
            app.buttons["pair-computer"].tap()
            let profile: [String: String] = ["id":"mobile-fixture", "name":"Studio fixture", "endpoint":"ws://127.0.0.1:28777", "token":String(repeating:"a",count:64), "deviceId":"fixture-mac"]
            let data = try JSONSerialization.data(withJSONObject: profile)
            let code = "noches-connect:" + data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
            let field = app.secureTextFields["connection-code"]
            field.tap(); field.typeText(code)
            app.buttons["Connect to computer"].tap()
        }
        XCTAssertTrue(app.staticTexts["Connected"].waitForExistence(timeout: 15))
        openSession(app, id: "fixture-approval")
        let allow = app.buttons.containing(.staticText, identifier: "Allow").firstMatch
        XCTAssertTrue(allow.waitForExistence(timeout: 5))
        capture("companion-approval")
        allow.tap()
        XCTAssertTrue(allow.waitForNonExistence(timeout: 5))
        app.navigationBars.buttons.firstMatch.tap()
        openSession(app, id: "fixture-working")
        let composer = app.descendants(matching: .any)["companion-composer"].firstMatch
        composer.tap(); composer.typeText("Follow up after this turn")
        app.buttons["Send message"].tap()
        app.buttons["Done"].tap()
        let queued = app.buttons["Queued · 1"]
        XCTAssertTrue(queued.waitForExistence(timeout: 5)); queued.tap()
        XCTAssertTrue(app.staticTexts["Follow up after this turn"].waitForExistence(timeout: 5))
        capture("companion-queued")
        app.buttons["Stop session"].tap()
        XCTAssertTrue(app.buttons["Stop session"].waitForNonExistence(timeout: 5))
        let (data, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:28777/commands")!)
        let commands = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [[String: Any]])
        let params = commands.compactMap { $0["params"] as? [String: Any] }
        let answer = try XCTUnwrap(params.compactMap { $0["command"] as? [String: Any] }.first { $0["kind"] as? String == "respondInput" })
        XCTAssertEqual(answer["requestId"] as? String, "approval-request")
        XCTAssertEqual((answer["answers"] as? [[String: Any]])?.first?["labels"] as? [String], ["Allow"])
        XCTAssertTrue(params.contains { $0["text"] as? String == "Follow up after this turn" && $0["holdForTurnEnd"] as? Bool == true })
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
