// minicast mic capture helper.
//
// ffmpeg's avfoundation input drops ~11% of mic buffers (its reader keeps a
// single slot and overwrites it), so streams get periodic silence gaps.
// AVCaptureSession delivers every buffer; this tool writes them to stdout as
// raw 48 kHz mono float32 little-endian PCM for ffmpeg to read on a pipe.
//
// Raw PCM carries no timestamps, so the output is a gap-free timeline: sample
// 0 is the instant `origin` (host-clock seconds, the same clock ffmpeg's
// avfoundation video uses), samples before it are cut and gaps are filled
// with silence. ffmpeg then places the stream with `-itsoffset <origin>`,
// which keeps audio and video in sync however long either side took to start.
//
// usage: miccap now                       print the host clock (seconds), exit
//        miccap "<device name>" <origin>  capture (exit 2: no such device,
//                                         3: capture error)
import AVFoundation

let sampleRate = 48000.0
/// Timeline slack before we pad or cut: smaller offsets are clock jitter.
let jitterSamples = 240

func hostSeconds() -> Double {
    CMTimeGetSeconds(CMClockGetTime(CMClockGetHostTimeClock()))
}

func fail(_ message: String, code: Int32) -> Never {
    FileHandle.standardError.write(Data("miccap: \(message)\n".utf8))
    exit(code)
}

func writeAll(_ base: UnsafeRawPointer, _ length: Int) {
    var written = 0
    while written < length {
        let n = write(1, base + written, length - written)
        if n <= 0 { exit(0) }  // ffmpeg closed the pipe: stream is over
        written += n
    }
}

final class Sink: NSObject, AVCaptureAudioDataOutputSampleBufferDelegate {
    let origin: Double
    var emitted = 0  // samples written so far; sample `emitted` is due at origin + emitted / rate
    let silence = [Float](repeating: 0, count: 4096)

    init(origin: Double) { self.origin = origin }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        guard let block = CMSampleBufferGetDataBuffer(sampleBuffer) else { return }
        var length = 0
        var pointer: UnsafeMutablePointer<Int8>?
        guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &pointer) == kCMBlockBufferNoErr,
              let base = pointer else { return }
        let count = length / MemoryLayout<Float>.size
        let pts = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sampleBuffer))
        let start = Int(((pts - origin) * sampleRate).rounded())

        var skip = 0
        if start + count <= emitted {
            return  // entirely before the timeline position: stale
        } else if start < emitted - jitterSamples {
            skip = emitted - start  // overlaps what we already wrote (or predates origin)
        } else if start > emitted + jitterSamples {
            var gap = start - emitted  // late buffer or startup delay: fill with silence
            while gap > 0 {
                let n = min(gap, silence.count)
                silence.withUnsafeBytes { writeAll($0.baseAddress!, n * MemoryLayout<Float>.size) }
                gap -= n
            }
            emitted = start
        }
        let offset = skip * MemoryLayout<Float>.size
        writeAll(UnsafeRawPointer(base) + offset, length - offset)
        emitted += count - skip
    }
}

let args = CommandLine.arguments
if args.count == 2, args[1] == "now" {
    print(String(format: "%.6f", hostSeconds()))
    exit(0)
}
guard args.count == 3, let origin = Double(args[2]) else {
    fail("usage: miccap now | miccap \"<device name>\" <origin host seconds>", code: 2)
}
let wanted = args[1]

let devices = AVCaptureDevice.DiscoverySession(
    deviceTypes: [.microphone, .external], mediaType: .audio, position: .unspecified
).devices
guard let device = devices.first(where: { $0.localizedName == wanted })
    ?? devices.first(where: { $0.localizedName.localizedCaseInsensitiveContains(wanted) })
else {
    fail("no audio device named \"\(wanted)\". Available: \(devices.map(\.localizedName).joined(separator: ", "))", code: 2)
}

let session = AVCaptureSession()
let output = AVCaptureAudioDataOutput()
output.audioSettings = [
    AVFormatIDKey: kAudioFormatLinearPCM,
    AVSampleRateKey: sampleRate,
    AVNumberOfChannelsKey: 1,
    AVLinearPCMBitDepthKey: 32,
    AVLinearPCMIsFloatKey: true,
    AVLinearPCMIsBigEndianKey: false,
    AVLinearPCMIsNonInterleaved: false,
]
let sink = Sink(origin: origin)
output.setSampleBufferDelegate(sink, queue: DispatchQueue(label: "miccap.audio"))

do {
    let input = try AVCaptureDeviceInput(device: device)
    guard session.canAddInput(input), session.canAddOutput(output) else {
        fail("cannot attach \"\(device.localizedName)\" to the capture session (mic permission denied?)", code: 3)
    }
    session.addInput(input)
    session.addOutput(output)
} catch {
    fail("cannot open \"\(device.localizedName)\": \(error.localizedDescription)", code: 3)
}

NotificationCenter.default.addObserver(forName: .AVCaptureSessionRuntimeError, object: session, queue: nil) { note in
    fail("capture session error: \(String(describing: note.userInfo?[AVCaptureSessionErrorKey]))", code: 3)
}

session.startRunning()
dispatchMain()
