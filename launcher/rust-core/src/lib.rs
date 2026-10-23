#![no_std]
#![allow(non_snake_case, non_camel_case_types)]

use core::arch::asm;
use core::ffi::c_void;
use core::panic::PanicInfo;
use core::ptr;
use core::slice;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unsafe { abort() }
}

type jint = i32;
type jlong = i64;
type jsize = i32;
type jobject = *mut c_void;
type jstring = jobject;
type c_int = i32;
type c_char = u8;

const O_RDONLY: c_int = 0;
const O_WRONLY: c_int = 1;
const O_CREAT: c_int = 0o100;
const O_TRUNC: c_int = 0o1000;

extern "C" {
    fn abort() -> !;
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn rename(old: *const c_char, new: *const c_char) -> c_int;
    fn unlink(path: *const c_char) -> c_int;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn AndroidBitmap_getInfo(env: *mut JNIEnv, jbitmap: jobject, info: *mut AndroidBitmapInfo) -> c_int;
    fn AndroidBitmap_lockPixels(env: *mut JNIEnv, jbitmap: jobject, addr_ptr: *mut *mut c_void) -> c_int;
    fn AndroidBitmap_unlockPixels(env: *mut JNIEnv, jbitmap: jobject) -> c_int;
}

#[repr(C)]
pub struct JNINativeInterface {
    reserved: [*mut c_void; 169],
    GetStringUTFLength: Option<unsafe extern "C" fn(*mut JNIEnv, jstring) -> jsize>,
    GetStringUTFChars: Option<unsafe extern "C" fn(*mut JNIEnv, jstring, *mut u8) -> *const c_char>,
    ReleaseStringUTFChars: Option<unsafe extern "C" fn(*mut JNIEnv, jstring, *const c_char)>,
}

type JNIEnv = *const JNINativeInterface;

#[repr(C)]
struct AndroidBitmapInfo {
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    flags: u32,
}

const ANDROID_BITMAP_FORMAT_RGBA_8888: i32 = 1;
const ANDROID_BITMAP_FORMAT_RGB_565: i32 = 4;
const TLZ4_MAGIC: u32 = 0x345A4C54; // "TLZ4" in little-endian
const TPCK_MAGIC: u32 = 0x4B435054; // "TPCK" in little-endian
const MAX_PACK_ENTRIES: usize = 256;

// Persistent scratch buffer for single-threaded AppLoader.WORKER I/O: eliminates per-tile
// malloc/free (and Bionic's underlying mmap/madvise syscalls on >64 KB buffers).
static mut IO_BUF: *mut u8 = ptr::null_mut();
static mut IO_CAP: usize = 0;

// In-memory contiguous pack archive loaded in one read() syscall on cold start.
static mut PACK_BUF: *mut u8 = ptr::null_mut();
static mut PACK_CAP: usize = 0;
static mut PACK_COUNT: usize = 0;
static mut PACK_OFFS: [u32; MAX_PACK_ENTRIES] = [0; MAX_PACK_ENTRIES];
static mut PACK_LENS: [u32; MAX_PACK_ENTRIES] = [0; MAX_PACK_ENTRIES];

#[inline(always)]
unsafe fn ensure_io_buf(need: usize) -> *mut u8 {
    if need <= IO_CAP && !IO_BUF.is_null() {
        return IO_BUF;
    }
    let cap = need.max(262_144);
    let p = realloc(IO_BUF as *mut c_void, cap) as *mut u8;
    if !p.is_null() {
        IO_BUF = p;
        IO_CAP = cap;
    }
    p
}

#[inline(always)]
unsafe fn ensure_pack_buf(need: usize) -> *mut u8 {
    if need <= PACK_CAP && !PACK_BUF.is_null() {
        return PACK_BUF;
    }
    let cap = need.max(1_572_864); // 1.5 MB initial capacity for all 22+ tiles
    let p = realloc(PACK_BUF as *mut c_void, cap) as *mut u8;
    if !p.is_null() {
        PACK_BUF = p;
        PACK_CAP = cap;
    }
    p
}

struct CPath {
    buf: [u8; 512],
    len: usize,
}

impl CPath {
    unsafe fn from_jstring(env: *mut JNIEnv, s: jstring) -> Option<Self> {
        if s.is_null() {
            return None;
        }
        let iface = &**env;
        let get_len = iface.GetStringUTFLength?;
        let get_utf = iface.GetStringUTFChars?;
        let rel_utf = iface.ReleaseStringUTFChars?;
        let len = get_len(env, s) as usize;
        if len == 0 || len >= 500 {
            return None;
        }
        let ptr = get_utf(env, s, ptr::null_mut());
        if ptr.is_null() {
            return None;
        }
        let mut out = Self {
            buf: [0u8; 512],
            len,
        };
        ptr::copy_nonoverlapping(ptr, out.buf.as_mut_ptr(), len);
        out.buf[len] = 0;
        rel_utf(env, s, ptr);
        Some(out)
    }

