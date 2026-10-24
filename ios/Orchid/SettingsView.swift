import SwiftUI

struct SettingsView: View {
    let client: OrchidClient
    @Binding var server: String

    @State private var loaded: ThermostatSettings?
    @State private var draft: ThermostatSettings?
    @State private var message: (text: String, ok: Bool)?
    @State private var saving = false
    @State private var serverDraft = ""

    var body: some View {
        NavigationStack {
            Form {
                if let message {
                    Label(message.text, systemImage: message.ok ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                        .foregroundStyle(message.ok ? .green : .red)
                }

                if draft != nil {
                    thermostatSections
                } else {
                    Section { ProgressView("Loading settings…") }
                }

                Section {
                    TextField("Server", text: $serverDraft)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    Button("Use this server") {
                        server = serverDraft.trimmingCharacters(in: .whitespaces)
                        Task { await load() }
                    }
                    .disabled(serverDraft == server)
                } header: {
                    Text("Connection")
                } footer: {
                    Text("Default \(OrchidClient.defaultServer). Reachable only with Tailscale on.")
                }
            }
            .navigationTitle("Settings")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button(saving ? "Saving…" : "Save") { Task { await save() } }
                        .disabled(draft == nil || draft == loaded || saving)
                }
                ToolbarItem(placement: .cancellationAction) {
                    Button("Revert") { draft = loaded; message = nil }
                        .disabled(draft == loaded)
                }
            }
            .refreshable { await load() }
            .task { serverDraft = server; await load() }
        }
    }

    @ViewBuilder
    private var thermostatSections: some View {
        Section("Heater") {
            Picker("Mode", selection: bind(\.mode)) {
                Text("Auto").tag("auto")
                Text("Off (keep heater off)").tag("off")
            }
        }
        Section("Schedule") {
            DatePicker("Day starts", selection: time(\.dayStart), displayedComponents: .hourAndMinute)
            DatePicker("Day ends", selection: time(\.dayEnd), displayedComponents: .hourAndMinute)
            degrees("Day minimum", \.daySetpointF, 40...90)
            degrees("Night minimum", \.nightSetpointF, 40...90)
        }
        Section("Control") {
            degrees("Hysteresis", \.hysteresisF, 0.5...5)
            degrees("High cutoff", \.maxTempF, 60...100)
        }
        Section("Alerts") {
            Toggle("Alerts on", isOn: bind(\.alertsEnabled))
            degrees("Alert when below setpoint by", \.alertBelowF, 0.5...10)
            Stepper(value: bind(\.staleMinutes), in: 5...240, step: 5) {
                LabeledContent("Sensor stale after", value: "\(draft?.staleMinutes ?? 0) min")
            }
        }
    }

    private func degrees(_ label: String, _ key: WritableKeyPath<ThermostatSettings, Double>, _ range: ClosedRange<Double>) -> some View {
        Stepper(value: bind(key), in: range, step: 0.5) {
            LabeledContent(label, value: String(format: "%.1f°F", draft?[keyPath: key] ?? 0))
        }
    }

    private func bind<T>(_ key: WritableKeyPath<ThermostatSettings, T>) -> Binding<T> {
        Binding(
            get: { draft![keyPath: key] },
            set: { draft![keyPath: key] = $0; message = nil }
        )
    }

    private func time(_ key: WritableKeyPath<ThermostatSettings, String>) -> Binding<Date> {
        Binding(
            get: { HHMM.date(from: draft![keyPath: key]) },
            set: { draft![keyPath: key] = HHMM.string(from: $0); message = nil }
        )
    }

    private func load() async {
        do {
            let s = try await client.settings()
            loaded = s
            draft = s
            message = nil
        } catch {
            message = (error.localizedDescription, false)
        }
    }

    private func save() async {
        guard let draft else { return }
        saving = true
        defer { saving = false }
        do {
            let saved = try await client.save(draft)
            loaded = saved
            self.draft = saved
            message = ("Saved.", true)
        } catch {
            message = (error.localizedDescription, false)
        }
    }
}
