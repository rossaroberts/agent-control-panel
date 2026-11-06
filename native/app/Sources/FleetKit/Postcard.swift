// Postcard, the subset fleetd's frames use (https://postcard.jamesmunns.com/wire-format). Positional, no field names:
// - u16 / u32 / u64 / usize: LEB128 varints (7 bits a byte, low first, high bit = more);
// - i64: zigzag, then a varint;  u8: one byte;  bool: one byte, 0 or 1;
// - String and Vec<u8>: a varint length, then the bytes (UTF-8 for a String);  Vec<T>: a varint length, then each T;
// - Option<T>: a byte 0 (None) or 1 then the T;  enum: a varint variant index, then the variant's fields.
// The reader is strict: what Rust's postcard would refuse it refuses too (an over-long or overflowing varint, a bool or
// Option tag other than 0 or 1, bad UTF-8, a length past the end), plus trailing bytes after a frame.

import Foundation

public enum PostcardError: Error, Equatable, Sendable {
    /// The bytes ended inside a value.
    case unexpectedEnd
    /// A varint longer than its type allows, or with a value past the type's maximum.
    case badVarint
    case badBool(UInt8)
    case badOptionTag(UInt8)
    case badUTF8
    /// A variant index the enum does not have.
    case badVariant(type: String, index: UInt32)
    /// Bytes left over after the frame.
    case trailingBytes(Int)
    /// A frame body over `Frame.maxFrame` bytes, which no reader accepts (`wire.rs` `encode` refuses it too).
    case frameTooLarge(Int)
}

/// Reads postcard values off the front of a byte buffer.
public struct PostcardReader {
    private let bytes: [UInt8]
    private var at = 0

    public init(_ bytes: [UInt8]) { self.bytes = bytes }
    public init(_ data: Data) { self.bytes = [UInt8](data) }

    public var remaining: Int { bytes.count - at }

    public mutating func u8() throws -> UInt8 {
        guard at < bytes.count else { throw PostcardError.unexpectedEnd }
        defer { at += 1 }
        return bytes[at]
    }

    /// A varint of at most `bits` bits.
    private mutating func varint(bits: Int) throws -> UInt64 {
        let maxBytes = (bits + 6) / 7
        var value: UInt64 = 0
        for i in 0..<maxBytes {
            let b = try u8()
            let shift = UInt64(7 * i)
            let payload = UInt64(b & 0x7f)
            if i == maxBytes - 1 {
                // The last byte may carry only the bits the type has left, and no continuation.
                let left = bits - 7 * i
                guard b & 0x80 == 0, payload >> left == 0 else { throw PostcardError.badVarint }
            }
            value |= payload << shift
            if b & 0x80 == 0 { return value }
        }
        throw PostcardError.badVarint
    }

    public mutating func u16() throws -> UInt16 { UInt16(try varint(bits: 16)) }
    public mutating func u32() throws -> UInt32 { UInt32(try varint(bits: 32)) }
    public mutating func u64() throws -> UInt64 { try varint(bits: 64) }

    public mutating func i64() throws -> Int64 {
        let z = try varint(bits: 64)
        return Int64(bitPattern: (z >> 1) ^ (0 &- (z & 1)))
    }

    public mutating func bool() throws -> Bool {
        switch try u8() {
        case 0: return false
        case 1: return true
        case let b: throw PostcardError.badBool(b)
        }
    }

    /// A length, which must fit in what is left (every item is at least a byte).
    private mutating func length() throws -> Int {
        let n = try varint(bits: 64)
        guard n <= UInt64(remaining) else { throw PostcardError.unexpectedEnd }
        return Int(n)
    }

    public mutating func bytesValue() throws -> [UInt8] {
        let n = try length()
        defer { at += n }
        return Array(bytes[at..<at + n])
    }

    public mutating func string() throws -> String {
        guard let s = String(bytes: try bytesValue(), encoding: .utf8) else { throw PostcardError.badUTF8 }
        return s
    }

    public mutating func option<T>(_ read: (inout PostcardReader) throws -> T) throws -> T? {
        switch try u8() {
        case 0: return nil
        case 1: return try read(&self)
        case let b: throw PostcardError.badOptionTag(b)
        }
    }

    public mutating func vec<T>(_ read: (inout PostcardReader) throws -> T) throws -> [T] {
        let n = try length()
        var out: [T] = []
        // A length is checked against the bytes left, not against what its items take: reserve a little only.
        out.reserveCapacity(min(n, 1024))
        for _ in 0..<n { out.append(try read(&self)) }
        return out
    }

    public mutating func variant() throws -> UInt32 { try u32() }

    /// Everything has been read.
    public func finish() throws {
        guard remaining == 0 else { throw PostcardError.trailingBytes(remaining) }
    }
}

/// Writes postcard values to a growing byte buffer.
public struct PostcardWriter {
    public private(set) var bytes: [UInt8] = []

    public init() {}

    public mutating func u8(_ v: UInt8) { bytes.append(v) }

    private mutating func varint(_ v: UInt64) {
        var v = v
        while v >= 0x80 {
            bytes.append(UInt8(v & 0x7f) | 0x80)
            v >>= 7
        }
        bytes.append(UInt8(v))
    }

    public mutating func u16(_ v: UInt16) { varint(UInt64(v)) }
    public mutating func u32(_ v: UInt32) { varint(UInt64(v)) }
    public mutating func u64(_ v: UInt64) { varint(v) }
    public mutating func i64(_ v: Int64) { varint(UInt64(bitPattern: (v << 1) ^ (v >> 63))) }
    public mutating func bool(_ v: Bool) { bytes.append(v ? 1 : 0) }

    public mutating func bytesValue<C: Collection>(_ v: C) where C.Element == UInt8 {
        varint(UInt64(v.count))
        bytes.append(contentsOf: v)
    }

    public mutating func string(_ v: String) { bytesValue(Array(v.utf8)) }

    public mutating func option<T>(_ v: T?, _ write: (inout PostcardWriter, T) -> Void) {
        if let v {
            bytes.append(1)
            write(&self, v)
        } else {
            bytes.append(0)
        }
    }

    public mutating func vec<T>(_ v: [T], _ write: (inout PostcardWriter, T) -> Void) {
        varint(UInt64(v.count))
        for x in v { write(&self, x) }
    }

    public mutating func variant(_ i: UInt32) { u32(i) }
}

/// A type with a postcard form: read in, and written out, field by field in the Rust declaration order.
public protocol Postcard {
    init(postcard r: inout PostcardReader) throws
    func write(postcard w: inout PostcardWriter)
}

extension PostcardReader {
    public mutating func value<T: Postcard>(_: T.Type = T.self) throws -> T { try T(postcard: &self) }
}

extension PostcardWriter {
    public mutating func value<T: Postcard>(_ v: T) { v.write(postcard: &self) }
}
