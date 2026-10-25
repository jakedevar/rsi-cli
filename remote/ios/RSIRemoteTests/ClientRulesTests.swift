import Foundation
import XCTest

@testable import RSIRemote

/// Host input, error mapping and display rules (no network).
final class ClientRulesTests: XCTestCase {
    func testHostNormalisationKeepsABareTailnetName() {
        XCTAssertEqual(normalizeHost(" https://Hub.Example.ts.net/ "), "hub.example.ts.net")
        XCTAssertEqual(normalizeHost("hub.example.ts.net:8443"), "hub.example.ts.net:8443")
        XCTAssertEqual(normalizeHost("hub"), "hub")
    }

    func testHostNormalisationRefusesPathsSchemesAndJunk() {
        XCTAssertNil(normalizeHost(""))
        XCTAssertNil(normalizeHost("http://hub.example.ts.net"))
        XCTAssertNil(normalizeHost("hub.example.ts.net/api"))
        XCTAssertNil(normalizeHost("hub example"))
        XCTAssertNil(normalizeHost("hub.example.ts.net:0"))
        XCTAssertNil(normalizeHost(".hub"))
    }

    func testStatusesMapToOperatorFacingErrors() {
        XCTAssertEqual(statusError(401, code: ""), .unauthorized)
        XCTAssertEqual(statusError(403, code: ""), .forbidden)
        XCTAssertEqual(statusError(502, code: ""), .daemonUnavailable)
        XCTAssertEqual(statusError(503, code: ""), .busy)
        XCTAssertEqual(statusError(500, code: "malformed"), .server(status: 500, code: "malformed"))
        XCTAssertEqual(
            RemoteError.forbidden.message,
            "The hub refused this device (403). Check that this phone is allowed in the hub's Remote settings.")
        XCTAssertEqual(RemoteError.busy.headline, "Hub busy")
        XCTAssertEqual(RemoteError.daemonUnavailable.headline, "Hub restarting")
        XCTAssertTrue(RemoteError.daemonUnavailable.message.contains("restarting"))
        XCTAssertEqual(RemoteError.unreachable("timed out").headline, "Can't reach the hub")
    }

    func testTransportFailuresAreUnreachableNotServerErrors() {
        XCTAssertEqual(unreachableReason(URLError(.cannotFindHost)), "host name not found")
        XCTAssertEqual(unreachableReason(URLError(.timedOut)), "timed out")
        XCTAssertEqual(unreachableReason(URLError(.notConnectedToInternet)), "no network")
        XCTAssertNil(unreachableReason(URLError(.badURL)))
        XCTAssertTrue(RemoteError.unreachable("timed out").isUnreachable)
        XCTAssertTrue(RemoteError.unreachable("timed out").message.contains("Tailscale"))
        XCTAssertFalse(RemoteError.busy.isUnreachable)
    }

    func testGatewayErrorBodiesYieldTheirCode() {
        XCTAssertEqual(errorCode(from: Data(#"{"error":"bad_origin"}"#.utf8)), "bad_origin")
        XCTAssertEqual(errorCode(from: Data("not json".utf8)), "")
    }

    func testUnknownLabelsStayVisibleAsUnknown() throws {
        let tagged = try JSONDecoder().decode(
            Tagged.self, from: Data(#"{"state":"unknown","label":"Pilot"}"#.utf8))
        XCTAssertEqual(tagged.display, "Unknown: Pilot")
        XCTAssertNil(tagged.knownValue)
    }

    func testPathComponentsArePercentEncoded() {
        XCTAssertEqual(escape("a/b c"), "a%2Fb%20c")
        XCTAssertEqual(escape("00000000-0000-4000-8000-000000000001"), "00000000-0000-4000-8000-000000000001")
    }

    @MainActor
    func testSavedHostSurvivesAndRejectsJunk() throws {
        let suite = "rsi-remote-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let model = AppModel(defaults: defaults)
        XCTAssertNil(model.host)
        XCTAssertFalse(model.setHost("http://nope"))
        XCTAssertTrue(model.setHost("https://hub.example.ts.net/"))
        XCTAssertEqual(AppModel(defaults: defaults).host, "hub.example.ts.net")
    }
}
