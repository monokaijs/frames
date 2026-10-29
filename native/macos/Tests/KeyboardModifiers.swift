import Foundation

@main struct KeyboardModifierTests {
    static func main() {
        func expect(_ events:[(UInt16,Bool)],_ expected:[(UInt16,Bool)]) {
            precondition(events.count == expected.count && zip(events,expected).allSatisfy { $0.0 == $0.1 }, "Unexpected modifier transitions: \(events), expected \(expected)")
        }
        var keys=KeyboardModifiers()
        keys.changed(0xe0,flags:(1 << 18)|0x0001)
        expect(keys.synchronize(physical:false),[(0xe0,true)])
        keys.changed(0xe4,flags:(1 << 18)|0x2001)
        expect(keys.synchronize(physical:false),[(0xe4,true)])
        keys.changed(0xe0,flags:(1 << 18)|0x2000)
        expect(keys.synchronize(physical:false),[(0xe0,false)])
        precondition(keys.held == [0xe4])
        keys.changed(0xe4,flags:0)
        expect(keys.synchronize(physical:false),[(0xe4,false)])

        // Option dead-key/IME text must not hold Alt on the remote desktop.
        keys.changed(0xe2,flags:(1 << 19)|0x0020)
        expect(keys.synchronize(physical:false),[])
        expect(keys.synchronize(physical:false,optionShortcut:true),[(0xe2,true)])
        expect(keys.synchronize(physical:false),[(0xe2,false)])
        expect(keys.synchronize(physical:true),[(0xe2,true)])
        expect(keys.synchronize(physical:true,suspend:true),[(0xe2,false)])
        expect(keys.synchronize(physical:true),[(0xe2,true)])
        keys.reset()
        precondition(keys.held.isEmpty && keys.forwarded.isEmpty)

        // Synthetic flagsChanged events may omit side bits altogether.
        keys.changed(0xe3,flags:1 << 20)
        keys.changed(0xe7,flags:1 << 20)
        expect(keys.synchronize(physical:false),[(0xe3,true),(0xe7,true)])
        keys.changed(0xe3,flags:1 << 20)
        expect(keys.synchronize(physical:false),[(0xe3,false)])
        keys.changed(0xe7,flags:0)
        expect(keys.synchronize(physical:false),[(0xe7,false)])
        print("PASS: independent modifier sides, Option composition, physical shortcuts, composition suspension, reset")
    }
}