    #[inline(always)]
    fn with_tmp(&self) -> Self {
        let mut t = Self {
            buf: self.buf,
            len: self.len + 4,
        };
        t.buf[self.len..self.len + 4].copy_from_slice(b".tmp");
        t.buf[self.len + 4] = 0;
        t
    }
}

#[inline(always)]
unsafe fn write_all_fd(fd: c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = write(fd, buf.as_ptr() as *const c_void, buf.len());
        if n <= 0 {
            return false;
        }
        buf = &buf[n as usize..];
    }
    true
}

#[inline(always)]
unsafe fn read_u32_le(ptr: *const u8) -> u32 {
    ptr::read_unaligned(ptr as *const u32).to_le()
}

#[inline(always)]
unsafe fn read_u64_le(ptr: *const u8) -> u64 {
    ptr::read_unaligned(ptr as *const u64).to_le()
}

// ---- 1. Inline ARMv7 / AArch64 NEON Assembly Primitives ----

/// Copies at least `len` bytes in 16-byte NEON chunks (caller guarantees `dst` has >= 16 bytes
/// of slack or handles the tail).
#[cfg(target_arch = "arm")]
#[inline(always)]
unsafe fn neon_wild_copy_16(dst: *mut u8, src: *const u8, len: usize) {
    let end = dst.add(len);
    asm!(
        "2:",
        "vld1.8 {{d0, d1}}, [{src}]!",
        "vst1.8 {{d0, d1}}, [{dst}]!",
        "cmp {dst}, {end}",
        "blo 2b",
        src = inout(reg) src => _,
        dst = inout(reg) dst => _,
        end = in(reg) end,
        out("d0") _,
        out("d1") _,
        options(nostack),
    );
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_wild_copy_16(dst: *mut u8, src: *const u8, len: usize) {
    let end = dst.add(len);
    asm!(
        "2:",
        "ldr q0, [{src}], #16",
        "str q0, [{dst}], #16",
        "cmp {dst}, {end}",
        "b.lo 2b",
        src = inout(reg) src => _,
        dst = inout(reg) dst => _,
        end = in(reg) end,
        out("v0") _,
        options(nostack),
    );
}

/// Broadcasts a 32-bit RGBA pixel across a 128-bit NEON register and writes 16-byte chunks
/// (accelerates LZ4 `offset == 4` RLE runs on solid-color banner backgrounds by 16x).
#[cfg(target_arch = "arm")]
#[inline(always)]
unsafe fn neon_fill_u32_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "vdup.32 q0, {val}",
        "2:",
        "vst1.8 {{d0, d1}}, [{dst}]!",
        "cmp {dst}, {end}",
        "blo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("d0") _,
        out("d1") _,
        options(nostack),
    );
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_fill_u32_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "dup v0.4s, {val:w}",
        "2:",
        "str q0, [{dst}], #16",
        "cmp {dst}, {end}",
        "b.lo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("v0") _,
        options(nostack),
    );
}

/// Broadcasts a 16-bit RGB_565 pixel across a 128-bit NEON register and writes 16-byte chunks
/// (accelerates LZ4 `offset == 2` RLE runs on solid-color RGB_565 banner backgrounds by 8x).
#[cfg(target_arch = "arm")]
#[inline(always)]
unsafe fn neon_fill_u16_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "vdup.16 q0, {val}",
        "2:",
        "vst1.8 {{d0, d1}}, [{dst}]!",
        "cmp {dst}, {end}",
        "blo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("d0") _,
        out("d1") _,
        options(nostack),
    );
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_fill_u16_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "dup v0.8h, {val:w}",
        "2:",
        "str q0, [{dst}], #16",
        "cmp {dst}, {end}",
        "b.lo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("v0") _,
        options(nostack),
    );
}

/// Broadcasts a single byte across a 128-bit NEON register and writes 16-byte chunks (`offset == 1`).
#[cfg(target_arch = "arm")]
#[inline(always)]
unsafe fn neon_fill_u8_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "vdup.8 q0, {val}",
        "2:",
        "vst1.8 {{d0, d1}}, [{dst}]!",
        "cmp {dst}, {end}",
        "blo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("d0") _,
        out("d1") _,
        options(nostack),
    );
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_fill_u8_16(dst: *mut u8, val: u32, len: usize) {
    let end = dst.add(len);
    asm!(
        "dup v0.16b, {val:w}",
        "2:",
        "str q0, [{dst}], #16",
        "cmp {dst}, {end}",
        "b.lo 2b",
        dst = inout(reg) dst => _,
        end = in(reg) end,
        val = in(reg) val,
        out("v0") _,
        options(nostack),
    );
}

