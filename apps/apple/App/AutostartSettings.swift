import ServiceManagement
import SwiftUI

/// The bundled login agent starts the daemon when the user logs in and it
/// stays up; without it the first client starts the daemon as usual.
struct AutostartSettings: View {
    @Environment(\.scenePhase) private var scenePhase
    @State private var status = Self.service.status
    @State private var problem: String?

    private static let service = SMAppService.agent(plistName: "dev.a.daemon.plist")

    var body: some View {
        LabeledContent("Start at login") {
            HStack {
                Text(description)
                if status == .notRegistered || status == .notFound {
                    Button("Enable") { change(enable: true) }
                } else {
                    Button("Disable") { change(enable: false) }
                }
            }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { status = Self.service.status }
        }
        if status == .requiresApproval {
            Button("Open Login Items Settings") { SMAppService.openSystemSettingsLoginItems() }
        }
        if let problem { Text(problem).font(.caption).foregroundStyle(.red) }
    }

    private var description: String {
        switch status {
        case .enabled: "Enabled"
        case .requiresApproval: "Awaiting approval in Login Items"
        case .notRegistered: "Disabled"
        case .notFound: "Agent unavailable"
        @unknown default: "Unknown status"
        }
    }

    private func change(enable: Bool) {
        problem = nil
        do {
            if enable {
                try Self.service.register()
            } else {
                try Self.service.unregister()
            }
        } catch {
            problem = error.localizedDescription
        }
        status = Self.service.status
    }
}
