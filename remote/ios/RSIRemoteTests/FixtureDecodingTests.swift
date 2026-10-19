import Foundation
import XCTest

@testable import RSIRemote

/// Every positive V1 response fixture of the methods the app reads decodes
/// into the app's models (remote/contracts/v1/fixtures/corpus.json, the same
/// corpus the Rust and TypeScript codecs qualify against).
final class FixtureDecodingTests: XCTestCase {
    private struct Case {
        let id: String
        let method: String
        let result: Data
    }

    private static func responseCases() throws -> [Case] {
        let bundle = Bundle(for: FixtureDecodingTests.self)
        let url = try XCTUnwrap(bundle.url(forResource: "corpus", withExtension: "json"))
        let corpus = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let cases = try XCTUnwrap(corpus["cases"] as? [[String: Any]])
        return try cases.compactMap { entry in
            guard entry["valid"] as? Bool == true,
                let input = entry["input"] as? [String: Any],
                input["type"] as? String == "response",
                let value = input["value"] as? [String: Any],
                let method = value["method"] as? String,
                let result = value["result"]
            else { return nil }
            return Case(
                id: try XCTUnwrap(entry["id"] as? String),
                method: method,
                result: try JSONSerialization.data(withJSONObject: result))
        }
    }

    private func decode(_ fixture: Case) throws {
        let decoder = JSONDecoder()
        switch fixture.method {
        case "RemoteGetInfoV1": _ = try decoder.decode(InfoResult.self, from: fixture.result)
        case "RemoteListProjectsV1":
            _ = try decoder.decode(ProjectListResult.self, from: fixture.result)
        case "RemoteListSessionsV1":
            _ = try decoder.decode(SessionListResult.self, from: fixture.result)
        case "RemoteGetSessionV1":
            _ = try decoder.decode(SessionDetailResult.self, from: fixture.result)
        case "RemoteGetHistoryPageV1":
            _ = try decoder.decode(HistoryPageResult.self, from: fixture.result)
        case "RemoteGetDecisionsV1":
            _ = try decoder.decode(DecisionListResult.self, from: fixture.result)
        default:
            XCTFail("unexpected method \(fixture.method) in \(fixture.id)")
        }
    }

    func testEveryPositiveResponseFixtureDecodes() throws {
        let cases = try Self.responseCases()
        let methods = Set(cases.map(\.method))
        XCTAssertEqual(
            methods,
            [
                "RemoteGetInfoV1", "RemoteListProjectsV1", "RemoteListSessionsV1",
                "RemoteGetSessionV1", "RemoteGetHistoryPageV1", "RemoteGetDecisionsV1",
            ])
        XCTAssertGreaterThanOrEqual(cases.count, 100)
        for fixture in cases {
            XCTAssertNoThrow(try decode(fixture), "fixture \(fixture.id) (\(fixture.method))")
        }
    }

    func testEveryPositiveSafeErrorFixtureDecodes() throws {
        let bundle = Bundle(for: FixtureDecodingTests.self)
        let url = try XCTUnwrap(bundle.url(forResource: "corpus", withExtension: "json"))
        let corpus = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let errors = (corpus["cases"] as? [[String: Any]] ?? []).compactMap { entry -> Data? in
            guard entry["valid"] as? Bool == true,
                let input = entry["input"] as? [String: Any],
                input["type"] as? String == "error",
                let value = input["value"]
            else { return nil }
            return try? JSONSerialization.data(withJSONObject: value)
        }
        XCTAssertFalse(errors.isEmpty)
        for body in errors {
            let decoded = try JSONDecoder().decode(SafeErrorBody.self, from: body)
            XCTAssertFalse(decoded.code.isEmpty)
            XCTAssertEqual(errorCode(from: body), decoded.code)
        }
    }

    func testIdentityFixtureKeepsOwnTitleAndProvider() throws {
        let fixture = try XCTUnwrap(
            Self.responseCases().first { $0.id == "session-own-parent-continuation-providers" })
        let page = try JSONDecoder().decode(SessionListResult.self, from: fixture.result)
        let first = try XCTUnwrap(page.items.first)
        XCTAssertEqual(page.project.name, "Identity project")
        XCTAssertEqual(first.ownTitle, "Own group identity")
        XCTAssertEqual(first.provider, .known("Claude"))
        XCTAssertEqual(first.status.display, "Running")
    }

    func testNativeApprovalFixtureShowsMethodAndDescription() throws {
        let fixture = try XCTUnwrap(
            Self.responseCases().first { $0.id == "decisions-two-native-no-history" })
        let list = try JSONDecoder().decode(DecisionListResult.self, from: fixture.result)
        let decision = try XCTUnwrap(list.items.first)
        XCTAssertEqual(decision.title, "commandExecution/approval")
        XCTAssertEqual(decision.detail, "Run tests")
        XCTAssertTrue(decision.isOpen)
    }
}