// ---- 2. SWAR + NEON LZ4 Block Compressor & Decompressor ----

unsafe fn lz4_compress_into(input: &[u8], out: *mut u8) -> usize {
    let len = input.len();
    let in_ptr = input.as_ptr();
    ptr::write_unaligned(out as *mut u32, (len as u32).to_le());
    let mut op = 4usize;

    if len < 16 {
        *out.add(op) = (len as u8) << 4;
        op += 1;
        ptr::copy_nonoverlapping(in_ptr, out.add(op), len);
        return op + len;
    }

    let mut dict = [0u32; 4096];
    let mut anchor = 0usize;
    let mut ip = 0usize;
    let limit = len.saturating_sub(12);
    let match_limit = len.saturating_sub(8);

    while ip < limit {
        let seq = read_u32_le(in_ptr.add(ip));
        let hash = ((seq.wrapping_mul(2654435761)) >> 20) as usize;
        let ref_idx = *dict.get_unchecked(hash) as usize;
        *dict.get_unchecked_mut(hash) = ip as u32;

        let offset = ip.wrapping_sub(ref_idx);
        if offset > 0 && offset <= 65535 && read_u32_le(in_ptr.add(ref_idx)) == seq {
            let lit_len = ip - anchor;
            let mut match_len = 4usize;

            // 64-bit SWAR XOR + trailing_zeros (compiles to `eor` + `rbit` + `clz` on ARM):
            // compares 8 bytes per cycle instead of 1 byte per cycle.
            while ip + match_len <= match_limit {
                let diff = read_u64_le(in_ptr.add(ref_idx + match_len))
                    ^ read_u64_le(in_ptr.add(ip + match_len));
                if diff == 0 {
                    match_len += 8;
                } else {
                    match_len += (diff.trailing_zeros() >> 3) as usize;
                    break;
                }
            }
            while ip + match_len < len.saturating_sub(5)
                && *in_ptr.add(ref_idx + match_len) == *in_ptr.add(ip + match_len)
            {
                match_len += 1;
            }

            let lit_code = lit_len.min(15) as u8;
            let match_extra = match_len - 4;
            let match_code = match_extra.min(15) as u8;
            *out.add(op) = (lit_code << 4) | match_code;
            op += 1;

            if lit_len >= 15 {
                let mut rem = lit_len - 15;
                while rem >= 255 {
                    *out.add(op) = 255;
                    op += 1;
                    rem -= 255;
                }
                *out.add(op) = rem as u8;
                op += 1;
            }
            ptr::copy_nonoverlapping(in_ptr.add(anchor), out.add(op), lit_len);
            op += lit_len;

            *out.add(op) = (offset & 0xFF) as u8;
            *out.add(op + 1) = ((offset >> 8) & 0xFF) as u8;
            op += 2;

            if match_extra >= 15 {
                let mut rem = match_extra - 15;
                while rem >= 255 {
                    *out.add(op) = 255;
                    op += 1;
                    rem -= 255;
                }
                *out.add(op) = rem as u8;
                op += 1;
            }

            ip += match_len;
            anchor = ip;
        } else {
            ip += 1 + ((ip - anchor) >> 5);
        }
    }

    let last_lit = len - anchor;
    let lit_code = last_lit.min(15) as u8;
    *out.add(op) = lit_code << 4;
    op += 1;
    if last_lit >= 15 {
        let mut rem = last_lit - 15;
        while rem >= 255 {
            *out.add(op) = 255;
            op += 1;
            rem -= 255;
        }
        *out.add(op) = rem as u8;
        op += 1;
    }
    ptr::copy_nonoverlapping(in_ptr.add(anchor), out.add(op), last_lit);
    op + last_lit
}

