import XCTest
@testable import Orchid

final class OrchidTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        try JSONDecoder().decode(T.self, from: Data(json.utf8))
    }

    func testDecodesStatus() throws {
        let st = try decode(Status.self, """
        {"alert":null,"apower_w":0.0,"controller_reason":"above_off_point","heater_on":false,
         "humidity":60.3,"mode":"auto","off_at_f":61.0,"period":"night","plug_error":null,
         "reading_age_s":203,"reading_ts":1792239841,"relay_age_s":38,"relay_watched":true,
         "setpoint_f":60.0,"temp_f":70.7}
        """)
        XCTAssertEqual(st.tempF, 70.7)
        XCTAssertEqual(st.heaterOn, false)
        XCTAssertEqual(st.period, "night")
        XCTAssertNil(st.alert)
        XCTAssertEqual(Fmt.heater(st), "Off")
    }

    func testDecodesStatusAlertAndHeaterOn() throws {
        let st = try decode(Status.self, """
        {"alert":{"kind":"plug_unreachable","detail":"Plug not answering"},
         "heater_on":true,"apower_w":1496.4,"mode":"auto","period":"day","setpoint_f":75}
        """)
        XCTAssertEqual(st.alert?.detail, "Plug not answering")
        XCTAssertEqual(Fmt.heater(st), "On · 1496 W")
    }

    func testPlugOnDrawingNothingSaysTheHeaterIsNotPluggedIn() throws {
        let st = try decode(Status.self, """
        {"heater_on":true,"apower_w":0.0,"mode":"auto","period":"day","setpoint_f":75}
        """)
        XCTAssertEqual(Fmt.heater(st), "Plug activated — heater not plugged in")
    }

    func testStatsWindowLookup() throws {
        let stats = try decode(Stats.self, """
        {"unit":"F","windows":[
          {"count":240,"days":1.0,"humMax":73.2,"humMin":56.1,"key":"24h","tempMaxF":83.1,"tempMinF":69.3},
          {"count":0,"days":7.0,"humMax":null,"humMin":null,"key":"7d","tempMaxF":null,"tempMinF":null}]}
        """)
        XCTAssertEqual(stats.window("24h")?.tempMaxF, 83.1)
        XCTAssertEqual(stats.window("24h")?.tempMinF, 69.3)
        XCTAssertNil(stats.window("7d")?.tempMaxF)
        XCTAssertNil(stats.window("48h"))
        XCTAssertEqual(Fmt.temp(nil), "—")
    }

    func testSeriesPointsTolerateNullsAndShortArrays() throws {
        let s = try decode(Series.self, """
        {"hum":[60.8,null],"t":[1792238402000,1792238762000,1792239121000],"tempF":[70.88,70.7,70.7],"unit":"F"}
        """)
        let p = s.points
        XCTAssertEqual(p.count, 3)
        XCTAssertEqual(p[0].date, Date(timeIntervalSince1970: 1_792_238_402))
        XCTAssertNil(p[1].hum)
        XCTAssertNil(p[2].hum)
        XCTAssertEqual(p[2].tempF, 70.7)
    }

    func testSeriesDecodesTheBluSensor() throws {
        let s = try decode(Series.self, """
        {"t":[1792238402000],"tempF":[70.9],"hum":[60.8],
         "blu":{"t":[1792238400000,1792238460000],"tempF":[70.3,70.4],"hum":[66.0,null]}}
        """)
        XCTAssertEqual(s.bluPoints.count, 2)
        XCTAssertEqual(s.bluPoints[1].tempF, 70.4)
        XCTAssertNil(s.bluPoints[1].hum)
        XCTAssertEqual(s.latestBlu?.tempF, 70.4)
        XCTAssertEqual(s.latestBlu?.hum, 66.0, "humidity from the newest beacon that has one")
        let none = try decode(Series.self, #"{"t":[],"tempF":[],"hum":[]}"#)
        XCTAssertTrue(none.bluPoints.isEmpty)
        XCTAssertNil(none.latestBlu)
    }

    func testHeaterClipsToWindowAndDropsOutside() {
        let h = HeaterIntervals(intervals: [[0, 2000], [5000, 7000], [9000, 12000], [20000, 21000], [1]])
        let from = Date(milliseconds: 1000), to = Date(milliseconds: 10000)
        let r = h.clipped(from: from, to: to)
        XCTAssertEqual(r, [
            Date(milliseconds: 1000)...Date(milliseconds: 2000),
            Date(milliseconds: 5000)...Date(milliseconds: 7000),
            Date(milliseconds: 9000)...Date(milliseconds: 10000),
        ])
    }

    func testHeaterStepLineStartsAndEndsOff() {
        let h = HeaterIntervals(intervals: [[5000, 7000]])
        let pts = h.stepPoints(from: Date(milliseconds: 0), to: Date(milliseconds: 10000))
        XCTAssertEqual(pts.map(\.on), [0, 0, 1, 1, 0, 0])
        XCTAssertEqual(pts.map { Int($0.date.timeIntervalSince1970 * 1000) }, [0, 5000, 5000, 7000, 7000, 10000])
    }

    func testHeaterStepLineWithNoIntervalsIsFlatOff() {
        let pts = HeaterIntervals(intervals: []).stepPoints(from: Date(milliseconds: 0), to: Date(milliseconds: 10))
        XCTAssertEqual(pts.map(\.on), [0, 0])
    }

    func testOnSecondsAndDurationText() {
        let h = HeaterIntervals(intervals: [[0, 3_600_000], [7_200_000, 7_200_000 + 21 * 60_000]])
        let on = h.onSeconds(from: Date(milliseconds: 1_800_000), to: Date(milliseconds: 10_000_000))
        XCTAssertEqual(on, 30 * 60 + 21 * 60)
        XCTAssertEqual(Duration_.text(on), "51 min")
        XCTAssertEqual(Duration_.text(12 * 3600 + 21 * 60), "12 h 21 min")
        XCTAssertEqual(Duration_.text(2 * 3600), "2 h")
    }

    func testHHMMRoundTrip() {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "America/Denver")!
        let d = HHMM.date(from: "07:05", calendar: cal)
        XCTAssertEqual(HHMM.string(from: d, calendar: cal), "07:05")
        XCTAssertEqual(HHMM.string(from: HHMM.date(from: "21:00", calendar: cal), calendar: cal), "21:00")
    }

    func testSettingsDecodeIgnoresExtraFieldsAndEncodesPatch() throws {
        let s = try decode(ThermostatSettings.self, """
        {"mode":"auto","plug_url":"http://192.168.0.30","timezone":"America/Denver","day_start":"07:00",
         "day_end":"21:00","day_setpoint_f":75.0,"night_setpoint_f":60.0,"hysteresis_f":1.0,"max_temp_f":85.0,
         "stale_minutes":20,"alert_below_f":1.5,"alert_phone":"","alert_email":"","alerts_enabled":true,
         "alert_high_f":85.0,"alert_low_f":55.0}
        """)
        XCTAssertEqual(s.daySetpointF, 75)
        XCTAssertEqual(s.staleMinutes, 20)
        XCTAssertEqual(s.alertHighF, 85)
        XCTAssertEqual(s.alertLowF, 55)
        let patch = try JSONSerialization.jsonObject(with: JSONEncoder().encode(s)) as! [String: Any]
        XCTAssertEqual(Set(patch.keys), [
            "mode", "day_start", "day_end", "day_setpoint_f", "night_setpoint_f",
            "hysteresis_f", "max_temp_f", "stale_minutes", "alert_below_f", "alerts_enabled",
            "alert_high_f", "alert_low_f",
        ])
        XCTAssertNil(patch["plug_url"], "the patch must never touch the plug address")
    }

    func testBadServerAddressIsReported() async {
        do {
            _ = try await OrchidClient(base: "not a url").status()
            XCTFail("expected an error")
        } catch {
            XCTAssertEqual(error.localizedDescription, APIError.badAddress.localizedDescription)
        }
    }

    func testDayNightBandsAndEdgesFollowTheScheduleInItsTimeZone() throws {
        let tz = TimeZone(identifier: "America/Denver")!
        let sched = try decode(Schedule.self, """
        {"day_start":"07:00","day_end":"21:00","timezone":"America/Denver","mode":"auto"}
        """)
        // 2026-10-23 07:56 MDT → 2026-10-24 07:56 MDT.
        let from = Date(timeIntervalSince1970: 1_792_763_760)
        let to = from.addingTimeInterval(86_400)
        let bands = DayNight.bands(from: from, to: to, schedule: sched)
        XCTAssertEqual(bands.map(\.isDay), [true, false, true])
        XCTAssertEqual(bands.first?.start, from)
        XCTAssertEqual(bands.last?.end, to)
        let edges = bands.dropFirst().map(\.start)
        XCTAssertEqual(edges.map { DayNight.label($0, tz: tz) }, ["9:00 PM\n10/23", "7:00 AM\n10/24"])
    }

    func testHeatingBandsComeFromPowerDrawNotThePlug() throws {
        let h = try decode(HeaterIntervals.self, """
        {"intervals":[[1000,9000]],"heating":[[5000,9000]]}
        """)
        let from = Date(milliseconds: 0), to = Date(milliseconds: 10_000)
        XCTAssertEqual(h.heatingClipped(from: from, to: to),
                       [Date(milliseconds: 5000)...Date(milliseconds: 9000)])
        XCTAssertEqual(h.onSeconds(from: from, to: to), 8)
        let old = try decode(HeaterIntervals.self, #"{"intervals":[[1000,9000]]}"#)
        XCTAssertEqual(old.heatingClipped(from: from, to: to), [])
    }
}
