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
        var crop=WindowContent.Crop()
        precondition(crop.bounds(frame:frame,titleButtons:nil)==frame)
        precondition(crop.bounds(frame:frame,titleButtons:[button])==body)
        // Failed AX scans must not alternate full-frame and content-only video.
        for _ in 0..<20 {
            precondition(crop.bounds(frame:frame,titleButtons:nil)==body)
            precondition(crop.bounds(frame:frame,titleButtons:[button])==body)
        }
        let moved=frame.offsetBy(dx:600,dy:220)
        let movedBody=body.offsetBy(dx:600,dy:220)
        precondition(crop.bounds(frame:moved,titleButtons:nil)==movedBody)
        let resized=CGRect(origin:moved.origin,size:CGSize(width:640,height:480))
        let resizedBody=crop.bounds(frame:resized,titleButtons:nil)
        precondition(resizedBody.minY==resized.minY+top && resizedBody.width==640 && resizedBody.height==480-top)
        precondition(WindowContent.pixelRegion(frame:resized,content:resizedBody,scale:2).minY==top*2)
        // Positive evidence of borderless/fullscreen or custom chrome resets it.
        precondition(crop.bounds(frame:frame,titleButtons:[])==frame)
        precondition(crop.bounds(frame:frame,titleButtons:nil)==frame)
        precondition(crop.bounds(frame:frame,titleButtons:[button])==body)
        let customButton=button.offsetBy(dx:0,dy:top*2)
        precondition(crop.bounds(frame:frame,titleButtons:[customButton])==frame)
        precondition(crop.bounds(frame:frame,titleButtons:nil)==frame)
        var otherWindow=WindowContent.Crop()
        precondition(crop.bounds(frame:frame,titleButtons:[button])==body)
        precondition(otherWindow.bounds(frame:frame,titleButtons:nil)==frame)
        let tiny=CGRect(x:0,y:0,width:10,height:1)
        precondition(crop.bounds(frame:tiny,titleButtons:nil)==tiny)
        let accessory=CGRect(x:frame.minX+6,y:frame.minY+6,width:66,height:20)
        precondition(WindowContent.isTitlebarAccessory(frame:accessory,ownerFrames:[frame]))
        precondition(!WindowContent.isTitlebarAccessory(frame:accessory.offsetBy(dx:0,dy:100),ownerFrames:[frame]))
        precondition(!WindowContent.isTitlebarAccessory(frame:frame,ownerFrames:[frame]))
        precondition(WindowContent.matchingWindow(frame:frame,title:"Short…",candidates:[(frame,"The full browser title - Chrome")])==0)
        precondition(WindowContent.matchingWindow(frame:frame,title:"Source",candidates:[(frame,"Cover"),(frame,"Source")])==1)
        precondition(WindowContent.matchingWindow(frame:frame,title:"Unknown",candidates:[(frame,"A"),(frame,"B")])==nil)
        precondition(WindowContent.matchingWindow(frame:frame,title:"Same",candidates:[(frame,"Same"),(frame,"Same")])==nil)
        precondition(WindowContent.matchingWindow(frame:frame,title:"Same",candidates:[(frame.offsetBy(dx:50,dy:0),"Same")])==nil)
        print("PASS: stable crop through AX failures, moves and resizes; confirmed chrome changes; independent windows; Retina coordinates")
    }
}
