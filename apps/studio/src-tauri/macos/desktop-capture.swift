import Foundation
import ScreenCaptureKit
import CoreMedia
import AudioToolbox

final class AudioSink: NSObject, SCStreamOutput {
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, sampleBuffer.isValid,
              let format = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(format)?.pointee,
              asbd.mFormatID == kAudioFormatLinearPCM,
              asbd.mBitsPerChannel == 32,
              (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0 else { return }

        let frames = CMSampleBufferGetNumSamples(sampleBuffer)
        guard frames > 0 else { return }
        let channels = Int(asbd.mChannelsPerFrame)
        guard channels == 2 else { return }
        let count = max(1, channels)
        let bytes = MemoryLayout<AudioBufferList>.size + (count - 1) * MemoryLayout<AudioBuffer>.size
        let storage = UnsafeMutableRawPointer.allocate(byteCount: bytes, alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { storage.deallocate() }
        let buffers = storage.bindMemory(to: AudioBufferList.self, capacity: 1)
        var block: CMBlockBuffer?
        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer, bufferListSizeNeededOut: nil, bufferListOut: buffers,
            bufferListSize: bytes, blockBufferAllocator: kCFAllocatorDefault,
            blockBufferMemoryAllocator: kCFAllocatorDefault,
            flags: UInt32(kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment),
            blockBufferOut: &block)
        guard status == noErr else { return }
        let audio = UnsafeMutableAudioBufferListPointer(buffers)
        var output = [Float](repeating: 0, count: frames * 2)
        if audio.count == 1, let data = audio[0].mData {
            let source = data.assumingMemoryBound(to: Float.self)
            for i in 0..<output.count { output[i] = source[i] }
        } else if audio.count >= 2, let left = audio[0].mData, let right = audio[1].mData {
            let l = left.assumingMemoryBound(to: Float.self)
            let r = right.assumingMemoryBound(to: Float.self)
            for i in 0..<frames { output[i * 2] = l[i]; output[i * 2 + 1] = r[i] }
        } else { return }
        output.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            var remaining = raw.count
            var pointer = base
            while remaining > 0 {
                let written = write(STDOUT_FILENO, pointer, remaining)
                if written <= 0 { return }
                pointer = pointer.advanced(by: written)
                remaining -= written
            }
        }
    }
}

@main struct CaptureApp {
    static func main() async {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
            guard let display = content.displays.first else { throw NSError(domain: "8EH", code: 1, userInfo: [NSLocalizedDescriptionKey: "Tidak ada display untuk capture audio"]) }
            let studio = content.applications.filter { $0.bundleIdentifier == "com.eighteh.studio" }
            let filter = SCContentFilter(display: display, excludingApplications: studio, exceptingWindows: [])
            let configuration = SCStreamConfiguration()
            configuration.capturesAudio = true
            configuration.excludesCurrentProcessAudio = true
            configuration.sampleRate = 44_100
            configuration.channelCount = 2
            configuration.width = 2
            configuration.height = 2
            let sink = AudioSink()
            let stream = SCStream(filter: filter, configuration: configuration, delegate: nil)
            try stream.addStreamOutput(sink, type: .audio, sampleHandlerQueue: DispatchQueue(label: "com.eighteh.studio.desktop-audio"))
            try await stream.startCapture()
            FileHandle.standardError.write(Data("READY\n".utf8))
            while true { try await Task.sleep(nanoseconds: 1_000_000_000) }
        } catch {
            FileHandle.standardError.write(Data("Desktop audio gagal: \(error.localizedDescription). Periksa System Settings > Privacy & Security > Screen Recording, lalu buka ulang 8EH Studio.\n".utf8))
            exit(1)
        }
    }
}