unsafe fn lz4_decompress_into(src: &[u8], dst: &mut [u8]) -> bool {
    let src_len = src.len();
    if src_len < 4 {
        return false;
    }
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();
    let expected_len = read_u32_le(src_ptr) as usize;
    if dst.len() < expected_len {
        return false;
    }
    let safe_dst_16 = expected_len.saturating_sub(16);
    let safe_src_16 = src_len.saturating_sub(16);
    let mut sp = 4usize;
    let mut dp = 0usize;

    while sp < src_len {
        let token = *src_ptr.add(sp) as usize;
        sp += 1;

        let mut lit_len = token >> 4;
        if lit_len == 15 {
            loop {
                if sp >= src_len {
                    return false;
                }
                let s = *src_ptr.add(sp) as usize;
                sp += 1;
                lit_len += s;
                if s != 255 {
                    break;
                }
            }
        }

        if sp + lit_len > src_len || dp + lit_len > expected_len {
            return false;
        }

        // Fast NEON 16-byte wild copy when away from buffer edges
        if lit_len > 0 {
            if dp + lit_len <= safe_dst_16 && sp + lit_len <= safe_src_16 {
                neon_wild_copy_16(dst_ptr.add(dp), src_ptr.add(sp), lit_len);
            } else {
                ptr::copy_nonoverlapping(src_ptr.add(sp), dst_ptr.add(dp), lit_len);
            }
            sp += lit_len;
            dp += lit_len;
        }

        if sp >= src_len {
            break;
        }

        if sp + 2 > src_len {
            return false;
        }
        let offset = (*src_ptr.add(sp) as usize) | ((*src_ptr.add(sp + 1) as usize) << 8);
        sp += 2;
        if offset == 0 || offset > dp {
            return false;
        }

        let mut match_len = (token & 0x0F) + 4;
        if (token & 0x0F) == 15 {
            loop {
                if sp >= src_len {
                    return false;
                }
                let s = *src_ptr.add(sp) as usize;
                sp += 1;
                match_len += s;
                if s != 255 {
                    break;
                }
            }
        }

        if dp + match_len > expected_len {
            return false;
        }

        let match_ptr = dst_ptr.add(dp - offset);
        let out_ptr = dst_ptr.add(dp);

        if offset >= 16 && dp + match_len <= safe_dst_16 {
            // Non-overlapping >=16B offset: 128-bit NEON wild copy
            neon_wild_copy_16(out_ptr, match_ptr, match_len);
        } else if offset == 4 && dp + match_len <= safe_dst_16 {
            // Solid RGBA_8888 pixel run: broadcast 32-bit pixel into 128-bit NEON register
            let px = ptr::read_unaligned(match_ptr as *const u32);
            neon_fill_u32_16(out_ptr, px, match_len);
        } else if offset == 2 && dp + match_len <= safe_dst_16 {
            // Solid RGB_565 pixel run (most common RLE pattern in 16-bit banners!):
            // broadcast 16-bit pixel into 128-bit NEON register (8 pixels / 16 bytes per store)
            let px = ptr::read_unaligned(match_ptr as *const u16) as u32;
            neon_fill_u16_16(out_ptr, px, match_len);
        } else if offset == 1 && dp + match_len <= safe_dst_16 {
            // Solid byte run (e.g. black/white/transparent spans)
            neon_fill_u8_16(out_ptr, *match_ptr as u32, match_len);
        } else if offset >= match_len {
            ptr::copy_nonoverlapping(match_ptr, out_ptr, match_len);
        } else {
            for i in 0..match_len {
                *out_ptr.add(i) = *match_ptr.add(i);
            }
        }
        dp += match_len;
    }
    dp == expected_len
}

// ---- 3. Bitwise / SWAR Division-Free Vivid Color Sampling ----

#[inline]
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let rf = r as f32 * (1.0 / 255.0);
    let gf = g as f32 * (1.0 / 255.0);
    let bf = b as f32 * (1.0 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let d = max - min;
    let v = max;
    let s = if max == 0.0 { 0.0 } else { d / max };
    let mut h = 0.0;
    if d != 0.0 {
        if max == rf {
            h = (gf - bf) / d + if gf < bf { 6.0 } else { 0.0 };
        } else if max == gf {
            h = (bf - rf) / d + 2.0;
        } else {
            h = (rf - gf) / d + 4.0;
        }
        h *= 60.0;
    }
    (h, s, v)
}

