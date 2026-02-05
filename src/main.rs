// UnQuantum - A modern decompressor for the Quantum archive format (.Q)
//
// Copyright (c) 2026 David Carrero Fernandez-Baillo
// https://carrero.es
// License: MIT (see LICENSE file)
// Repository: https://github.com/dcarrero/unquantum
//
// The Quantum compression format was created by David Stafford of Cinematronics
// (Austin, TX) circa 1993-1995. It uses LZ77 combined with arithmetic coding.
//
// This implementation is based on:
// - QUANTUM.DOC (official archive format specification)
// - libmspack by Stuart Caie (https://www.cabextract.org.uk/libmspack/)
// - Research by Matthew Russotto (http://www.russotto.net/quantumcomp.html)
// - Reverse engineering of UNPAQ.EXE and PAQ.EXE v0.97 by Cinematronics
//
// This tool handles standalone .Q archive files (not CAB-embedded Quantum).

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

// ============================================================================
// Constants - Quantum static data tables
// ============================================================================

/// Magic signature for Quantum archives: 0x44 0x53 ("DS" - David Stafford)
const QTM_SIGNATURE: [u8; 2] = [0x44, 0x53];

/// Position slot base offsets (42 entries)
/// Maps position slot numbers to base match offsets.
const POSITION_BASE: [u32; 42] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384,
    512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576,
    32768, 49152, 65536, 98304, 131072, 196608, 262144, 393216, 524288,
    786432, 1048576, 1572864,
];

/// Extra bits per position slot (42 entries)
const EXTRA_BITS: [u8; 42] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9,
    10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
    19, 19,
];

/// Length slot base values (27 entries) - for selector 6 variable-length matches
const LENGTH_BASE: [u16; 27] = [
    0, 1, 2, 3, 4, 5, 6, 8, 10, 12, 14, 18, 22, 26, 30, 38, 46, 54, 62,
    78, 94, 110, 126, 158, 190, 222, 254,
];

/// Extra bits per length slot (27 entries)
const LENGTH_EXTRA: [u8; 27] = [
    0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5,
    5, 5, 5, 0,
];

// ============================================================================
// Archive structures
// ============================================================================

/// Quantum archive header (8 bytes)
struct QArchiveHeader {
    major_version: u8,
    minor_version: u8,
    num_files: u16,
    table_size: u8,
    comp_flags: u8,
}

/// A file entry within the Quantum archive
struct QFileEntry {
    name: String,
    comment: String,
    size: u32,
    time: u16,
    date: u16,
}

impl QFileEntry {
    /// Format the DOS date as a human-readable string
    fn date_string(&self) -> String {
        let day = self.date & 0x1F;
        let month = (self.date >> 5) & 0x0F;
        let year = ((self.date >> 9) & 0x7F) + 1980;
        format!("{:02}-{:02}-{:04}", day, month, year)
    }

