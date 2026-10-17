import SwiftUI

struct NumbersView: View {
    let client: OrchidClient

    @State private var status: Status?
    @State private var stats: Stats?
    @State private var settings: ThermostatSettings?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            List {
                if let error { ErrorBanner(message: error).listRowInsets(EdgeInsets()) }

                Section {
                    HStack(alignment: .firstTextBaseline) {
                        Text(Fmt.temp(status?.tempF))
                            .font(.system(size: 52, weight: .semibold, design: .rounded))
                            .foregroundStyle(Color.temp)
                        Spacer()
                        Text(Fmt.hum(status?.humidity))
                            .font(.system(size: 30, weight: .medium, design: .rounded))
                            .foregroundStyle(Color.hum)
                    }
                    .padding(.vertical, 4)
                }

                if let st = status {
                    Section("Now") {
                        row("Heater", Fmt.heater(st), tint: st.heaterOn == true ? .heat : nil)
                        row("Period", st.period == "day" ? "Day" : "Night")
                        if let cfg = settings {
                            row("Day setpoint", Fmt.temp(cfg.daySetpointF), bold: st.period == "day")
                            row("Night setpoint", Fmt.temp(cfg.nightSetpointF), bold: st.period != "day")
                        } else {
                            row(st.period == "day" ? "Day setpoint" : "Night setpoint", Fmt.temp(st.setpointF))
                        }
                        row("Last reading", Fmt.age(st.readingAgeS))
                        if let detail = st.alert?.detail {
                            Label(detail, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                        }
                    }
                }

                windowSection("Last 24 hours", stats?.window("24h"))
                windowSection("Last 7 days", stats?.window("7d"))
            }
            .navigationTitle("Numbers")
            .refreshable { await load() }
            .polling(every: 60) { await load() }
        }
    }

    @ViewBuilder
    private func windowSection(_ title: String, _ w: Stats.Window?) -> some View {
        if let w {
            Section(title) {
                row("Temperature high", Fmt.temp(w.tempMaxF), tint: .temp)
                row("Temperature low", Fmt.temp(w.tempMinF), tint: .temp)
                row("Humidity high", Fmt.hum(w.humMax), tint: .hum)
                row("Humidity low", Fmt.hum(w.humMin), tint: .hum)
            }
        }
    }

    private func row(_ label: String, _ value: String, tint: Color? = nil, bold: Bool = false) -> some View {
        LabeledContent(label) {
            Text(value).monospacedDigit().fontWeight(bold ? .semibold : .regular).foregroundStyle(tint ?? .primary)
        }
    }

    private func load() async {
        do {
            async let s = client.status()
            async let x = client.stats()
            (status, stats) = try await (s, x)
            settings = try? await client.settings()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}
