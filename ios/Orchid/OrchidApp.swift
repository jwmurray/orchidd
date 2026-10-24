import SwiftUI

@main
struct OrchidApp: App {
    var body: some Scene {
        WindowGroup { RootView() }
    }
}

struct RootView: View {
    @AppStorage("server") private var server = OrchidClient.defaultServer
    @AppStorage("tab") private var tab = "graph"  // reopens on the last tab used

    var body: some View {
        TabView(selection: $tab) {
            GraphView(client: OrchidClient(base: server))
                .tabItem { Label("Graph", systemImage: "chart.xyaxis.line") }
                .tag("graph")
            SettingsView(client: OrchidClient(base: server), server: $server)
                .tabItem { Label("Settings", systemImage: "gearshape") }
                .tag("settings")
            NumbersView(client: OrchidClient(base: server))
                .tabItem { Label("Numbers", systemImage: "number") }
                .tag("numbers")
        }
    }
}

extension Color {
    static let temp = Color(red: 0.84, green: 0.20, blue: 0.16)
    static let hum = Color(red: 0.11, green: 0.42, blue: 0.85)
    static let heat = Color(red: 0.94, green: 0.55, blue: 0.0)
    /// Graph shading, matching the web page's day and night bands.
    static let day = Color(red: 0.99, green: 0.93, blue: 0.80).opacity(0.7)
    static let night = Color(red: 0.91, green: 0.92, blue: 0.99).opacity(0.8)
}

/// Shared formatting so the three tabs read the same.
enum Fmt {
    static func temp(_ v: Double?) -> String { v.map { String(format: "%.1f°F", $0) } ?? "—" }
    static func hum(_ v: Double?) -> String { v.map { String(format: "%.1f%%", $0) } ?? "—" }

    static func age(_ sec: Double?) -> String {
        guard let s = sec else { return "no reading yet" }
        if s < 90 { return "\(Int(s.rounded())) s ago" }
        if s < 5400 { return "\(Int((s / 60).rounded())) min ago" }
        return String(format: "%.1f h ago", s / 3600)
    }

    /// Below this the plug is on but nothing is plugged into it.
    static let heaterDrawMinW = 5.0

    static func heater(_ st: Status) -> String {
        if st.mode == "off" { return "Off (mode off)" }
        guard let on = st.heaterOn else { return "Unknown" }
        guard on else { return "Off" }
        guard let w = st.apowerW else { return "On" }
        if w < heaterDrawMinW { return "Plug activated — heater not plugged in" }
        return "On · \(Int(w.rounded())) W"
    }
}

/// Re-runs `work` now and every `seconds` while the view is on screen.
struct Polling: ViewModifier {
    let seconds: Double
    let work: () async -> Void

    func body(content: Content) -> some View {
        content.task {
            while !Task.isCancelled {
                await work()
                try? await Task.sleep(for: .seconds(seconds))
            }
        }
    }
}

extension View {
    func polling(every seconds: Double, _ work: @escaping () async -> Void) -> some View {
        modifier(Polling(seconds: seconds, work: work))
    }
}

struct ErrorBanner: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle.fill")
            .font(.footnote)
            .foregroundStyle(.white)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.red.opacity(0.85), in: RoundedRectangle(cornerRadius: 10))
    }
}