    /// Format the DOS time as a human-readable string
    fn time_string(&self) -> String {
        let seconds = (self.time & 0x1F) * 2;
        let minutes = (self.time >> 5) & 0x3F;
        let hours = (self.time >> 11) & 0x1F;
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

// ============================================================================
// Arithmetic coding model
// ============================================================================

#[derive(Clone)]
struct ModelSym {
    sym: u16,
    cumfreq: u16,
}

struct Model {
    shift_left: i32,
    entries: usize,
    syms: Vec<ModelSym>,
}

impl Model {
    /// Create a new adaptive frequency model for symbols [start..start+len)
    fn new(start: u16, len: usize) -> Self {
        let mut syms = Vec::with_capacity(len + 1);
        for i in 0..=len {
            syms.push(ModelSym {
                sym: start + i as u16,
                cumfreq: (len - i) as u16,
            });
        }
        Model {
            shift_left: 4,
            entries: len,
            syms,
        }
    }

    /// Rescale model frequencies when cumfreq exceeds 3800
    fn update(&mut self) {
        self.shift_left -= 1;
        if self.shift_left > 0 {
            // Halve cumulative frequencies, maintaining monotonicity
            for i in (0..self.entries).rev() {
                self.syms[i].cumfreq >>= 1;
                if self.syms[i].cumfreq <= self.syms[i + 1].cumfreq {
                    self.syms[i].cumfreq = self.syms[i + 1].cumfreq + 1;
                }
            }
        } else {
            self.shift_left = 50;
            // Convert cumulative frequencies to individual frequencies
            for i in 0..self.entries {
                self.syms[i].cumfreq -= self.syms[i + 1].cumfreq;
                self.syms[i].cumfreq += 1; // prevent zero frequency
                self.syms[i].cumfreq >>= 1;
            }
            // Selection sort by frequency (descending) - matches original behavior
            for i in 0..self.entries.saturating_sub(1) {
                for j in (i + 1)..self.entries {
                    if self.syms[i].cumfreq < self.syms[j].cumfreq {
                        self.syms.swap(i, j);
                    }
                }
            }
            // Convert back to cumulative frequencies
            for i in (0..self.entries).rev() {
                self.syms[i].cumfreq += self.syms[i + 1].cumfreq;
            }
        }
    }
}

// ============================================================================
// Bit reader - MSB-first, big-endian byte pairs
// ============================================================================

struct BitReader {
    data: Vec<u8>,
    pos: usize,
    bit_buffer: u32,
    bits_left: i32,
}

impl BitReader {
    fn new(data: Vec<u8>) -> Self {
        BitReader {
            data,
            pos: 0,
            bit_buffer: 0,
            bits_left: 0,
        }
    }

    /// Read 2 bytes in big-endian order and inject 16 bits into the buffer
    fn fill(&mut self) {
        let b0 = if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            b
        } else {
            0 // pad with zeros at end of input
        };
        let b1 = if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            b
        } else {
            0
        };
        let word = ((b0 as u32) << 8) | (b1 as u32);
        // MSB inject: place new bits after existing valid bits
        // bit_buffer has valid bits at positions [31..(32-bits_left)]
        // New bits go at position (32-bits_left-16)..(32-bits_left-1)
        self.bit_buffer |= word << (32 - 16 - self.bits_left as u32);
        self.bits_left += 16;
    }

    fn ensure_bits(&mut self, n: i32) {
        while self.bits_left < n {
            self.fill();
        }
    }

    fn peek_bits(&self, n: i32) -> u32 {
        self.bit_buffer >> (32 - n as u32)
    }

    fn remove_bits(&mut self, n: i32) {
        self.bit_buffer <<= n as u32;
        self.bits_left -= n;
    }

    fn read_bits(&mut self, n: i32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.ensure_bits(n);
        let val = self.peek_bits(n);
        self.remove_bits(n);
        val
    }

    /// Read many bits - handles n > 16 by reading in chunks
    fn read_many_bits(&mut self, mut n: i32) -> u32 {
        if n == 0 {
            return 0;
        }
        let mut val: u32 = 0;
        while n > 0 {
            if self.bits_left <= 16 {
                self.fill();
            }
            let bitrun = if self.bits_left < n {
                self.bits_left
            } else {
                n
            };
            val = (val << bitrun as u32) | self.peek_bits(bitrun);
            self.remove_bits(bitrun);
            n -= bitrun;
        }
        val
    }
}

// ============================================================================
// Quantum decompressor
// ============================================================================

