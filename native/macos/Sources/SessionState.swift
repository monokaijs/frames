import CoreGraphics

enum HostSessionState {
    static func controlError(_ session:[String:Any]? = CGSessionCopyCurrentDictionary() as? [String:Any])->String? {
        guard let session else{return "No interactive Mac session is available. Sign in on the Mac."}
        // WindowServer exposes these flags in the current session dictionary.
        // Check lock state before consulting AX: AX can remain trusted while locked.
        if session["CGSSessionScreenIsLocked"] as? Bool == true {
            return "Mac is locked. Unlock it on the Mac to use app windows."
        }
        if session["kCGSSessionOnConsoleKey"] as? Bool == false {
            return "Mac user session is inactive. Switch to that user on the Mac."
        }
        return nil
    }
}
