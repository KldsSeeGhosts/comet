// Zeron for iOS — a viewport onto the zeron mesh. The phone is a peer
// device: it joins the workspace and session doc rooms and drives remote
// engines through the durable command queue.

import SwiftUI

@main
struct ZeronApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .preferredColorScheme(AppearanceSettings.shared.scheme)
                // Monochrome controls: glass buttons, toolbar icons, and
                // toggles render white like the desktop — accent stays paint
                // for status/markdown, never chrome.
                .tint(Theme.text)
                .background(Theme.bg)
                .onChange(of: scenePhase) { _, phase in
                    if phase == .background {
                        model.flushDocs()
                    } else if phase == .active {
                        // Suspension kills sockets without running any
                        // failure path — without this kick the workspace
                        // room stays dead after foregrounding while chat
                        // views reconnect on open (frozen sidebar/Working
                        // indicators against live transcripts, 2026-08-04).
                        model.foregrounded()
                    }
                }
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Group {
            switch model.phase {
            case .signedOut:
                CompanionView()
            case .reauthenticationRequired:
                SignInView()
            case .pickingOrg(let tokens, let orgs):
                OrgPickerView(tokens: tokens, orgs: orgs)
            case .ready:
                HomeView()
            }
        }
        .task { model.restore(); AppearanceSettings.shared.systemDark = colorScheme == .dark }
        .onChange(of: colorScheme) { _, value in AppearanceSettings.shared.systemDark = value == .dark }
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didReceiveMemoryWarningNotification)) { _ in
            model.handleMemoryWarning()
        }
    }
}