/// Decode a symbol from a model using arithmetic coding.
/// Updates the model frequencies and renormalizes the coder state.
fn decode_symbol(
    model: &mut Model,
    bits: &mut BitReader,
    h: &mut u16,
    l: &mut u16,
    c: &mut u16,
) -> Result<u16, String> {
    let h_val = *h as u32;
    let l_val = *l as u32;
    let c_val = *c as u32;

    // Calculate the range and find the symbol
    let range = ((h_val.wrapping_sub(l_val)) & 0xFFFF) + 1;
    let total_freq = model.syms[0].cumfreq as u32;

    if total_freq == 0 || range == 0 {
        return Err("Decompression error: zero frequency or range".to_string());
    }

    let symf = ((c_val
        .wrapping_sub(l_val)
        .wrapping_add(1)
        .wrapping_mul(total_freq))
    .wrapping_sub(1)
        / range)
        & 0xFFFF;

    // Find the symbol whose cumulative frequency bracket contains symf
    let mut i = 1usize;
    while i < model.entries {
        if (model.syms[i].cumfreq as u32) <= symf {
            break;
        }
        i += 1;
    }

    let sym = model.syms[i - 1].sym;

    // Narrow the interval
    let range2 = h_val.wrapping_sub(l_val) + 1;
    let new_h = l_val + ((model.syms[i - 1].cumfreq as u32 * range2) / total_freq) - 1;
    let new_l = l_val + ((model.syms[i].cumfreq as u32 * range2) / total_freq);

    *h = new_h as u16;
    *l = new_l as u16;

    // Update cumulative frequencies for decoded symbol
    {
        let mut j = i;
        loop {
            j -= 1;
            model.syms[j].cumfreq += 8;
            if j == 0 {
                break;
            }
        }
    }

    // Rescale if total frequency exceeds threshold
    if model.syms[0].cumfreq > 3800 {
        model.update();
    }

    // Renormalization loop
    loop {
        if (*l & 0x8000) != (*h & 0x8000) {
            if (*l & 0x4000) != 0 && (*h & 0x4000) == 0 {
                // Underflow case
                *c ^= 0x4000;
                *l &= 0x3FFF;
                *h |= 0x4000;
            } else {
                break;
            }
        }
        *l <<= 1;
        *h = (*h << 1) | 1;
        bits.ensure_bits(1);
        let bit = bits.peek_bits(1);
        bits.remove_bits(1);
        *c = (*c << 1) | (bit as u16);
    }

    Ok(sym)
}

