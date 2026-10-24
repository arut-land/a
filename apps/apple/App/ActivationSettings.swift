import ServiceManagement
import SwiftUI

struct ActivationSettings: View {
    @State private var status = SMAppService.agent(plistName: "dev.a.daemon.plist").status
    @State private var busy = false
    @State private var problem: String?

    private var service: SMAppService { .agent(plistName: "dev.a.daemon.plist") }

    var body: some View {
        LabeledContent("Start on connection") {
            HStack {
                Text(description)
                if status == .notRegistered || status == .notFound {
                    Button("Enable") { change(enable: true) }
                } else {
                    Button("Disable") { change(enable: false) }
                }
            }
            .disabled(busy)
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
        busy = true
        problem = nil
        Task {
            let helper = Bundle.main.bundleURL.appending(path: "Contents/Helpers/a")
            do {
                if enable {
                    try await Self.command(helper, "install-activation")
                    try service.register()
                } else {
                    try await service.unregister()
                    try await Self.command(helper, "uninstall-activation")
                }
            } catch {
                problem = error.localizedDescription
                if enable && service.status == .notRegistered {
                    try? await Self.command(helper, "uninstall-activation")
                }
            }
            status = service.status
            busy = false
        }
    }

    nonisolated private static func command(_ helper: URL, _ action: String) async throws {
        try await Task.detached {
            let process = Process()
            process.executableURL = helper
            process.arguments = ["daemon", action, "--app"]
            let errors = Pipe()
            process.standardError = errors
            process.standardOutput = FileHandle.nullDevice
            try process.run()
            let data = errors.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            if process.terminationStatus != 0 {
                throw NSError(domain: "dev.a.activation", code: Int(process.terminationStatus),
                              userInfo: [NSLocalizedDescriptionKey: String(decoding: data, as: UTF8.self)])
            }
        }.value
    }
}
