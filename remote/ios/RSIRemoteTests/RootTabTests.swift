import XCTest

@testable import RSIRemote

final class RootTabTests: XCTestCase {
    func testLaunchArgumentPicksTheTabAndAnythingElseIsTheDashboard() {
        XCTAssertEqual(RootTab.parse("waiting"), .waiting)
        XCTAssertEqual(RootTab.parse(" Projects "), .projects)
        XCTAssertEqual(RootTab.parse("settings"), .settings)
        XCTAssertEqual(RootTab.parse(nil), .dashboard)
        XCTAssertEqual(RootTab.parse("nonsense"), .dashboard)
    }
}