/// Decompress a Quantum compressed data stream.
///
/// The standalone .Q format compresses all files as a single continuous stream.
/// The arithmetic coder state and adaptive models persist across file boundaries.
/// Between each file (except after the last), a 16-bit checksum is embedded in
/// the raw bit stream that must be consumed to keep the decoder in sync.
fn quantum_decompress(
    compressed_data: Vec<u8>,
    file_sizes: &[u32],
    window_bits: u8,
) -> Result<Vec<u8>, String> {
    let total_output_size: usize = file_sizes.iter().map(|&s| s as usize).sum();
    let window_size = 1usize << window_bits;
    let mut window = vec![0u8; window_size];
    let mut window_posn: usize = 0;
    let mut output = Vec::with_capacity(total_output_size);

    let mut bits = BitReader::new(compressed_data);

    // Initialize adaptive frequency models
    let i = (window_bits as usize) * 2;
    let mut model0 = Model::new(0, 64);
    let mut model1 = Model::new(64, 64);
    let mut model2 = Model::new(128, 64);
    let mut model3 = Model::new(192, 64);
    let mut model4 = Model::new(0, if i > 24 { 24 } else { i });
    let mut model5 = Model::new(0, if i > 36 { 36 } else { i });
    let mut model6 = Model::new(0, i);
    let mut model6len = Model::new(0, 27);
    let mut model7 = Model::new(0, 7);

    // Initialize arithmetic coder
    let mut h: u16 = 0xFFFF;
    let mut l: u16 = 0;
    let mut c: u16 = bits.read_bits(16) as u16;

    // Decompress each file, consuming the inter-file checksum between them
    for (file_idx, &file_size) in file_sizes.iter().enumerate() {
        let file_end = output.len() + file_size as usize;

        while output.len() < file_end {
            let selector =
                decode_symbol(&mut model7, &mut bits, &mut h, &mut l, &mut c)?;

            if selector < 4 {
                let model = match selector {
                    0 => &mut model0,
                    1 => &mut model1,
                    2 => &mut model2,
                    3 => &mut model3,
                    _ => unreachable!(),
                };
                let sym =
                    decode_symbol(model, &mut bits, &mut h, &mut l, &mut c)?;
                let byte = sym as u8;
                window[window_posn] = byte;
                window_posn = (window_posn + 1) & (window_size - 1);
                output.push(byte);
            } else {
                let (match_offset, match_length) = match selector {
                    4 => {
                        let sym = decode_symbol(
                            &mut model4,
                            &mut bits,
                            &mut h,
                            &mut l,
                            &mut c,
                        )? as usize;
                        if sym >= 42 {
                            return Err(format!(
                                "Invalid position slot {} in selector 4",
                                sym
                            ));
                        }
                        let extra =
                            bits.read_many_bits(EXTRA_BITS[sym] as i32);
                        let offset =
                            (POSITION_BASE[sym] + extra + 1) as usize;
                        (offset, 3usize)
                    }
                    5 => {
                        let sym = decode_symbol(
                            &mut model5,
                            &mut bits,
                            &mut h,
                            &mut l,
                            &mut c,
                        )? as usize;
                        if sym >= 42 {
                            return Err(format!(
                                "Invalid position slot {} in selector 5",
                                sym
                            ));
                        }
                        let extra =
                            bits.read_many_bits(EXTRA_BITS[sym] as i32);
                        let offset =
                            (POSITION_BASE[sym] + extra + 1) as usize;
                        (offset, 4usize)
                    }
                    6 => {
                        let len_sym = decode_symbol(
                            &mut model6len,
                            &mut bits,
                            &mut h,
                            &mut l,
                            &mut c,
                        )? as usize;
                        if len_sym >= 27 {
                            return Err(format!(
                                "Invalid length slot {}",
                                len_sym
                            ));
                        }
                        let len_extra =
                            bits.read_many_bits(LENGTH_EXTRA[len_sym] as i32);
                        let length = LENGTH_BASE[len_sym] as usize
                            + len_extra as usize
                            + 5;

                        let pos_sym = decode_symbol(
                            &mut model6,
                            &mut bits,
                            &mut h,
                            &mut l,
                            &mut c,
                        )? as usize;
                        if pos_sym >= 42 {
                            return Err(format!(
                                "Invalid position slot {} in selector 6",
                                pos_sym
                            ));
                        }
                        let pos_extra =
                            bits.read_many_bits(EXTRA_BITS[pos_sym] as i32);
                        let offset =
                            (POSITION_BASE[pos_sym] + pos_extra + 1) as usize;
                        (offset, length)
                    }
                    _ => {
                        return Err(format!(
                            "Invalid selector {} from model7",
                            selector
                        ));
                    }
                };

                let mut src = (window_posn + window_size - match_offset)
                    & (window_size - 1);
                let bytes_to_copy =
                    match_length.min(file_end - output.len());

                for _ in 0..bytes_to_copy {
                    let byte = window[src];
                    window[window_posn] = byte;
                    output.push(byte);
                    src = (src + 1) & (window_size - 1);
                    window_posn = (window_posn + 1) & (window_size - 1);
                }
            }
        }

        // Between files: consume the 16-bit checksum from the raw bit stream.
        // The coder state (H, L, C) and models are preserved across files.
        if file_idx < file_sizes.len() - 1 {
            let _checksum = bits.read_bits(16);
        }
    }

    Ok(output)
}

// ============================================================================
// Archive parsing
// ============================================================================

/// Read a variable-length string prefix.
/// If length < 128, stored as one byte.
/// If >= 128, high bit set and remaining 15 bits contain the length (big-endian).
fn read_var_length(data: &[u8], pos: &mut usize) -> Result<usize, String> {
    if *pos >= data.len() {
        return Err(
            "Unexpected end of archive reading string length".to_string(),
        );
    }
    let first = data[*pos];
    *pos += 1;
    if first < 128 {
        Ok(first as usize)
    } else {
        if *pos >= data.len() {
            return Err(
                "Unexpected end of archive reading string length".to_string(),
            );
        }
        let second = data[*pos];
        *pos += 1;
        let len = (((first & 0x7F) as usize) << 8) | (second as usize);
        Ok(len)
    }
}

