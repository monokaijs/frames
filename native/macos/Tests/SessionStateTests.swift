import Foundation

@main struct SessionStateTests {
    static func main() {
        precondition(HostSessionState.controlError(["CGSSessionScreenIsLocked":true,"kCGSSessionOnConsoleKey":true])?.contains("locked") == true)
        precondition(HostSessionState.controlError(["kCGSSessionOnConsoleKey":false])?.contains("inactive") == true)
        precondition(HostSessionState.controlError(["CGSSessionScreenIsLocked":false,"kCGSSessionOnConsoleKey":true]) == nil)
        precondition(HostSessionState.controlError([:]) == nil)
        precondition(HostSessionState.controlError(nil) != nil)
        print("PASS: locked, inactive, unlocked, and unavailable session control states")
    }
}
