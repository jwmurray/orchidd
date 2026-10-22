import Foundation

// Wire types for the orchidd JSON API. Field names follow the server exactly.

struct Status: Decodable, Equatable {
    struct Alert: Decodable, Equatable {
        let kind: String?
        let detail: String?
    }

    let tempF: Double?
    let humidity: Double?
    let setpointF: Double?
    let period: String?
    let mode: String?
    let heaterOn: Bool?
    let apowerW: Double?
    let readingAgeS: Double?
    let alert: Alert?

    enum CodingKeys: String, CodingKey {
        case tempF = "temp_f", humidity, setpointF = "setpoint_f", period, mode
        case heaterOn = "heater_on", apowerW = "apower_w", readingAgeS = "reading_age_s", alert
    }
}

struct Stats: Decodable, Equatable {
    struct Window: Decodable, Equatable {
        let key: String
        let days: Double
        let count: Int
        let tempMinF: Double?
        let tempMaxF: Double?
        let humMin: Double?
        let humMax: Double?
    }

    let windows: [Window]

    func window(_ key: String) -> Window? { windows.first { $0.key == key } }
}

/// One sensor's readings as parallel arrays.
struct Readings: Decodable, Equatable {
    let t: [Int64]
    let tempF: [Double?]
    let hum: [Double?]

    var points: [Series.Point] {
        t.indices.map { i in
            Series.Point(
                date: Date(milliseconds: t[i]),
                tempF: i < tempF.count ? tempF[i] : nil,
                hum: i < hum.count ? hum[i] : nil
            )
        }
    }
}

/// `/api/data`: the Wi-Fi sensor at the top level, the BLU sensor under `blu`.
/// The graph follows the BLU sensor; the Wi-Fi sensor is still logged.
struct Series: Decodable, Equatable {
    let t: [Int64]
    let tempF: [Double?]
    let hum: [Double?]
    let blu: Readings?

    struct Point: Identifiable, Equatable {
        let date: Date
        let tempF: Double?
        let hum: Double?
        var id: Date { date }
    }

    var points: [Point] { Readings(t: t, tempF: tempF, hum: hum).points }

    var bluPoints: [Point] { blu?.points ?? [] }

    /// Newest BLU temperature, with the newest humidity at or before it.
    var latestBlu: (tempF: Double, hum: Double?)? {
        guard let last = bluPoints.last(where: { $0.tempF != nil }), let tf = last.tempF else { return nil }
        let hum = bluPoints.last(where: { $0.date <= last.date && $0.hum != nil })?.hum
        return (tf, hum)
    }
}

struct HeaterIntervals: Decodable, Equatable {
    let intervals: [[Int64]]

    /// On-periods clipped to [from, to], dropping any that fall outside.
    func clipped(from: Date, to: Date) -> [ClosedRange<Date>] {
        intervals.compactMap { pair in
            guard pair.count == 2 else { return nil }
            let start = max(Date(milliseconds: pair[0]), from)
            let end = min(Date(milliseconds: pair[1]), to)
            return end > start ? start...end : nil
        }
    }

    /// Total on-time inside [from, to].
    func onSeconds(from: Date, to: Date) -> TimeInterval {
        clipped(from: from, to: to).reduce(0) { $0 + $1.upperBound.timeIntervalSince($1.lowerBound) }
    }

    /// The plug as a step line: 0 = off, 1 = on, starting and ending off.
    func stepPoints(from: Date, to: Date) -> [StepPoint] {
        var pts = [StepPoint(date: from, on: 0)]
        for r in clipped(from: from, to: to) {
            pts += [
                StepPoint(date: r.lowerBound, on: 0), StepPoint(date: r.lowerBound, on: 1),
                StepPoint(date: r.upperBound, on: 1), StepPoint(date: r.upperBound, on: 0),
            ]
        }
        pts.append(StepPoint(date: to, on: 0))
        return pts
    }
}

struct StepPoint: Equatable {
    let date: Date
    let on: Double
}

