# UnQuantum

A modern decompressor for the **Quantum archive format (.Q)**, originally created by David Stafford of Cinematronics (Austin, TX, 1993-1995).

More context about this format and its recovery: [Rescatando pasado digital: formato de compresion Q (Quantum) MS-DOS](https://carrero.es/rescatando-pasado-digital-formato-de-compresion-q-quantum-ms-dos/)

Written in Rust for cross-platform support on **Linux**, **macOS** and **Windows**.

## About the Quantum format

Quantum was a state-of-the-art data compressor for MS-DOS that used **LZ77 combined with arithmetic coding** (not Huffman, like most compressors of the era). It was licensed by Microsoft, Borland, and Novell, and notably integrated into Microsoft's Cabinet (.CAB) format.

The standalone `.Q` archive format uses:
- Magic signature: `0x44 0x53` ("DS")
- Configurable sliding window: 1 KB to 2 MB (`-t10` to `-t21`)
- 7 adaptive frequency models with arithmetic coding
- All files compressed as a single continuous stream with 16-bit per-file checksums

The original tools (`PAQ.EXE`, `UNPAQ.EXE`) were 32-bit DOS executables that required a Borland DPMI extender. This project provides a native reimplementation that runs directly on modern operating systems.

## Downloading the original Quantum tools

Quantum v0.97 (the last known version) can be downloaded from:

- [carrero.es](https://carrero.es/rescatando-pasado-digital-formato-de-compresion-q-quantum-ms-dos/) - Article with direct download links and more information about the format
- [Simtel MSDOS CD (archive.org)](https://archive.org/download/Simtel_MSDOS_1996-09/) - Historical Simtel MSDOS software collection from September 1996, search for `quantum` in the `arcers/` directory

## Building

### Prerequisites

You need the [Rust toolchain](https://rustup.rs/) installed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Linux

```bash
git clone https://github.com/dcarrero/unquantum.git
cd unquantum
cargo build --release
```

The binary will be at `target/release/unquantum`.

### macOS

```bash
git clone https://github.com/dcarrero/unquantum.git
cd unquantum
cargo build --release
```

The binary will be at `target/release/unquantum`.

If you use Homebrew, you can also install Rust via `brew install rust`.

### Windows

1. Install Rust from [rustup.rs](https://rustup.rs/) (download and run `rustup-init.exe`)
2. Open a terminal (PowerShell or cmd):

```powershell
git clone https://github.com/dcarrero/unquantum.git
cd unquantum
cargo build --release
```

The binary will be at `target\release\unquantum.exe`.

### Cross-compilation

To build for a different platform from your current one:

```bash
# Add target (example: Linux from macOS)
rustup target add x86_64-unknown-linux-gnu

# Build for that target
cargo build --release --target x86_64-unknown-linux-gnu
```

Common targets:
- `x86_64-unknown-linux-gnu` - Linux x86_64
- `x86_64-apple-darwin` - macOS Intel
- `aarch64-apple-darwin` - macOS Apple Silicon
- `x86_64-pc-windows-msvc` - Windows x86_64

## Usage

```
unquantum [OPTIONS] <archive.q>
```

### Options

| Flag | Description |
|------|-------------|
| `-x, --extract` | Extract files (default action) |
| `-l, --list` | List archive contents |
| `-t, --test` | Test archive integrity |
| `-i, --info` | Show detailed archive information |
| `-d, --dirs` | Restore directory structure from paths |
| `-o, --output DIR` | Output directory for extracted files |
| `-v, --verbose` | Verbose output during extraction |
| `-h, --help` | Show help message |

### Examples

```bash
# List contents of a .Q archive
unquantum -l archive.q

# Extract all files to the current directory
unquantum archive.q

# Extract preserving directory structure into a folder
unquantum -x -d -o output_dir archive.q

# Show detailed archive information
unquantum -i archive.q

# Test archive integrity without extracting
unquantum -t archive.q
```

## Technical details

This implementation is based on:

- **QUANTUM.DOC** - Official archive format specification by Cinematronics
- **libmspack** by Stuart Caie (LGPL 2.1) - Quantum decompressor for CAB files
- **Matthew Russotto's research** on the Quantum compressed data format
- **Reverse engineering** of the original `UNPAQ.EXE` and `PAQ.EXE` v0.97 binaries

### Algorithm

The Quantum compressor uses:

1. **LZ77** sliding window compression with configurable window sizes (2^10 to 2^21 bytes)
2. **Arithmetic coding** with 9 adaptive frequency models:
   - 4 literal models (each covering 64 byte values: 0-63, 64-127, 128-191, 192-255)
   - 3 match position models (for 3-byte, 4-byte, and variable-length matches)
   - 1 match length model (27 length slots for variable-length matches)
   - 1 selector model (chooses between literal types and match types)
3. **42 position slots** with 0-19 extra bits for encoding match offsets
4. **27 length slots** with 0-5 extra bits for encoding variable match lengths

### Archive format

```
Offset  Size  Description
------  ----  -----------
0       2     Signature: 0x44 0x53 ("DS")
2       1     Major version
3       1     Minor version
4       2     Number of files (little-endian)
6       1     Table size (10-21, window = 2^N bytes)
7       1     Compression flags

Per file entry:
  var   var   Filename length + filename
  var   var   Comment length + comment
  +0    4     Expanded file size (little-endian)
  +4    2     File time (DOS format)
  +6    2     File date (DOS format)

Compressed data follows immediately after file entries.
Between each file in the compressed stream, a 16-bit checksum is
embedded in the raw bit stream (not through the arithmetic decoder).
The coder state and all adaptive models persist across file boundaries.
```

## Testing

The `tests/` directory contains sample `.Q` archives created with the original PAQ.EXE v0.97:

- `test_single.q` - Single-file archive (1 file, 56 bytes)
- `test_multi.q` - Multi-file archive (3 files, 544 bytes total)

```bash
# Test single-file archive
unquantum -t tests/test_single.q

# Extract multi-file archive
unquantum -l tests/test_multi.q
unquantum tests/test_multi.q
```

## License

MIT

## Credits

- **David Stafford / Cinematronics** - Original Quantum compression algorithm (1993-1995)
- **Matthew Russotto** - Reverse engineering of the Quantum compressed data format
- **Stuart Caie** - libmspack Quantum decompressor implementation
- **David Carrero Fernandez-Baillo** - This Rust reimplementation and digital preservation effort