#[inline]
fn hsv_to_argb(h: f32, s: f32, v: f32) -> i32 {
    if s <= 0.0 {
        let c = (v * 255.0 + 0.5) as i32;
        return (0xFFu32 as i32) << 24 | (c << 16) | (c << 8) | c;
    }
    let mut hh = h * (1.0 / 60.0);
    if hh >= 6.0 {
        hh -= 6.0;
    }
    let i = hh as i32;
    let f = hh - (i as f32);
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (rf, gf, bf) = match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    let r = (rf * 255.0 + 0.5).clamp(0.0, 255.0) as i32;
    let g = (gf * 255.0 + 0.5).clamp(0.0, 255.0) as i32;
    let b = (bf * 255.0 + 0.5).clamp(0.0, 255.0) as i32;
    (0xFFu32 as i32) << 24 | (r << 16) | (g << 8) | b
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_vividColor(
    env: *mut JNIEnv,
    _clazz: jobject,
    jbitmap: jobject,
) -> jint {
    if jbitmap.is_null() {
        return 0xFF444444u32 as jint;
    }
    let mut info = AndroidBitmapInfo {
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
        flags: 0,
    };
    if AndroidBitmap_getInfo(env, jbitmap, &mut info) != 0 || info.width == 0 || info.height == 0 {
        return 0xFF444444u32 as jint;
    }
    let mut ptr: *mut c_void = ptr::null_mut();
    if AndroidBitmap_lockPixels(env, jbitmap, &mut ptr) != 0 || ptr.is_null() {
        return 0xFF444444u32 as jint;
    }

    let w = info.width as usize;
    let h = info.height as usize;
    let stride = info.stride as usize;
    let base = ptr as *const u8;

    let n = 12usize;
    let mut best_px = 0x00444444u32;
    let mut best_score_q16 = 0u32;
    // SWAR accumulator: bits 0..15 hold sum_r, bits 16..31 hold sum_b (max 144 * 255 = 36720 < 65535)
    let mut sum_rb = 0u32;
    let mut sum_g = 0u32;

    if info.format == ANDROID_BITMAP_FORMAT_RGBA_8888 {
        // Precompute X byte offsets once (12 entries) instead of multiplying inside the 144-iter loop
        let mut x_off = [0usize; 12];
        for gx in 0..n {
            x_off[gx] = ((gx * w + (w >> 1)) / n) << 2;
        }
        for gy in 0..n {
            let row = base.add(((gy * h + (h >> 1)) / n) * stride);
            for gx in 0..n {
                let px = ptr::read_unaligned(row.add(x_off[gx]) as *const u32);
                // Single-instruction SWAR addition of R (bits 0..7) and B (bits 16..23)
                sum_rb += px & 0x00FF00FF;
                let r = px & 0xFF;
                let g = (px >> 8) & 0xFF;
                let b = (px >> 16) & 0xFF;
                sum_g += g;

                let max = r.max(g).max(b);
                let min = r.min(g).min(b);
                let delta = max - min;
                if delta == 0 {
                    continue;
                }
                // Algebraic cancellation:
                // For max <= 182: val * 1.4 <= 1.0, so score = ((max - min) / max) * (1.4 * max / 255)
                // = (max - min) * 1.4 / 255. In Q16 fixed-point: delta * 360. Zero division!
                // For max > 182: score = (max - min) / max. Cross-multiply before dividing so udiv
                // only executes when a new best pixel is found (~1-3 times per tile).
                if max <= 182 {
                    let score_q16 = delta * 360;
                    if score_q16 > best_score_q16 {
                        best_score_q16 = score_q16;
                        best_px = px;
                    }
                } else if (delta << 16) > best_score_q16 * max {
                    best_score_q16 = (delta << 16) / max;
                    best_px = px;
                }
            }
        }
    } else if info.format == ANDROID_BITMAP_FORMAT_RGB_565 {
        for gy in 0..n {
            let row = base.add(((gy * h + (h >> 1)) / n) * stride);
            for gx in 0..n {
                let x = (gx * w + (w >> 1)) / n;
                let v = ptr::read_unaligned(row.add(x << 1) as *const u16) as u32;
                let r5 = (v >> 11) & 0x1F;
                let g6 = (v >> 5) & 0x3F;
                let b5 = v & 0x1F;
                let r = (r5 << 3) | (r5 >> 2);
                let g = (g6 << 2) | (g6 >> 4);
                let b = (b5 << 3) | (b5 >> 2);
                sum_rb += r | (b << 16);
                sum_g += g;
                let max = r.max(g).max(b);
                let min = r.min(g).min(b);
                let delta = max - min;
                if max <= 182 {
                    let score_q16 = delta * 360;
                    if score_q16 > best_score_q16 {
                        best_score_q16 = score_q16;
                        best_px = r | (g << 8) | (b << 16);
                    }
                } else if (delta << 16) > best_score_q16 * max {
                    best_score_q16 = (delta << 16) / max;
                    best_px = r | (g << 8) | (b << 16);
                }
            }
        }
    }

    AndroidBitmap_unlockPixels(env, jbitmap);

    // 0.25 in Q16 is 16384; division by 144 via reciprocal multiplication: (x * 455 + 32768) >> 16
    if best_score_q16 < 16384 {
        let sum_r = sum_rb & 0xFFFF;
        let sum_b = sum_rb >> 16;
        let ar = ((sum_r * 455) >> 16) as u8;
        let ag = ((sum_g * 455) >> 16) as u8;
        let ab = ((sum_b * 455) >> 16) as u8;
        let (h, s, v) = rgb_to_hsv(ar, ag, ab);
        return hsv_to_argb(h, s, v.max(0.55));
    }
    let best_r = (best_px & 0xFF) as u8;
    let best_g = ((best_px >> 8) & 0xFF) as u8;
    let best_b = ((best_px >> 16) & 0xFF) as u8;
    let (h, s, v) = rgb_to_hsv(best_r, best_g, best_b);
    hsv_to_argb(h, (s * 1.1).min(1.0), v.max(0.5))
}

// ---- 4. Zero-Allocation Single-Syscall Native LZ4 TileStore I/O ----

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_writeBitmapLz4(
    env: *mut JNIEnv,
    _clazz: jobject,
    jpath: jstring,
    jbitmap: jobject,
) -> jint {
    let path = match CPath::from_jstring(env, jpath) {
        Some(p) => p,
        None => return 0,
    };
    if jbitmap.is_null() {
        return 0;
    }
    let mut info = AndroidBitmapInfo {
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
        flags: 0,
    };
    if AndroidBitmap_getInfo(env, jbitmap, &mut info) != 0 || info.width == 0 || info.height == 0 {
        return 0;
    }
    let mut ptr: *mut c_void = ptr::null_mut();
    if AndroidBitmap_lockPixels(env, jbitmap, &mut ptr) != 0 || ptr.is_null() {
        return 0;
    }

    let raw_len = (info.stride * info.height) as usize;
    let max_out = 16 + raw_len + (raw_len >> 8) + 64;
    let buf_ptr = ensure_io_buf(max_out);
    if buf_ptr.is_null() {
        AndroidBitmap_unlockPixels(env, jbitmap);
        return 0;
    }

    let raw = slice::from_raw_parts(ptr as *const u8, raw_len);
    ptr::write_unaligned(buf_ptr as *mut u32, TLZ4_MAGIC.to_le());
    ptr::write_unaligned(buf_ptr.add(4) as *mut u16, (info.width as u16).to_le());
    ptr::write_unaligned(buf_ptr.add(6) as *mut u16, (info.height as u16).to_le());
    *buf_ptr.add(8) = info.format as u8;
    *buf_ptr.add(9) = 0;
    *buf_ptr.add(10) = 0;
    *buf_ptr.add(11) = 0;
    let comp_len = lz4_compress_into(raw, buf_ptr.add(12));
    AndroidBitmap_unlockPixels(env, jbitmap);

    let tmp = path.with_tmp();
    let fd = open(tmp.buf.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    let mut ok = false;
    if fd >= 0 {
        ok = write_all_fd(fd, slice::from_raw_parts(buf_ptr, 12 + comp_len));
        close(fd);
    }

    if ok && rename(tmp.buf.as_ptr(), path.buf.as_ptr()) == 0 {
        1
    } else {
        unlink(tmp.buf.as_ptr());
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_readBitmapLz4(
    env: *mut JNIEnv,
    _clazz: jobject,
    jpath: jstring,
    jbitmap: jobject,
) -> jint {
    let path = match CPath::from_jstring(env, jpath) {
        Some(p) => p,
        None => return 0,
    };
    if jbitmap.is_null() {
        return 0;
    }
    let mut info = AndroidBitmapInfo {
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
        flags: 0,
    };
    if AndroidBitmap_getInfo(env, jbitmap, &mut info) != 0 {
        return 0;
    }

    let dst_len = (info.stride * info.height) as usize;
    let max_file = 16 + dst_len + (dst_len >> 8) + 64;
    let buf_ptr = ensure_io_buf(max_file);
    if buf_ptr.is_null() {
        return 0;
    }

    let fd = open(path.buf.as_ptr(), O_RDONLY);
    if fd < 0 {
        return 0;
    }

    // Single read() syscall for both the 16-byte header and the compressed LZ4 payload
    let mut total = 0usize;
    loop {
        let n = read(
            fd,
            buf_ptr.add(total) as *mut c_void,
            max_file - total,
        );
        if n <= 0 {
            break;
        }
        total += n as usize;
        if total >= max_file {
            break;
        }
    }
    close(fd);

    if total < 16 || read_u32_le(buf_ptr) != TLZ4_MAGIC {
        return 0;
    }
    let w = u16::from_le(ptr::read_unaligned(buf_ptr.add(4) as *const u16)) as u32;
    let h = u16::from_le(ptr::read_unaligned(buf_ptr.add(6) as *const u16)) as u32;
    let fmt = *buf_ptr.add(8) as i32;
    let expected_raw = read_u32_le(buf_ptr.add(12)) as usize;
    if info.width != w || info.height != h || info.format != fmt || expected_raw != dst_len {
        return 0;
    }

    let mut ptr: *mut c_void = ptr::null_mut();
    let mut ok = false;
    if AndroidBitmap_lockPixels(env, jbitmap, &mut ptr) == 0 && !ptr.is_null() {
        let dst = slice::from_raw_parts_mut(ptr as *mut u8, dst_len);
        let comp = slice::from_raw_parts(buf_ptr.add(12), total - 12);
        ok = lz4_decompress_into(comp, dst);
        AndroidBitmap_unlockPixels(env, jbitmap);
    }
    if ok { 1 } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_readBitmapLz4Header(
    env: *mut JNIEnv,
    _clazz: jobject,
    jpath: jstring,
) -> jlong {
    let path = match CPath::from_jstring(env, jpath) {
        Some(p) => p,
        None => return 0,
    };
    let fd = open(path.buf.as_ptr(), O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut header = [0u8; 12];
    let n = read(fd, header.as_mut_ptr() as *mut c_void, 12);
    close(fd);
    if n != 12 || read_u32_le(header.as_ptr()) != TLZ4_MAGIC {
        return 0;
    }
    let w = u16::from_le_bytes([header[4], header[5]]) as u64;
    let h = u16::from_le_bytes([header[6], header[7]]) as u64;
    let fmt = header[8] as u64;
    ((fmt << 32) | (w << 16) | h) as jlong
}

// ---- 5. Contiguous Single-File Pack Archive (pack.lz4) for Zero-Syscall Cold-Start Decode ----

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_writePackLz4(
    env: *mut JNIEnv,
    _clazz: jobject,
    jmanifest: jstring,
) -> jint {
    if jmanifest.is_null() {
        return 0;
    }
    let iface = &**env;
    let (Some(get_len), Some(get_utf), Some(rel_utf)) = (
        iface.GetStringUTFLength,
        iface.GetStringUTFChars,
        iface.ReleaseStringUTFChars,
    ) else {
        return 0;
    };
    let mlen = get_len(env, jmanifest) as usize;
    if mlen == 0 || mlen > 65536 {
        return 0;
    }
    let mptr = get_utf(env, jmanifest, ptr::null_mut());
    if mptr.is_null() {
        return 0;
    }
    let mslice = slice::from_raw_parts(mptr, mlen);

    // Line 0 is the directory path; subsequent lines are tile file names.
    let mut pos = 0usize;
    while pos < mlen && mslice[pos] != b'\n' {
        pos += 1;
    }
    if pos == 0 || pos >= 420 {
        rel_utf(env, jmanifest, mptr);
        return 0;
    }
    let mut path_buf = [0u8; 512];
    path_buf[..pos].copy_from_slice(&mslice[..pos]);
    path_buf[pos] = b'/';
    let base_len = pos + 1;
    pos += 1;

    // Count entries
    let mut count = 0u32;
    let mut scan = pos;
    while scan < mlen {
        let start = scan;
        while scan < mlen && mslice[scan] != b'\n' {
            scan += 1;
        }
        if scan > start {
            count += 1;
        }
        scan += 1;
    }
    if count as usize > MAX_PACK_ENTRIES {
        rel_utf(env, jmanifest, mptr);
        return 0;
    }

    // Open dir/pack.lz4.tmp
    let mut tmp_path = path_buf;
    tmp_path[base_len..base_len + 12].copy_from_slice(b"pack.lz4.tmp");
    tmp_path[base_len + 12] = 0;

    let mut final_path = path_buf;
    final_path[base_len..base_len + 8].copy_from_slice(b"pack.lz4");
    final_path[base_len + 8] = 0;

    let out_fd = open(tmp_path.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if out_fd < 0 {
        rel_utf(env, jmanifest, mptr);
        return 0;
    }

    let scratch = ensure_io_buf(262_144);
    if scratch.is_null() {
        close(out_fd);
        unlink(tmp_path.as_ptr());
        rel_utf(env, jmanifest, mptr);
        return 0;
    }

    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(&TPCK_MAGIC.to_le_bytes());
    hdr[4..8].copy_from_slice(&count.to_le_bytes());
    let mut ok = write_all_fd(out_fd, &hdr);

    while ok && pos < mlen {
        let start = pos;
        while pos < mlen && mslice[pos] != b'\n' {
            pos += 1;
        }
        let name_len = pos - start;
        pos += 1;
        if name_len == 0 {
            continue;
        }
        if base_len + name_len >= 500 {
            ok = false;
            break;
        }
        path_buf[base_len..base_len + name_len].copy_from_slice(&mslice[start..start + name_len]);
        path_buf[base_len + name_len] = 0;

        let in_fd = open(path_buf.as_ptr(), O_RDONLY);
        if in_fd < 0 {
            ok = false;
            break;
        }
        let mut chunk_len = 0usize;
        loop {
            let n = read(
                in_fd,
                scratch.add(4 + chunk_len) as *mut c_void,
                IO_CAP.saturating_sub(4 + chunk_len),
            );
            if n <= 0 {
                break;
            }
            chunk_len += n as usize;
        }
        close(in_fd);
        if chunk_len < 16 {
            ok = false;
            break;
        }
        ptr::write_unaligned(scratch as *mut u32, (chunk_len as u32).to_le());
        ok = write_all_fd(out_fd, slice::from_raw_parts(scratch, 4 + chunk_len));
    }

    close(out_fd);
    rel_utf(env, jmanifest, mptr);

    if ok && rename(tmp_path.as_ptr(), final_path.as_ptr()) == 0 {
        1
    } else {
        unlink(tmp_path.as_ptr());
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_loadPackLz4(
    env: *mut JNIEnv,
    _clazz: jobject,
    jpath: jstring,
) -> jint {
    PACK_COUNT = 0;
    let path = match CPath::from_jstring(env, jpath) {
        Some(p) => p,
        None => return -1,
    };
    let fd = open(path.buf.as_ptr(), O_RDONLY);
    if fd < 0 {
        return -1;
    }

    let mut buf = ensure_pack_buf(1_572_864);
    if buf.is_null() {
        close(fd);
        return -1;
    }

    let mut total = 0usize;
    loop {
        if total + 65536 > PACK_CAP {
            buf = ensure_pack_buf(PACK_CAP * 2);
            if buf.is_null() {
                close(fd);
                return -1;
            }
        }
        let n = read(fd, buf.add(total) as *mut c_void, PACK_CAP - total);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    close(fd);

    if total < 8 || read_u32_le(buf) != TPCK_MAGIC {
        return -1;
    }
    let count = read_u32_le(buf.add(4)) as usize;
    if count > MAX_PACK_ENTRIES {
        return -1;
    }

    let mut off = 8usize;
    for i in 0..count {
        if off + 4 > total {
            return -1;
        }
        let clen = read_u32_le(buf.add(off)) as usize;
        off += 4;
        if clen < 16 || off + clen > total || read_u32_le(buf.add(off)) != TLZ4_MAGIC {
            return -1;
        }
        PACK_OFFS[i] = off as u32;
        PACK_LENS[i] = clen as u32;
        off += clen;
    }
    if off != total {
        return -1;
    }
    PACK_COUNT = count;
    count as jint
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_readPackHeader(
    _env: *mut JNIEnv,
    _clazz: jobject,
    index: jint,
) -> jlong {
    let idx = index as usize;
    if idx >= PACK_COUNT || PACK_BUF.is_null() {
        return 0;
    }
    let entry = PACK_BUF.add(PACK_OFFS[idx] as usize);
    let w = u16::from_le(ptr::read_unaligned(entry.add(4) as *const u16)) as u64;
    let h = u16::from_le(ptr::read_unaligned(entry.add(6) as *const u16)) as u64;
    let fmt = *entry.add(8) as u64;
    ((fmt << 32) | (w << 16) | h) as jlong
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_readPackBitmap(
    env: *mut JNIEnv,
    _clazz: jobject,
    index: jint,
    jbitmap: jobject,
) -> jint {
    let idx = index as usize;
    if idx >= PACK_COUNT || PACK_BUF.is_null() || jbitmap.is_null() {
        return 0;
    }
    let mut info = AndroidBitmapInfo {
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
        flags: 0,
    };
    if AndroidBitmap_getInfo(env, jbitmap, &mut info) != 0 {
        return 0;
    }
    let entry = PACK_BUF.add(PACK_OFFS[idx] as usize);
    let clen = PACK_LENS[idx] as usize;
    let w = u16::from_le(ptr::read_unaligned(entry.add(4) as *const u16)) as u32;
    let h = u16::from_le(ptr::read_unaligned(entry.add(6) as *const u16)) as u32;
    let fmt = *entry.add(8) as i32;
    let expected_raw = read_u32_le(entry.add(12)) as usize;
    let dst_len = (info.stride * info.height) as usize;
    if info.width != w || info.height != h || info.format != fmt || expected_raw != dst_len {
        return 0;
    }

    let mut ptr: *mut c_void = ptr::null_mut();
    let mut ok = false;
    if AndroidBitmap_lockPixels(env, jbitmap, &mut ptr) == 0 && !ptr.is_null() {
        let dst = slice::from_raw_parts_mut(ptr as *mut u8, dst_len);
        let comp = slice::from_raw_parts(entry.add(12), clen - 12);
        ok = lz4_decompress_into(comp, dst);
        AndroidBitmap_unlockPixels(env, jbitmap);
    }
    if ok { 1 } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_gathin_tvhome_RustCore_releaseBuffers(
    _env: *mut JNIEnv,
    _clazz: jobject,
) {
    if !PACK_BUF.is_null() {
        free(PACK_BUF as *mut c_void);
        PACK_BUF = ptr::null_mut();
        PACK_CAP = 0;
        PACK_COUNT = 0;
    }
    if !IO_BUF.is_null() {
        free(IO_BUF as *mut c_void);
        IO_BUF = ptr::null_mut();
        IO_CAP = 0;
    }
}