/// The thermostat fields the app edits. Other server settings (plug URL, time
/// zone, alert contacts) are left alone because the PUT body is a patch.
struct ThermostatSettings: Codable, Equatable {
    var mode: String
    var dayStart: String
    var dayEnd: String
    var daySetpointF: Double
    var nightSetpointF: Double
    var hysteresisF: Double
    var maxTempF: Double
    var staleMinutes: Int
    var alertBelowF: Double
    var alertsEnabled: Bool

    enum CodingKeys: String, CodingKey {
        case mode, dayStart = "day_start", dayEnd = "day_end"
        case daySetpointF = "day_setpoint_f", nightSetpointF = "night_setpoint_f"
        case hysteresisF = "hysteresis_f", maxTempF = "max_temp_f"
        case staleMinutes = "stale_minutes", alertBelowF = "alert_below_f"
        case alertsEnabled = "alerts_enabled"
    }
}

enum Duration_ {
    /// "12 h 21 min", "45 min".
    static func text(_ seconds: TimeInterval) -> String {
        let min = Int((seconds / 60).rounded())
        if min < 60 { return "\(min) min" }
        let h = min / 60, m = min % 60
        return m == 0 ? "\(h) h" : "\(h) h \(m) min"
    }
}

extension Date {
    init(milliseconds: Int64) { self.init(timeIntervalSince1970: Double(milliseconds) / 1000) }
}

/// "07:00" <-> a Date today at that time, for the DatePicker.
enum HHMM {
    static func date(from text: String, calendar: Calendar = .current, on day: Date = Date()) -> Date {
        let parts = text.split(separator: ":").compactMap { Int($0) }
        let h = parts.first ?? 0, m = parts.count > 1 ? parts[1] : 0
        return calendar.date(bySettingHour: h, minute: m, second: 0, of: day) ?? day
    }

    static func string(from date: Date, calendar: Calendar = .current) -> String {
        let c = calendar.dateComponents([.hour, .minute], from: date)
        return String(format: "%02d:%02d", c.hour ?? 0, c.minute ?? 0)
    }
}

enum APIError: LocalizedError {
    case badAddress
    case server(String)

    var errorDescription: String? {
        switch self {
        case .badAddress: return "The server address is not a valid URL."
        case .server(let msg): return msg
        }
    }
}

struct OrchidClient {
    static let defaultServer = "https://orchid.gardenway.org"

    let base: String
    var session: URLSession = .shared

    private func url(_ path: String) throws -> URL {
        let trimmed = base.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        guard let url = URL(string: trimmed + path), url.scheme != nil else { throw APIError.badAddress }
        return url
    }

    private func get<T: Decodable>(_ path: String) async throws -> T {
        var req = URLRequest(url: try url(path))
        req.cachePolicy = .reloadIgnoringLocalCacheData
        req.timeoutInterval = 15
        let (data, resp) = try await session.data(for: req)
        try check(resp, data)
        return try JSONDecoder().decode(T.self, from: data)
    }

    private func check(_ resp: URLResponse, _ data: Data) throws {
        guard let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) else { return }
        let msg = (try? JSONDecoder().decode([String: String].self, from: data))?["error"]
        throw APIError.server(msg ?? "Server returned HTTP \(http.statusCode).")
    }

    func status() async throws -> Status { try await get("/api/status") }
    func stats() async throws -> Stats { try await get("/api/stats") }
    func series(days: Double) async throws -> Series { try await get("/api/data?days=\(days)") }
    func heater(days: Double) async throws -> HeaterIntervals { try await get("/api/heater?days=\(days)") }
    func settings() async throws -> ThermostatSettings { try await get("/api/settings") }

    func save(_ settings: ThermostatSettings) async throws -> ThermostatSettings {
        var req = URLRequest(url: try url("/api/settings"))
        req.httpMethod = "PUT"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONEncoder().encode(settings)
        req.timeoutInterval = 15
        let (data, resp) = try await session.data(for: req)
        try check(resp, data)
        return try JSONDecoder().decode(ThermostatSettings.self, from: data)
    }
}