/// Read a variable-length string from the archive
fn read_var_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = read_var_length(data, pos)?;
    if *pos + len > data.len() {
        return Err(format!(
            "String length {} exceeds available data at offset {}",
            len, *pos
        ));
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).to_string();
    *pos += len;
    Ok(s)
}

/// Read a little-endian u16 from the data
fn read_u16_le(data: &[u8], pos: &mut usize) -> Result<u16, String> {
    if *pos + 2 > data.len() {
        return Err("Unexpected end of archive reading u16".to_string());
    }
    let val = (data[*pos] as u16) | ((data[*pos + 1] as u16) << 8);
    *pos += 2;
    Ok(val)
}

/// Read a little-endian u32 from the data
fn read_u32_le(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    if *pos + 4 > data.len() {
        return Err("Unexpected end of archive reading u32".to_string());
    }
    let val = (data[*pos] as u32)
        | ((data[*pos + 1] as u32) << 8)
        | ((data[*pos + 2] as u32) << 16)
        | ((data[*pos + 3] as u32) << 24);
    *pos += 4;
    Ok(val)
}

/// Parse a complete Quantum archive from raw data.
/// Returns (header, file_entries, offset_to_compressed_data).
fn parse_archive(
    data: &[u8],
) -> Result<(QArchiveHeader, Vec<QFileEntry>, usize), String> {
    if data.len() < 8 {
        return Err("File is too small to be a Quantum archive".to_string());
    }

    // Verify signature "DS" (0x44 0x53)
    if data[0] != QTM_SIGNATURE[0] || data[1] != QTM_SIGNATURE[1] {
        return Err(format!(
            "Invalid signature: expected 0x{:02X}{:02X} ('DS'), got 0x{:02X}{:02X}",
            QTM_SIGNATURE[0], QTM_SIGNATURE[1], data[0], data[1]
        ));
    }

    let mut pos = 2usize;
    let major_version = data[pos];
    pos += 1;
    let minor_version = data[pos];
    pos += 1;
    let num_files = read_u16_le(data, &mut pos)?;
    let table_size = data[pos];
    pos += 1;
    let comp_flags = data[pos];
    pos += 1;

    let header = QArchiveHeader {
        major_version,
        minor_version,
        num_files,
        table_size,
        comp_flags,
    };

    // Validate table size (window = 2^table_size bytes)
    if header.table_size < 10 || header.table_size > 21 {
        return Err(format!(
            "Invalid table size: {}. Must be between 10 and 21.",
            header.table_size
        ));
    }

    // Parse file entries
    let mut files = Vec::with_capacity(num_files as usize);
    for file_idx in 0..num_files {
        let name = read_var_string(data, &mut pos).map_err(|e| {
            format!("Error reading filename for file {}: {}", file_idx, e)
        })?;
        let comment = read_var_string(data, &mut pos).map_err(|e| {
            format!("Error reading comment for file {}: {}", file_idx, e)
        })?;
        let size = read_u32_le(data, &mut pos)?;
        let time = read_u16_le(data, &mut pos)?;
        let date = read_u16_le(data, &mut pos)?;

        files.push(QFileEntry {
            name,
            comment,
            size,
            time,
            date,
        });
    }

    Ok((header, files, pos))
}

// ============================================================================
// CLI and main logic
// ============================================================================

