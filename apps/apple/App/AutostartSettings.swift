import ServiceManagement
import SwiftUI

/// The bundled login agent starts the daemon when the user logs in and it
/// stays up; without it the first client starts the daemon as usual.
struct AutostartSettings: View {
    @Environment(\.scenePhase) private var scenePhase
    @State private var status = Self.service.status
    @State private var problem: String?
    @State private var registration: String?
    @State private var changing = false

    private static let service = SMAppService.agent(plistName: "dev.a.daemon.plist")

    var body: some View {
        LabeledContent("Start at login") {
            HStack {
                Text(description)
                if registration == "off" || registration == "unregistered" {
                    Button("Enable") { change(enable: true) }
                } else {
                    Button("Disable") { change(enable: false) }
                }
            }
            .disabled(changing || registration == nil || registration == "unavailable")
        }
        .task { await refresh() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { Task { await refresh() } }
        }
        if status == .requiresApproval {
            Button("Open Login Items Settings") { SMAppService.openSystemSettingsLoginItems() }
        }
        if let problem { Text(problem).font(.caption).foregroundStyle(.red) }
    }

    private var description: String {
        if registration == "unavailable" { return "Unavailable for isolated installations" }
        if registration == "off" { return "Disabled" }
        if status == .requiresApproval { return "Awaiting approval in Login Items" }
        if registration == "on" { return "Enabled" }
        return switch status {
        case .enabled: "Enabled"
        case .requiresApproval: "Awaiting approval in Login Items"
        case .notRegistered: "Disabled"
        case .notFound: "Agent unavailable"
        @unknown default: "Unknown status"
        }
    }

    private func refresh() async {
        status = Self.service.status
        do {
            registration = try await helper(["__autostart"])
        } catch {
            problem = error.localizedDescription
        }
    }

    private func change(enable: Bool) {
        changing = true
        problem = nil
        Task {
            defer { changing = false }
            do {
                // Only initial bundled registration needs SMAppService. The
                // shared owner controls future launches without terminating it.
                if enable && registration == "unregistered" {
                    try Self.service.register()
                    _ = try await helper(["__autostart", "bundle-on"])
                } else {
                    _ = try await helper(["daemon", "autostart", enable ? "on" : "off"])
                }
            } catch {
                problem = error.localizedDescription
            }
            await refresh()
        }
    }

    private func helper(_ arguments: [String]) async throws -> String {
        let executable = Bundle.main.bundleURL.appending(path: "Contents/Helpers/a")
        return try await Task.detached {
            let process = Process()
            process.executableURL = executable
            process.arguments = arguments
            let output = Pipe()
            let errors = Pipe()
            process.standardOutput = output
            process.standardError = errors
            try process.run()
            let data = output.fileHandleForReading.readDataToEndOfFile()
            let error = errors.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                throw NSError(domain: "Autostart", code: Int(process.terminationStatus),
                              userInfo: [NSLocalizedDescriptionKey: String(decoding: error, as: UTF8.self)])
            }
            return String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        }.value
    }
}
