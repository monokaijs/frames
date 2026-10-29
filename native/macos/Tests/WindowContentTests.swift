import AppKit

@main struct WindowContentTests {
    @MainActor static func main() {
        let frame=CGRect(x:-800,y:140,width:440,height:352)
        let native=NSWindow.contentRect(forFrameRect:NSRect(origin:.zero,size:frame.size),styleMask:[.titled,.closable,.miniaturizable,.resizable])
        let top=frame.height-native.height
        precondition(top>0)
        let button=CGRect(x:frame.minX+12,y:frame.minY+(top-14)/2,width:14,height:14)
        let body=WindowContent.bounds(frame:frame,titleButtons:[button])
        precondition(body.minY==frame.minY+top && body.width==native.width && body.height==native.height)
        let pixels=WindowContent.pixelRegion(frame:frame,content:body,scale:2)
        precondition(pixels.minY==top*2 && pixels.width==body.width*2 && pixels.height==body.height*2)
        precondition(WindowContent.bounds(frame:frame,titleButtons:[])==frame)
        precondition(WindowContent.bounds(frame:frame,titleButtons:[CGRect(x:0,y:0,width:14,height:14)])==frame)
        print("PASS: verified standard title-bar removal, Retina coordinates, unverified/custom chrome retained")
    }
}