fn print_usage() {
    eprintln!(
        r#"UnQuantum v0.1.0 - Quantum archive decompressor (.Q)
A modern reimplementation for Linux, macOS, and Windows.

Based on Quantum v0.97 by David Stafford / Cinematronics (1993-1995).
Algorithm: LZ77 + arithmetic coding with adaptive frequency models.

USAGE:
    unquantum [OPTIONS] <archive.q>

OPTIONS:
    -x, --extract     Extract files (default action)
    -l, --list        List archive contents
    -t, --test        Test archive integrity
    -i, --info        Show detailed archive information
    -d, --dirs        Restore directory structure from paths
    -o, --output DIR  Output directory for extracted files
    -v, --verbose     Verbose output during extraction
    -h, --help        Show this help message

EXAMPLES:
    unquantum archive.q              Extract all files to current directory
    unquantum -l archive.q           List contents of archive
    unquantum -i archive.q           Show archive details
    unquantum -x -d -o out archive.q Extract with directories to 'out/'
    unquantum -t archive.q           Test archive integrity

Author: David Carrero Fernandez-Baillo (https://carrero.es)
License: MIT | https://github.com/dcarrero/unquantum"#
    );
}

#[derive(PartialEq)]
enum Action {
    Extract,
    List,
    Test,
    Info,
}

struct Config {
    action: Action,
    archive_path: String,
    output_dir: Option<String>,
    restore_dirs: bool,
    verbose: bool,
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let mut action = Action::Extract;
    let mut archive_path = None;
    let mut output_dir = None;
    let mut restore_dirs = false;
    let mut verbose = false;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                process::exit(0);
            }
            "-x" | "--extract" => action = Action::Extract,
            "-l" | "--list" => action = Action::List,
            "-t" | "--test" => action = Action::Test,
            "-i" | "--info" => action = Action::Info,
            "-d" | "--dirs" => restore_dirs = true,
            "-v" | "--verbose" => verbose = true,
            "-o" | "--output" => {
                i += 1;
                if i >= args.len() {
                    return Err("-o/--output requires an argument".to_string());
                }
                output_dir = Some(args[i].clone());
            }
            arg if arg.starts_with('-') => {
                return Err(format!("Unknown option: {}", arg));
            }
            _ => {
                if archive_path.is_none() {
                    archive_path = Some(args[i].clone());
                } else {
                    return Err(format!("Unexpected argument: {}", args[i]));
                }
            }
        }
        i += 1;
    }

    let archive_path = archive_path.ok_or("No archive file specified")?;

    Ok(Config {
        action,
        archive_path,
        output_dir,
        restore_dirs,
        verbose,
    })
}

/// Display the archive file listing
fn do_list(header: &QArchiveHeader, files: &[QFileEntry]) {
    println!(
        "Quantum {}.{:02} archive - {} file(s)",
        header.major_version, header.minor_version, header.num_files
    );
    println!();
    println!(
        " {:>10}  {:>10}  {:>8}  {:<24}  {}",
        "Size", "Date", "Time", "Name", "Comment"
    );
    println!(
        " {:>10}  {:>10}  {:>8}  {:<24}  {}",
        "----------", "----------", "--------", "------------------------", "-------"
    );

    let mut total_size: u64 = 0;
    for f in files {
        println!(
            " {:>10}  {:>10}  {:>8}  {:<24}  {}",
            f.size,
            f.date_string(),
            f.time_string(),
            f.name,
            f.comment
        );
        total_size += f.size as u64;
    }

    println!(
        " {:>10}  {:>10}  {:>8}  {} file(s)",
        total_size, "", "", files.len()
    );
}

/// Display detailed archive information
fn do_info(
    header: &QArchiveHeader,
    files: &[QFileEntry],
    archive_size: usize,
) {
    let total_original: u64 = files.iter().map(|f| f.size as u64).sum();
    let window_size = 1u64 << header.table_size;
    let window_kb = window_size / 1024;

    println!("=== Quantum Archive Information ===");
    println!();
    println!(
        "Version:           {}.{:02}",
        header.major_version, header.minor_version
    );
    println!("Number of files:   {}", header.num_files);
    println!(
        "Table size:        {} (window = {} KB = {} bytes)",
        header.table_size, window_kb, window_size
    );
    println!("Compression flags: 0x{:02X}", header.comp_flags);
    println!("Archive size:      {} bytes", archive_size);
    println!("Original size:     {} bytes", total_original);
    if total_original > 0 {
        let ratio = (archive_size as f64 / total_original as f64) * 100.0;
        println!("Compression ratio: {:.1}%", ratio);
    }
    println!();
    println!("--- Files ---");
    for (idx, f) in files.iter().enumerate() {
        let comment_str = if f.comment.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", f.comment)
        };
        println!(
            "  [{}] {} ({} bytes) {} {}{}",
            idx,
            f.name,
            f.size,
            f.date_string(),
            f.time_string(),
            comment_str
        );
    }
}

