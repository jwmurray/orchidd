import Charts
import SwiftUI

enum GraphRange: Int, CaseIterable, Identifiable {
    case day = 24, twoDays = 48, week = 168  // hours
    var id: Int { rawValue }
    var days: Double { Double(rawValue) / 24 }
    var label: String {
        switch self {
        case .day: return "24 h"
        case .twoDays: return "48 h"
        case .week: return "7 days"
        }
    }
}

struct GraphView: View {
    let client: OrchidClient

    @AppStorage("graphRange") private var range: GraphRange = .day
    @State private var points: [Series.Point] = []
    @State private var heater = HeaterIntervals(intervals: [])
    @State private var status: Status?
    @State private var window = (from: Date(), to: Date())
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    Picker("Range", selection: $range) {
                        ForEach(GraphRange.allCases) { Text($0.label).tag($0) }
                    }
                    .pickerStyle(.segmented)

                    if let error { ErrorBanner(message: error) }
                    if let st = status { header(st) }

                    chartTitle("Temperature", Fmt.temp(status?.tempF), .temp)
                    temperatureChart.frame(height: 260)

                    chartTitle("Humidity", Fmt.hum(status?.humidity), .hum)
                    humidityChart.frame(height: 150)

                    chartTitle("Heater plug", status.map(Fmt.heater) ?? "—", .heat)
                    heaterChart.frame(height: 70)
                    Text(heaterSummary).font(.footnote).foregroundStyle(.secondary)
                }
                .padding()
            }
            .navigationTitle("Orchid room")
            .refreshable { await load() }
            .polling(every: 60) { await load() }
            .onChange(of: range) { Task { await load() } }
        }
    }

    private func header(_ st: Status) -> some View {
        HStack {
            Text("\(st.period == "day" ? "Day" : "Night") · set \(Fmt.temp(st.setpointF))")
            Spacer()
            Text(Fmt.age(st.readingAgeS)).foregroundStyle(.secondary)
        }
        .font(.subheadline)
    }

    private func chartTitle(_ title: String, _ value: String, _ color: Color) -> some View {
        HStack {
            Text(title).font(.headline)
            Spacer()
            Text(value).font(.headline).monospacedDigit().foregroundStyle(color)
        }
    }

    private var heaterSummary: String {
        let on = heater.onSeconds(from: window.from, to: window.to)
        let period = "the last \(range.label)"
        return on > 0 ? "On \(Duration_.text(on)) during \(period)." : "Off for \(period)."
    }

    private var onPeriods: [ClosedRange<Date>] { heater.clipped(from: window.from, to: window.to) }

    private var temperatureChart: some View {
        Chart {
            ForEach(Array(onPeriods.enumerated()), id: \.offset) { _, r in
                RectangleMark(xStart: .value("On", r.lowerBound), xEnd: .value("Off", r.upperBound))
                    .foregroundStyle(Color.heat.opacity(0.15))
            }
            ForEach(points) { p in
                if let v = p.tempF {
                    LineMark(x: .value("Time", p.date), y: .value("°F", v))
                        .foregroundStyle(Color.temp)
                        .interpolationMethod(.monotone)
                }
            }
        }
        .chartXScale(domain: window.from...window.to)
        .chartYScale(domain: .automatic(includesZero: false))
        .chartXAxis { timeAxis }
    }

    private var humidityChart: some View {
        Chart(points) { p in
            if let v = p.hum {
                LineMark(x: .value("Time", p.date), y: .value("%RH", v))
                    .foregroundStyle(Color.hum)
                    .interpolationMethod(.monotone)
            }
        }
        .chartXScale(domain: window.from...window.to)
        .chartYScale(domain: .automatic(includesZero: false))
        .chartXAxis { timeAxis }
    }

    private var heaterChart: some View {
        let steps = heater.stepPoints(from: window.from, to: window.to)
        return Chart(Array(steps.enumerated()), id: \.offset) { _, s in
            LineMark(x: .value("Time", s.date), y: .value("Plug", s.on))
                .foregroundStyle(Color.heat)
                .lineStyle(StrokeStyle(lineWidth: 2))
        }
        .chartXScale(domain: window.from...window.to)
        .chartYScale(domain: -0.1...1.1)
        .chartYAxis {
            AxisMarks(values: [0, 1]) { v in
                AxisValueLabel { Text(v.as(Double.self) == 1 ? "On" : "Off") }
            }
        }
        .chartXAxis { timeAxis }
    }

    private var timeAxis: some AxisContent {
        AxisMarks(values: .automatic(desiredCount: 4)) { _ in
            AxisGridLine()
            AxisValueLabel(format: range == .week
                ? .dateTime.month(.defaultDigits).day()
                : .dateTime.hour())
        }
    }

    private func load() async {
        let days = range.days
        do {
            async let s = client.series(days: days)
            async let h = client.heater(days: days)
            async let st = client.status()
            let (series, intervals, current) = try await (s, h, st)
            let now = Date()
            window = (now.addingTimeInterval(-days * 86400), now)
            points = series.points
            heater = intervals
            status = current
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}