/// Extract or test the archive
fn do_extract_or_test(
    header: &QArchiveHeader,
    files: &[QFileEntry],
    compressed_data: Vec<u8>,
    config: &Config,
) -> Result<(), String> {
    let total_output_size: usize = files.iter().map(|f| f.size as usize).sum();

    if config.verbose || config.action == Action::Test {
        println!(
            "Quantum {}.{:02} archive - {} file(s), table size {}",
            header.major_version,
            header.minor_version,
            header.num_files,
            header.table_size
        );
        println!("Total decompressed size: {} bytes", total_output_size);
        println!("Compressed data size:    {} bytes", compressed_data.len());
        println!();
    }

    if total_output_size == 0 {
        println!("Archive contains no data to extract.");
        return Ok(());
    }

    // Decompress the entire data stream
    if config.verbose {
        println!("Decompressing...");
    }
    let file_sizes: Vec<u32> = files.iter().map(|f| f.size).collect();
    let decompressed =
        quantum_decompress(compressed_data, &file_sizes, header.table_size)?;

    if decompressed.len() != total_output_size {
        return Err(format!(
            "Decompression size mismatch: expected {} bytes, got {}",
            total_output_size,
            decompressed.len()
        ));
    }

    if config.action == Action::Test {
        println!(
            "Archive integrity test PASSED ({} bytes decompressed successfully).",
            total_output_size
        );
        return Ok(());
    }

    // Split decompressed data into individual files and write them
    let base_dir = config
        .output_dir
        .as_ref()
        .map(|d| PathBuf::from(d))
        .unwrap_or_else(|| PathBuf::from("."));

    let mut offset: usize = 0;
    for f in files {
        let end = offset + f.size as usize;
        let file_data = &decompressed[offset..end];
        offset = end;

        // Convert DOS path separators to native
        let native_name = f.name.replace('\\', "/");
        let file_path = if config.restore_dirs {
            base_dir.join(&native_name)
        } else {
            // Strip directory components, keep only filename
            let filename = Path::new(&native_name)
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(&native_name));
            base_dir.join(filename)
        };

        // Create parent directories if needed
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create directory {}: {}",
                        parent.display(),
                        e
                    )
                })?;
            }
        }

        // Write the file
        let mut out_file = File::create(&file_path).map_err(|e| {
            format!("Failed to create file {}: {}", file_path.display(), e)
        })?;
        out_file.write_all(file_data).map_err(|e| {
            format!("Failed to write file {}: {}", file_path.display(), e)
        })?;

        if config.verbose {
            println!("  {} ({} bytes)", file_path.display(), f.size);
        } else {
            println!("  {}", f.name);
        }
    }

    println!(
        "\nExtracted {} file(s), {} bytes total.",
        files.len(),
        total_output_size
    );

    Ok(())
}

fn main() {
    let config = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            eprintln!("Use -h for help.");
            process::exit(1);
        }
    };

    // Read the entire archive into memory
    let archive_data = match fs::read(&config.archive_path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!(
                "Error: Cannot read '{}': {}",
                config.archive_path, e
            );
            process::exit(1);
        }
    };

    let archive_size = archive_data.len();

    // Parse the archive header and file entries
    let (header, files, data_offset) = match parse_archive(&archive_data) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    };

    match config.action {
        Action::List => {
            do_list(&header, &files);
        }
        Action::Info => {
            do_info(&header, &files, archive_size);
        }
        Action::Extract | Action::Test => {
            let compressed_data = archive_data[data_offset..].to_vec();

            if let Err(e) =
                do_extract_or_test(&header, &files, compressed_data, &config)
            {
                eprintln!("Error: {}", e);
                process::exit(1);
            }
        }
    }
}
